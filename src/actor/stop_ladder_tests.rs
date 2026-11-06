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
use tokio::sync::mpsc;

use super::*;
use crate::actor::context::ActorContext;
use crate::actor::ActorExt;
use crate::error::ActorResult;
use crate::helper::{HelperEnd, HelperKind};
use crate::system::{ActorSystem, LadderPoint};
use crate::types::{
    ActorId, ActorStatus, StatusTx, StatusValue, StopLane, StopOutcome, StopReason, TurnClock,
};

/// How long a wait that must complete may take before its test reports a
/// hang: the bound only limits how long a real hang takes to report. No test
/// asserts a timing relation against it.
const BOUND: Duration = Duration::from_secs(60);

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
    let id = ActorId::from("stop-before-attach-id");
    let name = "stop-before-attach-name";
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, stop_rx) = StopLane::new();
    let registered = ActorHandle::<Idle>::new(
        id.clone(),
        tx,
        stop_lane.clone(),
        1,
        status_rx,
        sys.name().into(),
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    let grace = Duration::from_millis(100);
    let seq = sys
        .register_actor::<Idle>(
            &id,
            Some(&Arc::from(name)),
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

    let call = tokio::spawn(async move {
        handle
            .stop_within(StopReason::Graceful, Duration::from_millis(20))
            .await
    });

    // The stop task has raised its `Kill` while the entry holds no abort.
    tokio::time::timeout(BOUND, async {
        while !matches!(stop_lane.reason(), Some(StopReason::Kill)) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the stop task raised its Kill");

    let task = tokio::spawn(async move {
        let _owned = (status_tx, stop_rx);
        std::future::pending::<()>().await;
    });
    sys.attach_abort(&id, Some(name), task.abort_handle(), seq);

    let ended = tokio::time::timeout(BOUND, task)
        .await
        .expect("the attach armed the backstop, which aborted the task");
    assert!(ended.unwrap_err().is_cancelled());
    let outcome = tokio::time::timeout(BOUND, call)
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

/// The reaper of a runtime without the time driver cannot hold an entry, so
/// a lost stop task takes no step for a target there, except a `Kill` whose
/// window has already passed: with a window of zero the `Kill` is raised at
/// once and the call reads `Killed`. The grace is long, so the call's own
/// bound cannot pass before the target's thread has taken the `Kill`.
#[tokio::test]
async fn a_lost_task_whose_window_has_passed_raises_its_kill_for_a_target_without_timers() {
    let sys = ActorSystem::create(unique("lost-passed-no-timers")).unwrap();
    let target_rt = runtime_without_timers();
    let h = veto_on(&target_rt, &sys, Duration::from_secs(30)).await;
    lose_the_task_at_its_start(&sys);
    let outcome = tokio::time::timeout(BOUND, h.stop_within(StopReason::Graceful, Duration::ZERO))
        .await
        .expect("the call returns")
        .expect("the request is accepted");
    sys.clear_ladder_hook();
    assert_eq!(outcome, StopOutcome::Killed);
    let reason = tokio::time::timeout(BOUND, h.wait_stopped())
        .await
        .expect("the actor ends");
    assert_eq!(reason, StopReason::Kill);
    target_rt.shutdown_background();
}

/// The stop task of a pending call holds no sender of the mailbox, so the
/// handles of users are the only ones that keep the actor's mailbox open: the
/// number of senders is the same before the call, while it pends, and after
/// it is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_call_holds_no_sender_of_the_mailbox() {
    let sys = ActorSystem::create(unique("no-handle-in-the-task")).unwrap();
    let id = ActorId::from("no-handle-in-the-task-id");
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let senders = tx.clone();
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, _stop_rx) = StopLane::new();
    let registered = ActorHandle::<Idle>::new(
        id.clone(),
        tx,
        stop_lane,
        1,
        status_rx,
        sys.name().into(),
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    sys.register_actor::<Idle>(
        &id,
        None,
        &registered,
        true,
        TurnClock::default(),
        Duration::from_millis(100),
        Handle::current(),
    )
    .unwrap();
    let before = senders.strong_count();

    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut call =
        Box::pin(registered.stop_within(StopReason::Graceful, Duration::from_secs(3600)));
    assert!(std::future::Future::poll(call.as_mut(), &mut cx).is_pending());
    assert_eq!(sys.live_helpers(), 1, "the call spawned its stop task");
    assert_eq!(
        senders.strong_count(),
        before,
        "the stop task of a pending call holds a sender of the mailbox"
    );
    drop(call);
    drop(status_tx);
}

// ---------------------------------------------------------------------------
// The stop task of a call that needs none of its steps
// ---------------------------------------------------------------------------

async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(BOUND, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

/// A call that returns gives its stop task back at once, not when its
/// window ends, whether it is made inside a task or outside every task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_that_returns_gives_its_stop_task_back() {
    let sys = ActorSystem::create(unique("returns")).unwrap();
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
    until("the stop tasks ended", || sys.live_helpers() == 0).await;
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

/// An actor whose `pre_stop` sleeps and counts how often it started.
struct SleepyStop(Arc<std::sync::atomic::AtomicUsize>);

impl Actor for SleepyStop {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(3600)).await;
        true
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

/// A call whose first request the lane refuses returns `Err` and leaves no
/// `Kill` on the lane, even when the stop task spawned before the raise has
/// reached its step by then (the caller is blocked between the spawn and the
/// raise).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_first_request_in_a_blocking_caller_leaves_no_kill() {
    let sys = ActorSystem::create(format!("refused-first-{}", uuid::Uuid::new_v4())).unwrap();
    let id = ActorId::from("refused-first-id");
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (_status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, _stop_rx) = StopLane::new();
    let registered = ActorHandle::<Idle>::new(
        id.clone(),
        tx,
        stop_lane.clone(),
        1,
        status_rx,
        sys.name().into(),
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    sys.register_actor::<Idle>(
        &id,
        None,
        &registered,
        true,
        TurnClock::default(),
        Duration::from_millis(100),
        Handle::current(),
    )
    .unwrap();
    let handle = sys.get_by_id::<Idle>(&id).unwrap();
    let lane = stop_lane.clone();
    sys.set_ladder_hook(move |point, _id| {
        if point == LadderPoint::StopBeforeRaise {
            lane.close_to_non_kill();
            // Gives the stop task, already spawned, every chance to act.
            let end = std::time::Instant::now() + Duration::from_millis(500);
            while lane.reason().is_none() && std::time::Instant::now() < end {
                std::thread::yield_now();
            }
        }
    });
    let rt = Handle::current();
    let result = tokio::task::spawn_blocking(move || {
        rt.block_on(handle.stop_within(StopReason::Graceful, Duration::ZERO))
    })
    .await
    .unwrap();
    assert!(result.is_err(), "the lane refused the request: {result:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        stop_lane.reason().is_none(),
        "the refused call left {:?} on the lane",
        stop_lane.reason()
    );
}

// ---------------------------------------------------------------------------
// A Kill whose target has no abort handle yet
// ---------------------------------------------------------------------------

/// A registered actor whose task exists but whose abort handle is not
/// attached yet.
struct Target {
    sys: Arc<ActorSystem>,
    handle: ActorHandle<Idle>,
    id: ActorId,
    name: String,
    seq: u64,
    task: tokio::task::JoinHandle<()>,
}

impl Target {
    fn new(kill_grace: Duration) -> Self {
        let sys = ActorSystem::create(unique("unattached")).unwrap();
        let id = ActorId::from(unique("unattached-id").as_str());
        let name = unique("unattached-name");
        let (tx, _rx) = crate::actor::mailbox::test_channel(1);
        let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, stop_rx) = StopLane::new();
        let registered = ActorHandle::<Idle>::new(
            id.clone(),
            tx,
            stop_lane,
            1,
            status_rx,
            sys.name().into(),
            None,
            Some(Arc::new(AtomicBool::new(false))),
        );
        let seq = sys
            .register_actor::<Idle>(
                &id,
                Some(&Arc::from(name.as_str())),
                &registered,
                true,
                TurnClock::default(),
                kill_grace,
                Handle::current(),
            )
            .unwrap();
        let handle = sys.get_by_id::<Idle>(&id).unwrap();
        let task = tokio::spawn(async move {
            let _owned = (status_tx, stop_rx);
            std::future::pending::<()>().await;
        });
        Self {
            sys,
            handle,
            id,
            name,
            seq,
            task,
        }
    }

    fn attach(&self) {
        self.sys.attach_abort(
            &self.id,
            Some(&self.name),
            self.task.abort_handle(),
            self.seq,
        );
    }

    /// Waits until the task is finished and returns how long that took from
    /// `since`.
    async fn finished_after(&self, since: std::time::Instant) -> Duration {
        tokio::time::timeout(BOUND, async {
            while !self.task.is_finished() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("the target was aborted");
        since.elapsed()
    }

    /// The same wait on a paused clock: the time it took is virtual, the
    /// declared deadline's distance from `since`.
    async fn finished_after_paused(&self, since: tokio::time::Instant) -> Duration {
        tokio::time::timeout(BOUND, async {
            while !self.task.is_finished() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("the target was aborted");
        since.elapsed()
    }
}

/// A `Kill` call whose window is longer than the kill grace is not aborted
/// before its window ends, also when the attach comes between the raise and
/// the arming that follows it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_call_is_not_aborted_before_its_window_when_the_grace_is_shorter() {
    const WINDOW: Duration = Duration::from_millis(800);
    let target = Arc::new(Target::new(Duration::from_millis(50)));
    {
        let attaching = target.clone();
        target
            .sys
            .set_after_kill_raise_hook(move || attaching.attach());
    }
    let began = std::time::Instant::now();
    let handle = target.handle.clone();
    let call = tokio::spawn(async move { handle.stop_within(StopReason::Kill, WINDOW).await });
    let took = target.finished_after(began).await;
    assert!(
        took >= WINDOW - Duration::from_millis(100),
        "aborted at {took:?}, before the window of {WINDOW:?}"
    );
    assert!(took < Duration::from_secs(3), "aborted at {took:?}");
    let _ = call.await;
}

/// A `Kill` that asks for the kill grace while another asks for a longer
/// window decides the abort time: the earliest wins, whichever came first.
/// The clock is paused, so the abort's instant is the declared deadline and
/// not the host's scheduling.
#[tokio::test(start_paused = true)]
async fn the_earliest_kill_deadline_declared_before_the_attach_decides_the_abort() {
    for windowed_first in [true, false] {
        for by_name in [true, false] {
            let target = Arc::new(Target::new(Duration::from_millis(150)));
            let began = tokio::time::Instant::now();
            let windowed = {
                let handle = target.handle.clone();
                async move {
                    let _ = handle
                        .stop_within(StopReason::Kill, Duration::from_secs(3))
                        .await;
                }
            };
            let grace_kill = {
                let (sys, id, name) = (target.sys.clone(), target.id.clone(), target.name.clone());
                async move {
                    if by_name {
                        let _ = sys.kill(&name).await;
                    } else {
                        let _ = sys.kill_by_id(&id).await;
                    }
                }
            };
            type Call = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
            let (windowed, grace_kill): (Call, Call) = (Box::pin(windowed), Box::pin(grace_kill));
            let (first, second) = if windowed_first {
                (windowed, grace_kill)
            } else {
                (grace_kill, windowed)
            };
            let (mut first, mut second) = (first, second);
            let first_done = poll_once(first.as_mut());
            assert_eq!(
                target.handle.stop_lane().reason(),
                Some(StopReason::Kill),
                "the first call raised its Kill"
            );
            let second_done = poll_once(second.as_mut());
            target.attach();
            let took = target.finished_after_paused(began).await;
            assert!(
                took < Duration::from_millis(1500),
                "windowed_first={windowed_first} by_name={by_name}: aborted at {took:?}, not at the grace"
            );
            if !first_done {
                first.await;
            }
            if !second_done {
                second.await;
            }
        }
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

/// Flags the hook sets as the ladder passes its points.
#[derive(Default)]
struct Passed {
    lost: AtomicBool,
    decided: AtomicBool,
}

fn spin_until(flag: &AtomicBool) {
    let end = std::time::Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::SeqCst) && std::time::Instant::now() < end {
        std::thread::yield_now();
    }
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
    {
        let (passed, lane) = (passed.clone(), held.lane.clone());
        held.sys.set_ladder_hook(move |point, _id| match point {
            LadderPoint::StopBeforeRaise if refuse => lane.close_to_non_kill(),
            LadderPoint::StopHelperStart => {
                if lost_at == LostAt::AfterTheDecision {
                    spin_until(&passed.decided);
                }
                panic!("stop task lost");
            }
            LadderPoint::StopHelperLost => passed.lost.store(true, Ordering::SeqCst),
            LadderPoint::StopRaised if lost_at == LostAt::BeforeTheDecision => {
                spin_until(&passed.lost);
            }
            LadderPoint::StopDecided => passed.decided.store(true, Ordering::SeqCst),
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
    // The window and the grace have passed twice over.
    tokio::time::sleep(Duration::from_millis(500)).await;
    held.sys.clear_ladder_hook();
    let mut fed = 0;
    while held.feed.try_recv().is_ok() {
        fed += 1;
    }
    (result, fed, held.lane.reason())
}

/// A stop task lost before the caller decided that its first request was
/// accepted hands nothing over; the caller hands over the steps, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_task_lost_before_an_accepted_request_is_decided_has_its_steps_handed_over_once() {
    let (result, fed, _) = lost_task_call(LostAt::BeforeTheDecision, false).await;
    assert!(result.is_ok(), "the request was accepted: {result:?}");
    assert_eq!(fed, 1, "one hand-off to the reaper");
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
    let target = Arc::new(Target::new(GRACE));
    let fired = Arc::new(AtomicBool::new(false));
    {
        let fired = fired.clone();
        target.sys.set_ladder_hook(move |point, _id| {
            if point == LadderPoint::StopHelperStart && !fired.swap(true, Ordering::SeqCst) {
                panic!("stop task lost");
            }
        });
    }
    let h1 = target.handle.clone();
    let first = tokio::spawn(async move {
        h1.stop_within(StopReason::Graceful, Duration::from_millis(10))
            .await
    });
    // The hand-off raises its Kill when the task is lost. Only the paused
    // clock's bound ends this wait, and it ends it only if nothing else can
    // run, so the Kill was never raised.
    tokio::time::timeout(BOUND, async {
        while target.handle.stop_lane().reason() != Some(StopReason::Kill) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("hand-off raised Kill");
    let raised = tokio::time::Instant::now();
    let h2 = target.handle.clone();
    let mut second = Box::pin(async move { h2.stop_within(StopReason::Kill, LONG_WINDOW).await });
    poll_once(second.as_mut());
    target.attach();
    tokio::time::timeout(BOUND, async {
        while !target.task.is_finished() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the target was aborted");
    let took = raised.elapsed();
    assert!(
        took < LONG_WINDOW / 2,
        "aborted {took:?} after the Kill, grace was {GRACE:?}"
    );
    let _ = first.await;
    let _ = second.await;
}
