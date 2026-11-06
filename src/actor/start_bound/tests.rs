//! The start bound under test: the race of the gate between the helper and
//! the deadline, the closed answer, and the fail-closed action of the
//! helper's guard. Each point is ordered by observation: the test holds the
//! helper (or the guard) at a named point, acts, and then lets it go on.
//!
//! In every case the caller and the actor agree: either the caller got the
//! handle and the actor runs, or the caller got `StartTimeout` and the actor
//! was aborted and its name freed.

use std::future::IntoFuture;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::task::JoinHandle;
use tokio::time::{advance, timeout, Instant};

use super::{hooks, Bound, Gate, Guard, Outcome, ACKED, ARMED, TIMED_OUT};
use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::runtime::FAR_FUTURE;
use crate::actor::supervision::ABORT_SETTLE;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorResult, SpawnError};
use crate::helper::{HelperEnd, HelperKind};
use crate::system::ActorSystem;
use crate::types::{ActorId, LaneRx, StopLane, StopReason};

const START_TIMEOUT: Duration = Duration::from_secs(1);
const BOUND: Duration = Duration::from_secs(30);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// An actor that starts at once and answers.
struct Live;

impl Actor for Live {
    type Message = ();
    type Response = u8;

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<u8> {
        Ok(7)
    }
}

/// An actor whose initialization takes `delay` of the runtime's clock.
struct SlowStart(Duration);

impl Actor for SlowStart {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        tokio::time::sleep(self.0).await;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn fresh_system() -> Arc<ActorSystem> {
    ActorSystem::create(format!("start-bound-{}", uuid::Uuid::new_v4())).unwrap()
}

type Spawned = JoinHandle<Result<ActorHandle<Live>, SpawnError>>;

/// Starts a `Live` named `name` under a start timeout, on its own task.
fn start(system: &Arc<ActorSystem>, name: &str) -> Spawned {
    let builder = Live
        .spawn()
        .named(name)
        .on_system(system)
        .start_timeout(START_TIMEOUT);
    tokio::spawn(async move { builder.await })
}

/// Yields until `condition` holds. The bound only limits how long a real hang takes to report.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let waited = timeout(HANG_BOUND, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(waited.is_ok(), "{what}");
}

/// Lets every task that can run, run.
async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

/// The caller has no bound of its own. The helper has taken `Acked` and not
/// yet answered; the deadline and the settle time pass; the caller still
/// waits, and when the helper goes on it gets the handle and the actor runs.
#[tokio::test(start_paused = true)]
async fn a_helper_that_decides_first_gets_the_caller_the_handle() {
    let system = fresh_system();
    let rt = tokio::runtime::Handle::current();
    let name = unique("helper-first");
    let hold = hooks::hold(hooks::AFTER_ACKED, ActorId::from(name.as_str()));
    let spawned = start(&system, &name);
    hold.arrived().await;

    advance(START_TIMEOUT + ABORT_SETTLE + BOUND).await;
    settle().await;
    assert!(!spawned.is_finished(), "the caller only waits");
    hold.release();
    let handle = match spawned.await.unwrap() {
        Ok(handle) => handle,
        Err(err) => panic!("the helper took the gate first, got {err}"),
    };

    assert_eq!(handle.send(()).await.unwrap(), 7);
    assert!(handle.is_alive());
    assert!(system.get::<Live>(&name).is_some());
    until("the helper is counted down", || system.live_helpers() == 0).await;
    assert_eq!(
        system.helper_ends(&rt),
        vec![(HelperKind::StartBound, HelperEnd::Finished)]
    );
    assert!(
        handle.is_alive(),
        "the deadline does not touch a started actor"
    );
}

/// A helper that ends without a decision or an answer, with its runtime
/// alive, leaves the caller a closed answer and an undecided gate. The caller
/// classifies from the status plane with a bound of the deadline plus the
/// settle time, and reports `StartTimeout` when it passes; it never waits
/// without bound.
#[tokio::test(start_paused = true)]
async fn a_closed_answer_with_no_decision_is_read_within_a_bound() {
    let system = fresh_system();
    let name = unique("helper-lost");
    let hold = hooks::hold(hooks::BEFORE_DECISION, ActorId::from(name.as_str()));
    let began = Instant::now();
    let spawned = start(&system, &name);
    hold.arrived().await;

    hold.lose();
    let answer = timeout(BOUND, spawned)
        .await
        .expect("the closed answer is read within a bound")
        .unwrap();
    match answer {
        Err(SpawnError::StartTimeout) => {}
        Err(other) => panic!("expected StartTimeout, got {other}"),
        Ok(_) => panic!("a lost helper decided nothing, the caller must not start"),
    }
    assert!(
        began.elapsed() >= START_TIMEOUT + ABORT_SETTLE,
        "the caller waited out the deadline and the settle time"
    );
}

/// The bound of a closed answer is the capped deadline plus the settle time,
/// each added with its own cap. For a start timeout of 30 years or more the
/// caller therefore waits out the settle time past the cap, and a start
/// timeout that does not fit in an `Instant` does not panic the call.
#[tokio::test(start_paused = true)]
async fn a_closed_answer_under_a_start_timeout_of_the_cap_reads_after_the_settle_time() {
    for start_timeout in [FAR_FUTURE, Duration::MAX] {
        let system = fresh_system();
        let name = unique("helper-lost-cap");
        let hold = hooks::hold(hooks::BEFORE_DECISION, ActorId::from(name.as_str()));
        let builder = Live
            .spawn()
            .named(name.as_str())
            .on_system(&system)
            .start_timeout(start_timeout);
        let spawned: Spawned = tokio::spawn(async move { builder.await });
        hold.arrived().await;
        hold.lose();
        settle().await;

        advance(FAR_FUTURE + ABORT_SETTLE / 2).await;
        settle().await;
        assert!(
            !spawned.is_finished(),
            "{start_timeout:?}: the closed answer is not read at the cap, the settle time follows it"
        );
        advance(ABORT_SETTLE).await;
        settle().await;
        let answer = timeout(BOUND, spawned)
            .await
            .expect("the closed answer is read once the settle time has passed")
            .expect("the caller does not panic");
        assert!(
            matches!(answer, Err(SpawnError::StartTimeout)),
            "{start_timeout:?}: a lost helper decided nothing"
        );
    }
}

/// A helper that panics before its decision aborts the start through its
/// guard before the answer channel closes: the caller is told `StartTimeout`
/// once the actor's reason is final, the name is free, and an immediate
/// retry under it is not refused.
#[tokio::test(start_paused = true)]
async fn a_helper_that_panics_before_its_decision_fails_closed() {
    let system = fresh_system();
    let rt = tokio::runtime::Handle::current();
    let name = unique("panic-before");
    let hold = hooks::hold(hooks::BEFORE_DECISION, ActorId::from(name.as_str()));
    let spawned = start(&system, &name);
    hold.arrived().await;
    let actor = system.get::<Live>(&name).expect("the actor is registered");

    hold.fail();
    match timeout(BOUND, spawned).await.unwrap().unwrap() {
        Err(SpawnError::StartTimeout) => {}
        Err(other) => panic!("expected StartTimeout, got {other}"),
        Ok(_) => panic!("the helper never took the gate for the ack"),
    }
    assert_eq!(
        timeout(BOUND, actor.wait_stopped()).await.unwrap(),
        StopReason::Kill
    );
    assert_eq!(
        actor.stop_lane().reason(),
        Some(StopReason::Kill),
        "the guard cut the start with a Kill on the lane"
    );
    assert!(system.get::<Live>(&name).is_none(), "the name is free");
    let retry = Live
        .spawn()
        .named(name.as_str())
        .on_system(&system)
        .await
        .expect("an immediate retry under the name is not refused");
    assert!(retry.is_alive());
    until("the helper is counted down", || system.live_helpers() == 0).await;
    assert_eq!(
        system.helper_ends(&rt),
        vec![(HelperKind::StartBound, HelperEnd::Panicked)]
    );
}

/// A helper that panics after it took `Acked` leaves the actor running: its
/// guard finds the gate decided and aborts nothing, and the caller reads
/// `Acked` from the closed answer and gets the handle.
#[tokio::test(start_paused = true)]
async fn a_helper_that_panics_after_the_ack_was_taken_leaves_the_actor_running() {
    let system = fresh_system();
    let name = unique("panic-after");
    let hold = hooks::hold(hooks::AFTER_ACKED, ActorId::from(name.as_str()));
    let spawned = start(&system, &name);
    hold.arrived().await;

    hold.fail();
    let handle = match timeout(BOUND, spawned).await.unwrap().unwrap() {
        Ok(handle) => handle,
        Err(err) => panic!("the gate stood at Acked, got {err}"),
    };
    settle().await;
    assert_eq!(handle.send(()).await.unwrap(), 7);
    assert!(system.get::<Live>(&name).is_some());
}

/// A helper that finds the gate taken after a good ack cuts the start
/// itself, so a caller told `StartTimeout` never leaves an actor running.
#[tokio::test(start_paused = true)]
async fn a_helper_that_finds_the_gate_taken_after_its_ack_cuts_the_start() {
    let system = fresh_system();
    let name = unique("gate-taken");
    let id = ActorId::from(name.as_str());
    let probe = hooks::probe(id.clone());
    let hold = hooks::hold(hooks::BEFORE_DECISION, id);
    let spawned = start(&system, &name);
    hold.arrived().await;
    let actor = system.get::<Live>(&name).expect("the actor is registered");

    assert_eq!(probe.gate().decide(TIMED_OUT), TIMED_OUT);
    hold.release();
    match timeout(BOUND, spawned).await.unwrap().unwrap() {
        Err(SpawnError::StartTimeout) => {}
        Err(other) => panic!("expected StartTimeout, got {other}"),
        Ok(_) => panic!("the gate stood at TimedOut"),
    }
    assert_eq!(
        timeout(BOUND, actor.wait_stopped()).await.unwrap(),
        StopReason::Kill
    );
    assert_eq!(
        actor.stop_lane().reason(),
        Some(StopReason::Kill),
        "the helper cut the start with a Kill on the lane"
    );
    assert!(system.get::<Live>(&name).is_none(), "the name is free");
}

/// A helper that is dropped before its first poll drops its guard: the guard
/// is built before the helper's future exists.
#[test]
fn a_helper_dropped_before_its_first_poll_drops_its_guard() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let system = fresh_system();
    let name = unique("never-polled");
    let probe = hooks::probe(ActorId::from(name.as_str()));
    rt.block_on(async {
        let mut spawn = Live
            .spawn()
            .named(name.as_str())
            .on_system(&system)
            .start_timeout(START_TIMEOUT)
            .into_future();
        let first = spawn.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(first, Poll::Pending));
    });
    assert_eq!(
        probe
            .guards_dropped
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "the helper is queued and has not run"
    );
    drop(rt);
    assert_eq!(
        probe
            .guards_dropped
            .load(std::sync::atomic::Ordering::Acquire),
        1,
        "the helper dropped unpolled drops the guard it holds"
    );
}

/// The cut raises the `Kill` before it requests the abort: when the abort is
/// recorded, the lane already holds the `Kill`, and afterwards the task is
/// aborted.
#[tokio::test(start_paused = true)]
async fn the_cut_raises_the_kill_before_it_requests_the_abort() {
    let task = tokio::spawn(std::future::pending::<()>());
    let (lane, _lane_rx) = StopLane::new();
    let held_at_the_abort = Arc::new(std::sync::Mutex::new(None));
    {
        let (lane, held_at_the_abort) = (lane.clone(), held_at_the_abort.clone());
        crate::actor::subtree::test_hook::arm_after_write(move || {
            *held_at_the_abort.lock().unwrap() = Some(lane.reason());
        });
    }

    super::cut_start(&lane, &task.abort_handle());

    assert_eq!(
        held_at_the_abort.lock().unwrap().take(),
        Some(Some(StopReason::Kill)),
        "the lane held the Kill when the abort was recorded"
    );
    assert_eq!(lane.reason(), Some(StopReason::Kill));
    assert!(was_aborted(task).await, "the cut aborted the task");
}

/// A guard on a task, a stop lane, a gate and an answer channel the test holds.
struct Rig {
    task: JoinHandle<()>,
    lane: StopLane,
    /// Keeps the lane open to requests, as the actor's run loop does.
    _lane_rx: LaneRx,
    gate: Arc<Gate>,
    answered: oneshot::Receiver<Outcome>,
    hold: Arc<hooks::StdHold>,
    guard: Guard,
}

impl Rig {
    fn new() -> Self {
        let task = tokio::spawn(std::future::pending::<()>());
        let (lane, lane_rx) = StopLane::new();
        let gate = Gate::new();
        let (answer, answered) = oneshot::channel();
        let hold = Arc::new(hooks::StdHold::default());
        let mut guard = Guard::new(gate.clone(), lane.clone(), task.abort_handle(), answer);
        guard.hold = Some(hold.clone());
        Self {
            task,
            lane,
            _lane_rx: lane_rx,
            gate,
            answered,
            hold,
            guard,
        }
    }
}

/// Whether the task was aborted: an abort ends the task at its next poll.
async fn was_aborted(task: JoinHandle<()>) -> bool {
    settle().await;
    task.is_finished()
}

/// The answer channel closes only after the guard's action: while the guard
/// is held in its action the channel is still open, and when the channel is
/// closed the gate is decided and the task is aborted.
#[tokio::test(start_paused = true)]
async fn the_answer_channel_closes_after_the_guards_action() {
    let mut rig = Rig::new();
    rig.guard.panicked = true;
    let guard = rig.guard;
    let dropper = std::thread::spawn(move || drop(guard));
    rig.hold.wait_arrived();

    assert!(
        matches!(rig.answered.try_recv(), Err(TryRecvError::Empty)),
        "the channel is open while the guard acts"
    );
    rig.hold.release();
    dropper.join().unwrap();

    assert!(matches!(rig.answered.try_recv(), Err(TryRecvError::Closed)));
    assert_eq!(rig.gate.state(), TIMED_OUT);
    assert_eq!(
        rig.lane.reason(),
        Some(StopReason::Kill),
        "the guard raised a Kill before it aborted"
    );
    assert!(was_aborted(rig.task).await, "the guard aborted the task");
}

/// The guard takes the gate with one exchange: a helper that takes `Acked`
/// while the guard is about to act leaves the actor running, and the gate
/// stands at `Acked`.
#[tokio::test(start_paused = true)]
async fn the_guard_aborts_only_when_it_wins_the_gate() {
    let mut rig = Rig::new();
    rig.guard.panicked = true;
    let guard = rig.guard;
    let dropper = std::thread::spawn(move || drop(guard));
    rig.hold.wait_arrived();

    assert_eq!(rig.gate.decide(ACKED), ACKED);
    rig.hold.release();
    dropper.join().unwrap();

    assert_eq!(rig.gate.state(), ACKED);
    assert_eq!(rig.lane.reason(), None, "a gate at Acked raises nothing");
    assert!(
        !was_aborted(rig.task).await,
        "a gate at Acked leaves the task running"
    );
}

/// A panic after the answer was sent takes no action: the gate stays
/// undecided and the task runs.
#[tokio::test(start_paused = true)]
async fn a_panic_after_the_answer_was_sent_takes_no_action() {
    let mut rig = Rig::new();
    rig.guard.hold = None;
    rig.guard.complete(Outcome::ClosedAck);
    rig.guard.panicked = true;
    drop(rig.guard);

    assert!(matches!(rig.answered.try_recv(), Ok(Outcome::ClosedAck)));
    assert_eq!(rig.gate.state(), ARMED);
    assert_eq!(rig.lane.reason(), None, "a completed helper raises nothing");
    assert!(
        !was_aborted(rig.task).await,
        "a completed helper leaves the task alone"
    );
}

/// A helper that ends because its runtime ended takes no action either: the
/// guard of a helper that did not panic aborts nothing.
#[tokio::test(start_paused = true)]
async fn a_guard_dropped_without_a_panic_takes_no_action() {
    let mut rig = Rig::new();
    rig.guard.hold = None;
    drop(rig.guard);

    assert_eq!(rig.gate.state(), ARMED);
    assert_eq!(
        rig.lane.reason(),
        None,
        "a guard without a panic raises nothing"
    );
    assert!(!was_aborted(rig.task).await);
}

/// The race between the ack and the deadline, at real time: whichever side
/// wins, a caller told `StartTimeout` finds the name free at once, and a
/// caller that got the handle has a running actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_named_retry_after_a_start_timeout_never_finds_the_name_taken() {
    let init = Duration::from_millis(4);
    let mut timed_out = 0;
    let mut started = 0;
    for pass in 0..80u64 {
        let system = fresh_system();
        let name = unique(&format!("race-{pass}"));
        let offset = Duration::from_micros(pass % 8 * 500);
        let within = init - Duration::from_millis(2) + offset;
        let result = SlowStart(init)
            .spawn()
            .named(name.as_str())
            .on_system(&system)
            .start_timeout(within)
            .await;
        match result {
            Ok(handle) => {
                started += 1;
                assert!(handle.is_alive(), "a started actor runs");
            }
            Err(SpawnError::StartTimeout) => {
                timed_out += 1;
                let retry = Live.spawn().named(name.as_str()).on_system(&system).await;
                assert!(retry.is_ok(), "retry after StartTimeout: {:?}", retry.err());
            }
            Err(other) => panic!("unexpected {other}"),
        }
    }
    assert!(
        timed_out > 0 && started > 0,
        "the race went both ways: {timed_out} timed out, {started} started"
    );
}

/// A start timeout of zero leaves no time for an ack: the spawn answers
/// `StartTimeout` however early the actor acknowledges, the actor is gone and
/// its name is free for a retry.
#[tokio::test(start_paused = true)]
async fn a_zero_start_timeout_answers_start_timeout_and_frees_the_name() {
    let system = fresh_system();
    let name = unique("zero");
    let result = Live
        .spawn()
        .named(name.as_str())
        .on_system(&system)
        .start_timeout(Duration::ZERO)
        .await;
    assert!(
        matches!(result, Err(SpawnError::StartTimeout)),
        "{:?}",
        result.err()
    );
    let retry = Live.spawn().named(name.as_str()).on_system(&system).await;
    assert!(retry.is_ok(), "retry after StartTimeout: {:?}", retry.err());
}

/// An ack that is ready when the helper runs is accepted whatever the
/// deadline says: at the deadline instant, and after it.
#[tokio::test(start_paused = true)]
async fn an_ack_ready_at_or_after_the_deadline_is_accepted() {
    for past in [Duration::ZERO, Duration::from_millis(5)] {
        let task = tokio::spawn(std::future::pending::<StopReason>());
        let gate = Gate::new();
        let (ack_tx, ack) = oneshot::channel();
        ack_tx.send(Ok(())).unwrap();
        let deadline = Instant::now();
        advance(past).await;
        let bound = Bound {
            gate: gate.clone(),
            ack,
            abort: task.abort_handle(),
            join: task,
            lane: StopLane::new().0,
            deadline,
            immediate: false,
            id: ActorId::from("ready-ack"),
        };
        let outcome = bound.run().await;
        assert!(
            matches!(outcome, Outcome::Started),
            "past={past:?}: {outcome:?}"
        );
        assert_eq!(gate.state(), ACKED);
    }
}

// ---------------------------------------------------------------------------
// Wakers of other tasks that panic
// ---------------------------------------------------------------------------

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: std::sync::atomic::AtomicBool,
    woken: std::sync::atomic::AtomicUsize,
}

impl LoudWake {
    fn arm(&self, armed: bool) {
        self.armed.store(armed, std::sync::atomic::Ordering::SeqCst);
    }

    fn woken(&self) -> usize {
        self.woken.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl std::task::Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
            panic!("a waker's wake panics");
        }
    }
}

/// The cut raises the `Kill` before it requests the abort, and the raise wakes
/// the tasks that wait for the lane's signals. A waker among them that panics
/// stays out of the raise: the lane holds the `Kill`, the cut goes on to the
/// abort, and the abort is recorded after the `Kill`.
#[tokio::test(start_paused = true)]
async fn the_cut_aborts_the_task_when_a_waiter_of_the_lane_panics_in_the_kill_raise() {
    let task = tokio::spawn(std::future::pending::<()>());
    let (lane, mut lane_rx) = StopLane::new();
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    let mut cx = Context::from_waker(&waker);
    let mut waiter = Box::pin(lane_rx.changed());
    assert!(std::future::Future::poll(waiter.as_mut(), &mut cx).is_pending());
    let held_at_the_abort = Arc::new(std::sync::Mutex::new(None));
    {
        let (lane, held_at_the_abort) = (lane.clone(), held_at_the_abort.clone());
        crate::actor::subtree::test_hook::arm_after_write(move || {
            *held_at_the_abort.lock().unwrap() = Some(lane.reason());
        });
    }
    loud.arm(true);

    let cut = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::cut_start(&lane, &task.abort_handle())
    }));
    loud.arm(false);

    assert!(cut.is_ok(), "the waiter's waker unwound out of the cut");
    assert_eq!(loud.woken(), 1, "the waiter's waker was woken once");
    assert_eq!(
        held_at_the_abort.lock().unwrap().take(),
        Some(Some(StopReason::Kill)),
        "the lane held the Kill when the abort was recorded"
    );
    assert!(was_aborted(task).await, "the cut aborted the task");
}

/// A task waits for the actor's end with a waker that panics when it is woken.
/// The start times out and the helper's cut raises the `Kill`, which wakes the
/// waiter: the panic stays out of the helper, so the helper ends clean, the
/// caller is told `StartTimeout`, the actor ends as `Kill` and its name is
/// free.
#[tokio::test(start_paused = true)]
async fn a_waiter_whose_waker_panics_in_the_cut_of_a_timed_out_start_leaves_the_helper_clean() {
    let system = fresh_system();
    let rt = tokio::runtime::Handle::current();
    let name = unique("loud-cut");
    let builder = SlowStart(Duration::from_secs(3600))
        .spawn()
        .named(name.as_str())
        .on_system(&system)
        .start_timeout(START_TIMEOUT);
    let spawned = tokio::spawn(async move { builder.await });
    until("the actor is registered", || {
        system.get::<SlowStart>(&name).is_some()
    })
    .await;
    let actor = system.get::<SlowStart>(&name).unwrap();
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    let mut waiter = Box::pin(actor.wait_stopped());
    let mut cx = Context::from_waker(&waker);
    assert!(std::future::Future::poll(waiter.as_mut(), &mut cx).is_pending());
    loud.arm(true);

    advance(START_TIMEOUT + ABORT_SETTLE).await;
    settle().await;
    match timeout(BOUND, spawned).await.unwrap().unwrap() {
        Err(SpawnError::StartTimeout) => {}
        Err(other) => panic!("expected StartTimeout, got {other}"),
        Ok(_) => panic!("the start cannot have succeeded"),
    }
    loud.arm(false);
    assert_eq!(loud.woken(), 1, "the waiter's waker was woken once");
    assert_eq!(
        timeout(BOUND, actor.wait_stopped()).await.unwrap(),
        StopReason::Kill
    );
    assert!(system.get::<SlowStart>(&name).is_none(), "the name is free");
    until("the helper is counted down", || system.live_helpers() == 0).await;
    assert_eq!(
        system.helper_ends(&rt),
        vec![(HelperKind::StartBound, HelperEnd::Finished)],
        "a waker's panic ended the helper"
    );
}

/// The same waiter, and a helper that panics before its decision: the guard's
/// cut raises the `Kill` while the helper's panic unwinds, and the wake of the
/// waiter panics inside that unwind. A panic that escaped the guard's drop
/// would be a second panic during an unwind and end the process; contained, the
/// guard goes on to the abort and the caller is told `StartTimeout`.
#[test]
fn a_waiter_whose_waker_panics_in_the_guards_cut_does_not_end_the_process() {
    if !crate::actor::panic::in_a_child_process(
        module_path!(),
        "a_waiter_whose_waker_panics_in_the_guards_cut_does_not_end_the_process",
    ) {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a paused current-thread runtime");
    runtime.block_on(async {
        let system = fresh_system();
        let name = unique("loud-guard");
        let hold = hooks::hold(hooks::BEFORE_DECISION, ActorId::from(name.as_str()));
        let spawned = start(&system, &name);
        hold.arrived().await;
        let actor = system.get::<Live>(&name).expect("the actor is registered");
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut waiter = Box::pin(actor.wait_stopped());
        let mut cx = Context::from_waker(&waker);
        assert!(std::future::Future::poll(waiter.as_mut(), &mut cx).is_pending());
        loud.arm(true);

        hold.fail();
        match timeout(BOUND, spawned).await.unwrap().unwrap() {
            Err(SpawnError::StartTimeout) => {}
            Err(other) => panic!("expected StartTimeout, got {other}"),
            Ok(_) => panic!("the helper never took the gate for the ack"),
        }
        loud.arm(false);
        assert_eq!(loud.woken(), 1, "the waiter's waker was woken once");
        assert_eq!(
            timeout(BOUND, actor.wait_stopped()).await.unwrap(),
            StopReason::Kill
        );
        assert!(system.get::<Live>(&name).is_none(), "the name is free");
    });
}

/// The caller of a timed spawn waits on the helper's answer, with no bound of
/// its own, and polls with a waker that panics when it is woken. The helper
/// answers: the panic stays out of the helper, which ends clean, and the
/// caller reads the handle on its next poll.
#[tokio::test(start_paused = true)]
async fn a_caller_whose_waker_panics_when_the_helper_answers_leaves_the_helper_clean() {
    let system = fresh_system();
    let rt = tokio::runtime::Handle::current();
    let name = unique("loud-answer");
    let hold = hooks::hold(hooks::AFTER_ACKED, ActorId::from(name.as_str()));
    let mut call = Box::pin(
        Live.spawn()
            .named(name.as_str())
            .on_system(&system)
            .start_timeout(START_TIMEOUT)
            .into_future(),
    );
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    assert!(
        std::future::Future::poll(call.as_mut(), &mut Context::from_waker(&waker)).is_pending()
    );
    hold.arrived().await;
    loud.arm(true);

    hold.release();
    until("the caller is woken", || loud.woken() == 1).await;
    loud.arm(false);

    until("the helper is counted down", || system.live_helpers() == 0).await;
    assert_eq!(
        system.helper_ends(&rt),
        vec![(HelperKind::StartBound, HelperEnd::Finished)],
        "a caller's waker ended the helper"
    );
    match std::future::Future::poll(call.as_mut(), &mut Context::from_waker(Waker::noop())) {
        Poll::Ready(Ok(handle)) => assert!(handle.is_alive()),
        Poll::Ready(Err(err)) => panic!("the helper answered Started, got {err}"),
        Poll::Pending => panic!("the caller reads the answer on its next poll"),
    }
}

/// The same caller, and a helper that panics before its decision: the guard
/// drops the answer unanswered while the panic unwinds, which wakes the caller
/// and panics in the wake. A panic that escaped the guard's drop would be a
/// second panic during the unwind and end the process; contained, the guard
/// goes on and the caller is told `StartTimeout`.
#[test]
fn a_caller_whose_waker_panics_when_a_panicked_helpers_guard_drops_the_answer_does_not_end_the_process(
) {
    if !crate::actor::panic::in_a_child_process(
        module_path!(),
        "a_caller_whose_waker_panics_when_a_panicked_helpers_guard_drops_the_answer_does_not_end_the_process",
    ) {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a paused current-thread runtime");
    runtime.block_on(async {
        let system = fresh_system();
        let name = unique("loud-answer-unwind");
        let hold = hooks::hold(hooks::BEFORE_DECISION, ActorId::from(name.as_str()));
        let mut call = Box::pin(
            Live.spawn()
                .named(name.as_str())
                .on_system(&system)
                .start_timeout(START_TIMEOUT)
                .into_future(),
        );
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        assert!(
            std::future::Future::poll(call.as_mut(), &mut Context::from_waker(&waker)).is_pending()
        );
        hold.arrived().await;
        loud.arm(true);

        hold.fail();
        until("the caller is woken", || loud.woken() == 1).await;
        loud.arm(false);
        match timeout(BOUND, call).await.unwrap() {
            Err(SpawnError::StartTimeout) => {}
            Err(other) => panic!("expected StartTimeout, got {other}"),
            Ok(_) => panic!("the helper never took the gate for the ack"),
        }
        assert!(system.get::<Live>(&name).is_none(), "the name is free");
    });
}
