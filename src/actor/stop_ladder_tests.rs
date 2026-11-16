//! The stop task of a bounded stop: a stop taken on a handle from a registry
//! lookup, before the actor's task has its abort handle attached, still
//! escalates (the stop task raises its `Kill` while no abort handle exists,
//! and the attach itself arms the backstop the lane's `Kill` owes), and a stop
//! task that is lost hands its steps to the reaper of the actor's runtime,
//! with the window counted from the call.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, Notify};

use super::*;
use crate::actor::context::ActorContext;
use crate::actor::incarnation::StatusValue;
use crate::actor::incarnation::{StatusTx, StopLane, TurnClock};
use crate::actor::ActorExt;
use crate::error::ActorResult;
use crate::helper::{HelperEnd, HelperKind};
use crate::system::{ActorSystem, LadderPoint};
use crate::types::{ActorId, ActorStatus, StopOutcome, StopReason};

// Every wall-clock bound below only limits how long a real hang takes to
// report; no test asserts a timing relation against it. A timing relation is
// read on a paused clock, where every instant is virtual.

/// Scheduling rounds of a one-thread runtime that an awaited step is given.
const ROUNDS: usize = 1000;

/// A gate a ladder hook blocks its thread on until the test, or another
/// hook, opens it.
#[derive(Default)]
struct Latch {
    open: std::sync::Mutex<bool>,
    opened: std::sync::Condvar,
}

impl Latch {
    fn open(&self) {
        *self.open.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.opened.notify_all();
    }

    /// Blocks the calling thread until the latch is open.
    fn wait(&self) {
        let open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        let _ = self
            .opened
            .wait_timeout_while(open, Duration::from_secs(60), |open| !*open);
    }
}

/// Says when it is dropped. Held by a target's task, its drop says that the
/// abort has landed. As the payload a lost stop task panics with, its drop
/// says that the loss has been handled: the helper drops the payload after
/// the task's own future, whose drop hands the lost steps over.
struct DropSignal(Arc<Notify>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// Waits for `notify`; the bound only reports a hang.
async fn notified(notify: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(60), notify.notified())
        .await
        .unwrap_or_else(|_| panic!("{what}"));
}

/// A channel the ladder hook reports each answered stop task on. A stop task
/// answers as the last step of its run, so on a one-thread runtime the end
/// of the task is recorded in the same poll, before the test runs again.
async fn answered(rx: &mut mpsc::UnboundedReceiver<()>, what: &str) {
    tokio::time::timeout(Duration::from_secs(60), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("{what}"))
        .expect("the hook holds the sender");
}

#[derive(Default)]
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Polls `call` once and leaves it where it is: a kill call raises its `Kill`
/// before its first await. Returns whether the call finished in that poll,
/// after which it must not be polled again.
fn poll_once<F: std::future::Future + ?Sized>(call: std::pin::Pin<&mut F>) -> bool {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    call.poll(&mut cx).is_ready()
}

/// The call is made while the registry entry has no abort handle; the stop
/// task raises its `Kill` in that window, and the attach that follows arms
/// the abort, which ends the task and so the call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_taken_before_the_abort_handle_is_attached_still_escalates() {
    let sys = ActorSystem::create(format!("stop-before-attach-{}", uuid::Uuid::new_v4())).unwrap();
    let name = "stop-before-attach-name";
    let id = ActorId::from(name);
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, stop_rx) = StopLane::new();
    let registered = ActorHandle::<Idle>::new(
        id.clone(),
        tx,
        stop_lane.clone(),
        1,
        status_rx,
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    let grace = Duration::from_millis(100);
    let seq = sys
        .register_actor::<Idle>(
            &id,
            true,
            &registered,
            true,
            TurnClock::default(),
            grace,
            Handle::current(),
        )
        .unwrap();
    let handle = sys
        .get_by_id::<Idle>(&id)
        .expect("the registry lookup finds the actor");
    let raised = Arc::new(Notify::new());
    {
        let raised = raised.clone();
        sys.set_ladder_hook(move |point, _id| {
            if point == LadderPoint::StopAfterKill {
                raised.notify_one();
            }
        });
    }

    let call = tokio::spawn(async move {
        handle
            .stop_within(StopReason::Graceful, Duration::from_millis(20))
            .await
    });

    // The stop task has raised its `Kill` while the entry holds no abort.
    notified(&raised, "the stop task raised its Kill").await;
    assert_eq!(stop_lane.reason(), Some(StopReason::Kill));

    let task = tokio::spawn(async move {
        let _owned = (status_tx, stop_rx);
        std::future::pending::<()>().await;
    });
    sys.attach_abort(&id, task.abort_handle(), seq);

    let ended = tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .expect("the attach armed the backstop, which aborted the task");
    assert!(ended.unwrap_err().is_cancelled());
    sys.clear_ladder_hook();
    let outcome = tokio::time::timeout(Duration::from_secs(60), call)
        .await
        .expect("the call returns")
        .unwrap()
        .expect("the graceful request was accepted");
    assert_eq!(outcome, StopOutcome::Aborted);
}

// ---------------------------------------------------------------------------
// A lost stop task
// ---------------------------------------------------------------------------

/// Vetoes every stoppable stop.
#[derive(Default)]
struct Veto;

impl Actor for Veto {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }
}

fn unique(what: &str) -> String {
    format!("{what}-{}", uuid::Uuid::new_v4())
}

/// Installs a hook that makes the stop task panic when it starts.
fn lose_the_task_at_its_start(sys: &ActorSystem) {
    sys.set_ladder_hook(|point, _id| {
        if point == LadderPoint::StopHelperStart {
            panic!("stop task lost");
        }
    });
}

/// A runtime without the time driver, with a worker thread of its own.
fn runtime_without_timers() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap()
}

/// A vetoing actor with kill grace `grace` running on `rt`, registered in
/// `sys`.
async fn veto_on(
    rt: &tokio::runtime::Runtime,
    sys: &Arc<ActorSystem>,
    grace: Duration,
) -> ActorHandle<Veto> {
    let sys = sys.clone();
    rt.spawn(async move {
        Veto.spawn()
            .on_system(&sys)
            .kill_grace(grace)
            .await
            .unwrap()
    })
    .await
    .unwrap()
}

/// Vetoes every stoppable stop, and ends by itself on a message.
#[derive(Default)]
struct Quitter;

impl Actor for Quitter {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(crate::error::ActorError::user("done"))
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }
}

/// The stop task of a pending call holds no handle of the actor, so the
/// handles of users are the only ones that keep the actor's mailbox open: the
/// handle block, which holds the mailbox's sender, has the same number of
/// holders before the call and while it pends, and the mailbox has the same
/// number of senders.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_call_holds_no_sender_of_the_mailbox() {
    let sys = ActorSystem::create(unique("no-handle-in-the-task")).unwrap();
    let id = ActorId::from("no-handle-in-the-task-id");
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, _stop_rx) = StopLane::new();
    let registered = ActorHandle::<Idle>::new(
        id.clone(),
        tx,
        stop_lane,
        1,
        status_rx,
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    sys.register_actor::<Idle>(
        &id,
        false,
        &registered,
        true,
        TurnClock::default(),
        Duration::from_millis(100),
        Handle::current(),
    )
    .unwrap();
    let before = Arc::strong_count(&registered.inner);
    let senders = registered.inner.tx.strong_count();

    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut call =
        Box::pin(registered.stop_within(StopReason::Graceful, Duration::from_secs(3600)));
    assert!(std::future::Future::poll(call.as_mut(), &mut cx).is_pending());
    assert_eq!(sys.live_helpers(), 1, "the call spawned its stop task");
    assert_eq!(
        Arc::strong_count(&registered.inner),
        before,
        "the stop task of a pending call holds a handle of the actor"
    );
    assert_eq!(
        registered.inner.tx.strong_count(),
        senders,
        "the stop task of a pending call holds a sender of the mailbox"
    );
    drop(call);
    drop(status_tx);
}

// ---------------------------------------------------------------------------
// The stop task of a call that needs none of its steps
// ---------------------------------------------------------------------------

/// Installs a hook on `sys` that reports each answered stop task on the
/// returned channel.
fn report_answers(sys: &ActorSystem) -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    sys.set_ladder_hook(move |point, _id| {
        if point == LadderPoint::StopAnswered {
            let _ = tx.send(());
        }
    });
    rx
}

/// A call that returns gives its stop task back at once, not when its
/// window ends, whether it is made inside a task or outside every task. The
/// window outlasts the bound on the wait for the tasks' answers, and the
/// runtime has one thread, so each task's end is recorded when its answer is
/// reported.
#[tokio::test]
async fn a_call_that_returns_gives_its_stop_task_back() {
    let sys = ActorSystem::create(unique("returns")).unwrap();
    let mut answers = report_answers(&sys);
    let inside = Idle.spawn().on_system(&sys).await.unwrap();
    let outside = Idle.spawn().on_system(&sys).await.unwrap();
    let from_a_task = tokio::spawn(async move {
        inside
            .stop_within(StopReason::Graceful, Duration::from_secs(3600))
            .await
    })
    .await
    .unwrap()
    .expect("the request is accepted");
    assert_eq!(from_a_task, StopOutcome::Graceful);
    let from_the_test = outside
        .stop_within(StopReason::Graceful, Duration::from_secs(3600))
        .await
        .expect("the request is accepted");
    assert_eq!(from_the_test, StopOutcome::Graceful);
    answered(&mut answers, "the first stop task ended").await;
    answered(&mut answers, "the second stop task ended").await;
    sys.clear_ladder_hook();
    assert_eq!(sys.live_helpers(), 0, "the stop tasks ended");
    assert_eq!(
        sys.helper_ends(&Handle::current()),
        vec![
            (HelperKind::StopLadder, HelperEnd::Finished),
            (HelperKind::StopLadder, HelperEnd::Finished)
        ]
    );
}

// ---------------------------------------------------------------------------
// The abort after the window of a bounded Kill
// ---------------------------------------------------------------------------

/// Handles a message by waiting on a future that never completes: a wedge
/// an abort cuts at its `.await`, though a `Kill` is never read in it. It
/// says when it has entered the turn.
struct Hang(Arc<Notify>);

impl Actor for Hang {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.0.notify_one();
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// An actor whose `pre_stop` never returns and counts how often it started.
struct SleepyStop(Arc<std::sync::atomic::AtomicUsize>);

impl Actor for SleepyStop {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<bool>().await
    }
}

/// A window that has already passed at the call takes the `Kill` in that
/// call, with the request: on a current-thread runtime the actor has not run
/// since, and reads the `Kill`, so its `pre_stop` never starts.
#[tokio::test(start_paused = true)]
async fn a_window_that_has_passed_at_the_call_takes_the_kill_at_the_call() {
    let sys = ActorSystem::create(unique("window-passed")).unwrap();
    let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = SleepyStop(started.clone())
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();
    let outcome = h
        .stop_within(StopReason::Graceful, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(outcome, StopOutcome::Killed);
    assert_eq!(started.load(Ordering::SeqCst), 0, "pre_stop never started");
}

// ---------------------------------------------------------------------------
// A Kill whose target has no abort handle yet
// ---------------------------------------------------------------------------

/// A registered actor whose task exists but whose abort handle is not
/// attached yet. The task never ends by itself and says when it is dropped.
struct Target {
    sys: Arc<ActorSystem>,
    handle: ActorHandle<Idle>,
    id: ActorId,
    name: String,
    seq: u64,
    task: tokio::task::JoinHandle<()>,
    dropped: Arc<Notify>,
}

impl Target {
    fn new(kill_grace: Duration) -> Self {
        let sys = ActorSystem::create(unique("unattached")).unwrap();
        let name = unique("unattached-name");
        let id = ActorId::from(name.as_str());
        let (tx, _rx) = crate::actor::mailbox::test_channel(1);
        let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, stop_rx) = StopLane::new();
        let registered = ActorHandle::<Idle>::new(
            id.clone(),
            tx,
            stop_lane,
            1,
            status_rx,
            None,
            Some(Arc::new(AtomicBool::new(false))),
        );
        let seq = sys
            .register_actor::<Idle>(
                &id,
                true,
                &registered,
                true,
                TurnClock::default(),
                kill_grace,
                Handle::current(),
            )
            .unwrap();
        let handle = sys.get_by_id::<Idle>(&id).unwrap();
        let dropped = Arc::new(Notify::new());
        let signal = DropSignal(dropped.clone());
        let task = tokio::spawn(async move {
            let _owned = (status_tx, stop_rx, signal);
            std::future::pending::<()>().await;
        });
        Self {
            sys,
            handle,
            id,
            name,
            seq,
            task,
            dropped,
        }
    }

    fn attach(&self) {
        self.sys
            .attach_abort(&self.id, self.task.abort_handle(), self.seq);
    }

    /// Waits until the abort has dropped the task and returns how long that
    /// took from `since`. Used on a paused clock, where the time is virtual:
    /// the declared deadline's distance from `since`.
    async fn aborted_after(&self, since: tokio::time::Instant) -> Duration {
        notified(&self.dropped, "the target was aborted").await;
        since.elapsed()
    }
}

// ---------------------------------------------------------------------------
// The decision on a call's first request
// ---------------------------------------------------------------------------

/// A registered actor with no run loop, its abort handle attached, and the
/// reaper of the test runtime replaced by a receiver that holds what it is
/// fed.
struct Held {
    sys: Arc<ActorSystem>,
    handle: ActorHandle<Idle>,
    lane: StopLane,
    feed: mpsc::UnboundedReceiver<crate::system::ReaperEntry>,
    _task: tokio::task::JoinHandle<()>,
}

fn held(kill_grace: Duration) -> Held {
    let target = Target::new(kill_grace);
    target.attach();
    let feed = target.sys.capture_reaper(&Handle::current());
    let lane = target.handle.stop_lane().clone();
    Held {
        sys: target.sys.clone(),
        handle: target.handle.clone(),
        lane,
        feed,
        _task: target.task,
    }
}

/// Latches the hook opens as the ladder passes its points.
#[derive(Default)]
struct Passed {
    lost: Latch,
    decided: Latch,
}

/// How the stop task of a call made inside a task is lost, relative to the
/// decision on the call's first request.
#[derive(Clone, Copy, PartialEq)]
enum LostAt {
    BeforeTheDecision,
    AfterTheDecision,
}

/// Runs `stop_within(Graceful, window)` inside a task with the stop task lost
/// as `lost_at` says, `refuse` making the lane refuse the request, and
/// returns the call's result and what the reaper was fed.
async fn lost_task_call(
    lost_at: LostAt,
    refuse: bool,
) -> (
    Result<StopOutcome, crate::error::SendError>,
    usize,
    Option<StopReason>,
) {
    let mut held = held(Duration::from_millis(100));
    let passed = Arc::new(Passed::default());
    let loss_handled = Arc::new(Notify::new());
    {
        let (passed, lane, loss_handled) =
            (passed.clone(), held.lane.clone(), loss_handled.clone());
        held.sys.set_ladder_hook(move |point, _id| match point {
            LadderPoint::StopBeforeRaise if refuse => lane.close_to_non_kill(),
            LadderPoint::StopHelperStart => {
                if lost_at == LostAt::AfterTheDecision {
                    passed.decided.wait();
                }
                std::panic::panic_any(DropSignal(loss_handled.clone()));
            }
            LadderPoint::StopHelperLost => passed.lost.open(),
            LadderPoint::StopRaised if lost_at == LostAt::BeforeTheDecision => {
                passed.lost.wait();
            }
            LadderPoint::StopDecided => passed.decided.open(),
            _ => {}
        });
    }
    // The caller is a thread outside the runtime's workers, so the stop task
    // it spawns is picked up by a worker while the caller waits in a hook.
    let handle = held.handle.clone();
    let rt = Handle::current();
    let result = tokio::task::spawn_blocking(move || {
        rt.block_on(handle.stop_within(StopReason::Graceful, Duration::from_millis(100)))
    })
    .await
    .unwrap();
    // The caller hands its steps over before it returns, and the lost task
    // before its payload is dropped: after both, nothing more is fed.
    notified(&loss_handled, "the loss was handled").await;
    held.sys.clear_ladder_hook();
    let mut fed = 0;
    while held.feed.try_recv().is_ok() {
        fed += 1;
    }
    (result, fed, held.lane.reason())
}

/// A lost stop task's hand-off on a target whose abort handle is not attached
/// declares its grace before it raises Kill, so a windowed Kill declared
/// meanwhile with a longer window does not decide the abort. The clock is
/// paused, so the abort's instant is the declared deadline and not the host's
/// scheduling: the grace ends it in a fraction of the longer window.
#[tokio::test(start_paused = true)]
async fn a_lost_tasks_hand_off_declares_its_grace_before_its_kill() {
    const GRACE: Duration = Duration::from_millis(150);
    const LONG_WINDOW: Duration = Duration::from_secs(4);
    let target = Target::new(GRACE);
    let fired = Arc::new(AtomicBool::new(false));
    let handed_off = Arc::new(Notify::new());
    {
        let (fired, handed_off) = (fired.clone(), handed_off.clone());
        target.sys.set_ladder_hook(move |point, _id| match point {
            LadderPoint::StopHelperStart if !fired.swap(true, Ordering::SeqCst) => {
                panic!("stop task lost")
            }
            LadderPoint::StopHelperLost => handed_off.notify_one(),
            _ => {}
        });
    }
    let h1 = target.handle.clone();
    let first = tokio::spawn(async move {
        h1.stop_within(StopReason::Graceful, Duration::from_millis(10))
            .await
    });
    // The lost task's hand-off raises its Kill, then reports the loss. Only
    // the paused clock's bound ends this wait, and it ends it only if nothing
    // else can run, so the loss was never handled.
    notified(&handed_off, "the lost task handed its steps over").await;
    assert_eq!(
        target.handle.stop_lane().reason(),
        Some(StopReason::Kill),
        "hand-off raised Kill"
    );
    let raised = tokio::time::Instant::now();
    let h2 = target.handle.clone();
    let mut second = Box::pin(async move { h2.stop_within(StopReason::Kill, LONG_WINDOW).await });
    poll_once(second.as_mut());
    target.attach();
    let took = target.aborted_after(raised).await;
    assert!(
        took < LONG_WINDOW / 2,
        "aborted {took:?} after the Kill, grace was {GRACE:?}"
    );
    let _ = first.await;
    let _ = second.await;
}
