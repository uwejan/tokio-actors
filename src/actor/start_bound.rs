//! The start bound of a spawn, owned by the system.
//!
//! A spawn with a `start_timeout` commits the system to a step: if the actor
//! has not acknowledged its initialization by the deadline, the system cuts
//! the start and waits for its teardown. That step runs in a helper task of
//! the caller's runtime, which is the runtime the actor runs on, spawned at
//! the moment the spawn commits, so the bound holds whether or not the caller
//! keeps waiting: a caller that drops its future, or stores it and never
//! polls it again, leaves the bound in force.
//!
//! The cut raises `Kill` on the actor's stop lane and then requests the abort
//! of its task. The abort takes effect when the task next suspends. The
//! `Kill` is read before each start hook and at each turn boundary, so once
//! the start has timed out no hook begins and no message is handled; a hook
//! that is already running and never yields finishes first.
//!
//! The caller only waits for the helper's answer and has no bound of its own:
//! the caller, the helper and the actor share one runtime, so the helper
//! answers, or its answer channel closes. One gate decides the outcome once,
//! so the caller and the actor never disagree:
//!
//! - the helper takes `Acked` when it reads a good ack, and then answers
//!   `Started`;
//! - the helper takes `TimedOut` at the deadline, cuts the start and answers
//!   `TimedOut`;
//! - a helper that panics before it answers takes `TimedOut` through its
//!   drop guard, unless the gate already stands at `Acked`, and cuts the
//!   start before the answer channel closes;
//! - a closed answer is read by the caller against the gate and the actor's
//!   status plane; the caller only classifies and does no cleanup.
//!
//! A helper whose runtime ends before it answers takes no action: the actor
//! ends with that runtime.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::{timeout, timeout_at, Instant};

use crate::actor::handle::ActorHandle;
use crate::actor::panic::{catch_callback, contain_waker_panic, send_contained};
use crate::actor::runtime::saturating_deadline;
use crate::actor::supervision::ABORT_SETTLE;
use crate::actor::{closed_ack_error, Actor};
use crate::error::{ActorError, SpawnError};
use crate::helper::{HelperKind, HelperOwner};
use crate::system::{raise_noted, wait_final_reason, ActorSystem};
use crate::types::{ActorId, StopLane, StopReason};

/// The state of the gate before anyone decided.
const ARMED: u8 = 0;
/// The helper read a good ack first: the actor runs and the caller gets its
/// handle.
const ACKED: u8 = 1;
/// The deadline passed, or the helper was lost, first: the start is cut and
/// the caller gets `StartTimeout`.
const TIMED_OUT: u8 = 2;

/// Decides the outcome of one timed spawn, once.
#[derive(Debug)]
struct Gate(AtomicU8);

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicU8::new(ARMED)))
    }

    /// Takes `want` if nobody decided yet; returns the state that stands.
    ///
    /// The gate guards no other memory: the actor, the abort and the answer
    /// travel through their own synchronized channels. `AcqRel` on the
    /// winning exchange and `Acquire` on the losing load make the decision a
    /// single total order that every later reader of the gate observes, which
    /// is all the loser needs to conform.
    fn decide(&self, want: u8) -> u8 {
        match self
            .0
            .compare_exchange(ARMED, want, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => want,
            Err(standing) => standing,
        }
    }

    /// The state that stands.
    fn state(&self) -> u8 {
        self.0.load(Ordering::Acquire)
    }
}

/// What the helper tells the caller.
#[derive(Debug)]
enum Outcome {
    /// The actor acknowledged its initialization before the deadline.
    Started,
    /// The initialization failed.
    Init(ActorError),
    /// The ack sender dropped without a reply.
    ClosedAck,
    /// The start timed out and was cut.
    TimedOut,
    /// A test ended the helper without a decision or an answer.
    #[cfg(test)]
    Lost,
}

/// The step the system owns, run by the helper.
struct Bound {
    gate: Arc<Gate>,
    ack: oneshot::Receiver<Result<(), ActorError>>,
    join: JoinHandle<StopReason>,
    abort: AbortHandle,
    lane: StopLane,
    deadline: Instant,
    /// The deadline is the call instant itself: no ack can arrive before it.
    immediate: bool,
    #[cfg(test)]
    id: ActorId,
}

impl Bound {
    async fn run(self) -> Outcome {
        let Bound {
            gate,
            ack,
            join,
            abort,
            lane,
            deadline,
            immediate,
            #[cfg(test)]
            id,
        } = self;
        // `timeout_at` polls the ack first, so an ack that is ready wins over
        // a deadline that has passed, whenever this helper runs. A zero
        // timeout leaves no time for any ack: the deadline is the call
        // instant itself, and the start is cut.
        let acked = if immediate {
            None
        } else {
            timeout_at(deadline, ack).await.ok()
        };
        match acked {
            Some(Ok(Ok(()))) => {
                #[cfg(test)]
                if hooks::reach(hooks::BEFORE_DECISION, &id).await == hooks::Reach::Lost {
                    return Outcome::Lost;
                }
                if gate.decide(ACKED) == ACKED {
                    #[cfg(test)]
                    if hooks::reach(hooks::AFTER_ACKED, &id).await == hooks::Reach::Lost {
                        return Outcome::Lost;
                    }
                    Outcome::Started
                } else {
                    // The start was already cut: the actor must not keep
                    // running.
                    cut(&lane, &abort, join).await;
                    Outcome::TimedOut
                }
            }
            Some(Ok(Err(err))) => Outcome::Init(err),
            Some(Err(_closed)) => Outcome::ClosedAck,
            // No ack by the deadline, or a zero timeout.
            None => {
                gate.decide(TIMED_OUT);
                cut(&lane, &abort, join).await;
                Outcome::TimedOut
            }
        }
    }
}

/// Cuts the start of the task at once, as Erlang/OTP kills a process whose
/// start times out: raises `Kill` on its stop lane, then requests its abort.
///
/// The abort takes effect when the task next suspends, and the task is dropped
/// there, mid-poll. The `Kill` is read before each start hook and at each turn
/// boundary, so once the start has timed out no hook begins and no message is
/// handled; a hook that is already running and never yields is beyond the
/// abort and finishes first, and the actor then ends as `Kill`.
///
/// The raise wakes the tasks that wait for the actor's end, and a panic of
/// one of their wakers is contained in the raise (see
/// [`StopLane::try_raise`]), so the raise always returns and the abort always
/// follows. That holds from the guard's drop too, which runs while the
/// helper's own panic unwinds, where a second panic would end the process.
pub(super) fn cut_start(lane: &StopLane, abort: &AbortHandle) {
    // The bound holds the task's abort handle and requests the abort below
    // itself, so no registry entry is involved and no deadline is declared.
    raise_noted(
        None,
        lane,
        StopReason::Kill,
        crate::telemetry::StopOrigin::Requested,
    );
    lane.abort_record().abort(abort);
}

/// Cuts the start, then waits, bounded, for the abort to finish the task's
/// teardown (its registry guard drop included), so a retry under the same name
/// does not race a predecessor whose teardown ends within that short wait.
async fn cut(lane: &StopLane, abort: &AbortHandle, join: JoinHandle<StopReason>) {
    cut_start(lane, abort);
    let _ = timeout(ABORT_SETTLE, join).await;
}

/// Holds the answer channel of one helper and takes the fail-closed action
/// when the helper panics before it answers.
///
/// The guard is built before the helper's future exists, so a helper that is
/// dropped before its first poll is covered as well. On a panic it takes the
/// gate with a single exchange and cuts the start only if the gate stands at
/// `TimedOut`; a gate that stands at `Acked` leaves the actor running. The
/// answer channel closes only after that action: the guard's own `drop` takes
/// the sender out and drops it as its last step, inside a catch, so a caller
/// that sees a closed answer sees a decided gate. A helper that ends any other
/// way (its runtime ended) takes no action: the actor ends with that runtime.
struct Guard {
    gate: Arc<Gate>,
    lane: StopLane,
    abort: AbortHandle,
    /// Set when the helper's step panicked.
    panicked: bool,
    /// Set once the answer was sent: a panic after that takes no action.
    completed: bool,
    #[cfg(test)]
    hold: Option<Arc<hooks::StdHold>>,
    #[cfg(test)]
    probe: Option<Arc<hooks::Probe>>,
    /// Dropped last, by the guard's own `drop`, after its action.
    answer: Option<oneshot::Sender<Outcome>>,
}

impl Guard {
    fn new(
        gate: Arc<Gate>,
        lane: StopLane,
        abort: AbortHandle,
        answer: oneshot::Sender<Outcome>,
    ) -> Self {
        Self {
            gate,
            lane,
            abort,
            panicked: false,
            completed: false,
            #[cfg(test)]
            hold: None,
            #[cfg(test)]
            probe: None,
            answer: Some(answer),
        }
    }

    /// Runs the helper's step; a panic of it is contained here, marked for
    /// the guard's drop, and raised again for the slot to record.
    async fn guarded(mut self, step: impl std::future::Future<Output = Outcome>) {
        match catch_callback(step).await {
            Ok(outcome) => {
                #[cfg(test)]
                if matches!(outcome, Outcome::Lost) {
                    return;
                }
                self.complete(outcome);
            }
            Err(payload) => {
                self.panicked = true;
                std::panic::resume_unwind(payload)
            }
        }
    }

    /// Answers the caller.
    fn complete(&mut self, outcome: Outcome) {
        if let Some(answer) = self.answer.take() {
            // A caller that stopped waiting needs no answer.
            let _ = send_contained(answer, outcome);
        }
        self.completed = true;
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.panicked && !self.completed {
            #[cfg(test)]
            if let Some(hold) = &self.hold {
                hold.reach();
            }
            if self.gate.decide(TIMED_OUT) == TIMED_OUT {
                cut_start(&self.lane, &self.abort);
            }
        }
        #[cfg(test)]
        if let Some(probe) = &self.probe {
            probe.guards_dropped.fetch_add(1, Ordering::AcqRel);
        }
        // An answer still held is dropped here, last: its drop wakes the
        // caller of the timed spawn, whose waker belongs to the caller's task,
        // and this drop can run while the helper unwinds from a panic of its
        // step. A panic of that waker stays out of the drop, so it adds no
        // second panic to the unwind.
        if let Some(answer) = self.answer.take() {
            contain_waker_panic("start answer", None, move || drop(answer));
        }
    }
}

/// Bounds the start of the actor `id` by `start_timeout`, with the step run by
/// a helper task of `system` on the current runtime, and waits for its answer.
///
/// `handle`, `join` and `ack` are those of the actor just spawned by the
/// caller on the current runtime.
pub(super) async fn bounded_start<A: Actor>(
    system: &ActorSystem,
    id: ActorId,
    handle: ActorHandle<A>,
    join: JoinHandle<StopReason>,
    ack: oneshot::Receiver<Result<(), ActorError>>,
    start_timeout: Duration,
) -> Result<ActorHandle<A>, SpawnError> {
    // Both ends of the step are fixed at the call, not at the helper's first
    // poll; the second is the first plus the time the abort is given.
    let call = Instant::now();
    let deadline = saturating_deadline(call, start_timeout);
    let settled = deadline.checked_add(ABORT_SETTLE).unwrap_or(deadline);

    let gate = Gate::new();
    let (answer, answered) = oneshot::channel();
    let lane = handle.stop_lane();
    let abort = join.abort_handle();
    let bound = Bound {
        gate: gate.clone(),
        ack,
        join,
        abort: abort.clone(),
        lane: lane.clone(),
        deadline,
        immediate: deadline <= call,
        #[cfg(test)]
        id: id.clone(),
    };
    let rt = Handle::current();
    let owner = HelperOwner::new(id.clone(), Some(lane.abort_record().clone()));
    drop(
        system.spawn_helper(&rt, HelperKind::StartBound, Some(owner), {
            let gate = gate.clone();
            move |end| {
                // Built here, outside the helper's future, so a helper that
                // is dropped before its first poll drops it as well.
                let guard = Guard::new(gate, lane, abort, answer);
                #[cfg(test)]
                let guard = hooks::attach(guard, &id);
                end.observe(async move { guard.guarded(bound.run()).await })
            }
        }),
    );

    match answered.await {
        Ok(Outcome::Started) => Ok(handle),
        Ok(Outcome::Init(err)) => Err(SpawnError::Init(Box::new(err))),
        // Same closed-ack rule as the spawn without a deadline.
        Ok(Outcome::ClosedAck) => Err(closed_ack_error(&handle).await),
        Ok(Outcome::TimedOut) => Err(SpawnError::StartTimeout),
        #[cfg(test)]
        Ok(Outcome::Lost) => unreachable!("a lost helper sends no answer"),
        Err(_closed) => classify_closed(handle, &gate, settled).await,
    }
}

/// Reads a closed answer against the gate and the actor's status plane. The
/// caller does no cleanup: the guard of a helper that panicked has already
/// cut the start unless the gate stood at `Acked`.
///
/// - `Acked`: the actor runs and the caller gets its handle.
/// - `TimedOut`: the start was cut; the caller is told so once the actor's
///   reason is final, which is after its name is free.
/// - undecided: the helper ended without a decision. The caller gives the
///   status plane until `settled` to reach a final reason, which maps as the
///   spawn without a deadline maps it, and reports `StartTimeout` when that
///   bound passes.
async fn classify_closed<A: Actor>(
    handle: ActorHandle<A>,
    gate: &Gate,
    settled: Instant,
) -> Result<ActorHandle<A>, SpawnError> {
    match gate.state() {
        ACKED => Ok(handle),
        TIMED_OUT => {
            let mut status_rx = handle.status_rx();
            let _ = timeout_at(settled, wait_final_reason(&mut status_rx)).await;
            Err(SpawnError::StartTimeout)
        }
        _ => match timeout_at(settled, closed_ack_error(&handle)).await {
            Ok(err) => Err(err),
            Err(_elapsed) => Err(SpawnError::StartTimeout),
        },
    }
}

/// Test hooks that hold or end the helper at a named point, so a test orders
/// the race of the gate by observation.
#[cfg(test)]
mod hooks {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Condvar, Mutex, OnceLock};

    use tokio::sync::Notify;

    use super::{Gate, Guard};
    use crate::types::ActorId;

    /// After a good ack, before the helper takes the gate.
    pub(super) const BEFORE_DECISION: &str = "before the decision";
    /// After the helper took `Acked`, before it answers.
    pub(super) const AFTER_ACKED: &str = "after the ack was taken";

    /// How a held helper goes on.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Reach {
        /// The helper goes on with its step.
        Go,
        /// The helper ends without deciding or answering.
        Lost,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Release {
        Go,
        Lose,
        Panic,
    }

    /// A helper held at a point.
    #[derive(Debug)]
    pub(super) struct Hold {
        arrived: Notify,
        release: Notify,
        how: Mutex<Release>,
    }

    impl Hold {
        /// Waits until the helper is held here.
        pub(super) async fn arrived(&self) {
            self.arrived.notified().await;
        }

        /// Lets the helper go on.
        pub(super) fn release(&self) {
            self.release.notify_one();
        }

        /// Ends the helper at this point, without a decision or an answer.
        pub(super) fn lose(&self) {
            *self.how.lock().unwrap() = Release::Lose;
            self.release.notify_one();
        }

        /// Makes the helper panic at this point.
        pub(super) fn fail(&self) {
            *self.how.lock().unwrap() = Release::Panic;
            self.release.notify_one();
        }
    }

    type Holds = Mutex<HashMap<(&'static str, ActorId), Arc<Hold>>>;

    fn holds() -> &'static Holds {
        static HOLDS: OnceLock<Holds> = OnceLock::new();
        HOLDS.get_or_init(Default::default)
    }

    /// Holds the helper of the actor `id` once, at `point`.
    pub(super) fn hold(point: &'static str, id: ActorId) -> Arc<Hold> {
        let hold = Arc::new(Hold {
            arrived: Notify::new(),
            release: Notify::new(),
            how: Mutex::new(Release::Go),
        });
        holds().lock().unwrap().insert((point, id), hold.clone());
        hold
    }

    pub(super) async fn reach(point: &'static str, id: &ActorId) -> Reach {
        let hold = holds().lock().unwrap().remove(&(point, id.clone()));
        let Some(hold) = hold else {
            return Reach::Go;
        };
        hold.arrived.notify_one();
        hold.release.notified().await;
        let how = std::mem::replace(&mut *hold.how.lock().unwrap(), Release::Go);
        match how {
            Release::Go => Reach::Go,
            Release::Lose => Reach::Lost,
            Release::Panic => panic!("the helper is made to panic at {point}"),
        }
    }

    /// A thread held by a plain lock, for a point the guard reaches in its
    /// own `drop`, which cannot await. Every wait is bounded: a hold nobody
    /// reaches or releases ends the wait instead of hanging the test.
    #[derive(Debug, Default)]
    pub(super) struct StdHold {
        state: Mutex<(bool, bool)>,
        changed: Condvar,
    }

    const HOLD_BOUND: std::time::Duration = std::time::Duration::from_secs(30);

    impl StdHold {
        /// Blocks the calling thread until released.
        pub(super) fn reach(&self) {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            self.changed.notify_all();
            let _ = self
                .changed
                .wait_timeout_while(state, HOLD_BOUND, |state| !state.1)
                .unwrap();
        }

        /// Waits until a thread is held here.
        pub(super) fn wait_arrived(&self) {
            let state = self.state.lock().unwrap();
            let (_state, waited) = self
                .changed
                .wait_timeout_while(state, HOLD_BOUND, |state| !state.0)
                .unwrap();
            assert!(!waited.timed_out(), "nothing reached the hold");
        }

        /// Lets the held thread go on.
        pub(super) fn release(&self) {
            self.state.lock().unwrap().1 = true;
            self.changed.notify_all();
        }
    }

    /// What a test reads of the helper of one actor.
    #[derive(Debug, Default)]
    pub(super) struct Probe {
        /// The guards of this actor's helpers that were dropped.
        pub(super) guards_dropped: AtomicUsize,
        gate: Mutex<Option<Arc<Gate>>>,
    }

    impl Probe {
        /// The gate of the actor's timed spawn.
        pub(super) fn gate(&self) -> Arc<Gate> {
            self.gate
                .lock()
                .unwrap()
                .clone()
                .expect("the guard was built")
        }
    }

    type Probes = Mutex<HashMap<ActorId, Arc<Probe>>>;

    fn probes() -> &'static Probes {
        static PROBES: OnceLock<Probes> = OnceLock::new();
        PROBES.get_or_init(Default::default)
    }

    /// Watches the helper of the actor `id`.
    pub(super) fn probe(id: ActorId) -> Arc<Probe> {
        let probe = Arc::new(Probe::default());
        probes().lock().unwrap().insert(id, probe.clone());
        probe
    }

    /// Attaches the probe of `id`, if a test asked for one, to a new guard.
    pub(super) fn attach(mut guard: Guard, id: &ActorId) -> Guard {
        if let Some(probe) = probes().lock().unwrap().get(id).cloned() {
            *probe.gate.lock().unwrap() = Some(guard.gate.clone());
            guard.probe = Some(probe);
        }
        guard
    }
}

#[cfg(test)]
mod tests;
