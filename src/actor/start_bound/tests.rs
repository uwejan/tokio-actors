//! The start bound under test: the race of the gate between the helper and
//! the deadline, the closed answer, and the fail-closed action of the
//! helper's guard. Each point is ordered by observation: the test holds the
//! helper (or the guard) at a named point, acts, and then lets it go on.
//!
//! In every case the caller and the actor agree: either the caller got the
//! handle and the actor runs, or the caller got `StartTimeout` and the actor
//! was aborted and its name freed.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{advance, timeout, Instant};

use super::{hooks, Bound, Gate, Guard, Outcome, ACKED};
use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
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
