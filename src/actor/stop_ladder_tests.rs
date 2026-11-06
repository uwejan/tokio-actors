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

/// Scheduling rounds of a one-thread runtime that an awaited step is given.
const ROUNDS: usize = 1000;

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

/// A stop task lost at its start leaves its two steps to the reaper entry the
/// loss hands over: the vetoing actor gets its `Kill` at the end of the
/// window, counted from the call, and not before; the call reads `Killed`,
/// the step the entry took.
#[tokio::test(start_paused = true)]
async fn a_task_lost_at_its_start_is_carried_on_at_the_end_of_the_window() {
    const WINDOW: Duration = Duration::from_secs(2);
    let sys = ActorSystem::create(unique("lost-at-start")).unwrap();
    let h = Veto
        .spawn()
        .on_system(&sys)
        .kill_grace(Duration::from_millis(200))
        .await
        .unwrap();
    lose_the_task_at_its_start(&sys);
    let started = tokio::time::Instant::now();
    let outcome = h
        .stop_within(StopReason::Graceful, WINDOW)
        .await
        .expect("the request is accepted");
    sys.clear_ladder_hook();
    let took = started.elapsed();
    assert_eq!(outcome, StopOutcome::Killed);
    assert!(
        took >= WINDOW && took < WINDOW + Duration::from_millis(200),
        "the Kill came at the end of the window, took {took:?}"
    );
    assert_eq!(h.wait_stopped().await, StopReason::Kill);
    let rt = Handle::current();
    tokio::time::timeout(BOUND, async {
        while !sys
            .helper_ends(&rt)
            .contains(&(HelperKind::StopLadder, HelperEnd::Panicked))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the loss was recorded");
}

/// The same loss at the 30-year cap: the closed-answer wait is built term by
/// term, so it still reaches the reaper entry's `Kill` at the cap and the
/// call reads `Killed` where a bound summed before it is capped would end the
/// wait at the cap itself.
#[tokio::test(start_paused = true)]
async fn a_lost_task_at_the_cap_still_reads_the_kill_of_the_cap() {
    let sys = ActorSystem::create(unique("lost-at-cap")).unwrap();
    let h = Veto
        .spawn()
        .on_system(&sys)
        .kill_grace(Duration::from_secs(5))
        .await
        .unwrap();
    lose_the_task_at_its_start(&sys);
    let outcome = h
        .stop_within(StopReason::Graceful, Duration::MAX)
        .await
        .expect("the request is accepted");
    sys.clear_ladder_hook();
    assert_eq!(outcome, StopOutcome::Killed);
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

/// The same loss with the window still open: the target's runtime cannot hold
/// the entry, so no `Kill` is raised for it, the vetoing actor keeps running
/// and the call reads `Unresponsive` at the ladder's own bound. The caller's
/// clock is paused, so the window is open at the loss however long the host
/// takes to start the stop task.
#[tokio::test(start_paused = true)]
async fn a_lost_task_whose_window_is_open_raises_nothing_for_a_target_without_timers() {
    let sys = ActorSystem::create(unique("lost-open-no-timers")).unwrap();
    let target_rt = runtime_without_timers();
    let h = veto_on(&target_rt, &sys, Duration::from_millis(200)).await;
    lose_the_task_at_its_start(&sys);
    let outcome = tokio::time::timeout(
        BOUND,
        h.stop_within(StopReason::Graceful, Duration::from_millis(300)),
    )
    .await
    .expect("the call returns")
    .expect("the request is accepted");
    sys.clear_ladder_hook();
    assert_eq!(outcome, StopOutcome::Unresponsive);
    assert_eq!(
        h.stop_lane().reason(),
        Some(StopReason::Graceful),
        "no Kill was raised for the target"
    );
    target_rt.shutdown_background();
}

/// The loss action runs before the answer channel closes: the caller finds the
/// answer closed only after the steps are handed over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_loss_is_handled_before_the_answer_closes() {
    let sys = ActorSystem::create(unique("lost-ordering")).unwrap();
    let h = Veto
        .spawn()
        .on_system(&sys)
        .kill_grace(Duration::from_millis(100))
        .await
        .unwrap();
    let closed_seen = Arc::new(AtomicBool::new(false));
    let closed_during_loss = Arc::new(AtomicBool::new(false));
    {
        let (closed_seen, closed_during_loss) = (closed_seen.clone(), closed_during_loss.clone());
        sys.set_ladder_hook(move |point, _id| match point {
            LadderPoint::StopHelperStart => panic!("stop task lost"),
            LadderPoint::StopHelperLost => {
                // Gives a caller that can see the closed answer the time to
                // say so. The window of the call ends long after this.
                let until = std::time::Instant::now() + Duration::from_millis(100);
                while std::time::Instant::now() < until && !closed_seen.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(2));
                }
                closed_during_loss.store(closed_seen.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            LadderPoint::StopClosedSeen => closed_seen.store(true, Ordering::SeqCst),
            _ => {}
        });
    }
    let outcome = tokio::time::timeout(
        BOUND,
        h.stop_within(StopReason::Graceful, Duration::from_millis(500)),
    )
    .await
    .expect("the call returns")
    .expect("the request is accepted");
    sys.clear_ladder_hook();
    assert_eq!(outcome, StopOutcome::Killed);
    assert!(
        closed_seen.load(Ordering::SeqCst),
        "the caller saw the closed answer"
    );
    assert!(
        !closed_during_loss.load(Ordering::SeqCst),
        "the answer closed while the loss was still being handled"
    );
}

/// The caller that finds the answer closed does not wait for ever: an actor
/// that never yields is reported `Unresponsive` once the call's own ladder
/// bound has passed, and it is not given up on. The caller runs on a runtime
/// with a paused clock, so the ladder's bound is virtual time and the outer
/// bound on the call elapses only if the call never returns, whatever the host
/// does with the threads. The target's thread is blocked for real.
#[test]
fn a_lost_task_whose_actor_never_yields_is_reported_unresponsive_at_the_ladder_bound() {
    struct Blocker {
        entered: Option<std::sync::mpsc::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Actor for Blocker {
        type Message = ();
        type Response = ();

        async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            let _ = self.release.recv();
            Ok(())
        }
    }

    let target_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let caller_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let sys = ActorSystem::create(unique("lost-unresponsive")).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let h = target_rt.block_on(async {
        let h = Blocker {
            entered: Some(entered_tx),
            release: release_rx,
        }
        .spawn()
        .on_system(&sys)
        .kill_grace(Duration::from_millis(100))
        .await
        .unwrap();
        h.notify(()).await.unwrap();
        h
    });
    // The handler is entered once the actor sent this; the wait has no clock.
    entered_rx.recv().expect("the handler is entered");
    lose_the_task_at_its_start(&sys);
    let outcome = caller_rt.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(5),
            h.stop_within(StopReason::Graceful, Duration::from_millis(50)),
        )
        .await
    });
    sys.clear_ladder_hook();
    release_tx.send(()).unwrap();
    assert_eq!(
        outcome.expect("the call returns by its own bound").unwrap(),
        StopOutcome::Unresponsive
    );
    // Ends once the handler returns; the harness bounds a hang.
    target_rt.block_on(h.wait_stopped());
}

/// The caller that sees the actor end before the stop task answers reads the
/// same outcome the task reads: the step cell holds `Killed` from the raise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_end_seen_by_the_caller_first_reads_as_the_end_seen_by_the_task() {
    let returned = Arc::new(AtomicBool::new(false));
    for hold_the_task in [true, false] {
        let sys = ActorSystem::create(unique("race")).unwrap();
        let h = Veto
            .spawn()
            .on_system(&sys)
            .kill_grace(Duration::from_millis(200))
            .await
            .unwrap();
        returned.store(false, Ordering::SeqCst);
        if hold_the_task {
            // The task is held between its raise and its classification
            // until the caller has returned, so the caller decides alone.
            let returned = returned.clone();
            sys.set_ladder_hook(move |point, _id| {
                if point == LadderPoint::StopAfterKill {
                    let until = std::time::Instant::now() + Duration::from_secs(5);
                    while std::time::Instant::now() < until && !returned.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            });
        }
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            BOUND,
            h.stop_within(StopReason::Graceful, Duration::from_millis(30)),
        )
        .await
        .expect("the call returns")
        .expect("the request is accepted");
        let took = started.elapsed();
        returned.store(true, Ordering::SeqCst);
        sys.clear_ladder_hook();
        assert_eq!(outcome, StopOutcome::Killed, "task held: {hold_the_task}");
        assert!(
            took < Duration::from_secs(4),
            "the caller decided before the held task was let go, took {took:?}"
        );
    }
}

/// A panic after the task has answered takes no step: the ladder had run to
/// its end, and nothing is handed to the reaper beyond the one entry the
/// ladder's own `Kill` armed. The caller runs on a runtime with a paused
/// clock, so the ladder's steps are virtual time and the stop task's end is
/// awaited in rounds of that one thread.
#[test]
fn a_panic_after_the_answer_takes_no_step() {
    struct Blocker {
        entered: Option<std::sync::mpsc::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Actor for Blocker {
        type Message = ();
        type Response = ();

        async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            let _ = self.release.recv();
            Ok(())
        }
    }

    let target_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let caller_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let sys = ActorSystem::create(unique("panic-after-answer")).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let h = target_rt.block_on(async {
        let h = Blocker {
            entered: Some(entered_tx),
            release: release_rx,
        }
        .spawn()
        .on_system(&sys)
        .kill_grace(Duration::from_millis(100))
        .await
        .unwrap();
        h.notify(()).await.unwrap();
        h
    });
    entered_rx.recv().expect("the handler is entered");
    let mut entries = sys.capture_reaper(target_rt.handle());
    let answered = Arc::new(AtomicBool::new(false));
    {
        let answered = answered.clone();
        sys.set_ladder_hook(move |point, _id| {
            if point == LadderPoint::StopAnswered {
                answered.store(true, Ordering::SeqCst);
                panic!("stop task lost after its answer");
            }
        });
    }
    let outcome = caller_rt
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                h.stop_within(StopReason::Graceful, Duration::from_millis(50)),
            )
            .await
        })
        .expect("the call returns")
        .unwrap();
    assert_eq!(outcome, StopOutcome::Unresponsive);
    // The stop task runs on the caller's one thread, so the rounds that it
    // needs to reach its hook and to record its panic are counted, not timed.
    let rt = caller_rt.handle().clone();
    caller_rt.block_on(async {
        for _ in 0..ROUNDS {
            if answered.load(Ordering::SeqCst)
                && sys
                    .helper_ends(&rt)
                    .contains(&(HelperKind::StopLadder, HelperEnd::Panicked))
            {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the hook ran and its panic was recorded");
    });
    sys.clear_ladder_hook();
    let mut fed = 0;
    while entries.try_recv().is_ok() {
        fed += 1;
    }
    release_tx.send(()).unwrap();
    assert_eq!(fed, 1, "only the ladder's own Kill armed an entry");
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

/// The step that raises the `Kill` stores its cell before the raise. A caller
/// that sees the actor end inside that window waits for the step to settle:
/// the lane of an actor that ended by itself refuses the `Kill`, so the call
/// reads the plain request, as the task does, and never `Killed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_that_sees_the_end_inside_the_kill_step_reads_the_settled_step() {
    let sys = ActorSystem::create(unique("kill-pending")).unwrap();
    let h = Quitter
        .spawn()
        .on_system(&sys)
        .kill_grace(Duration::from_millis(200))
        .await
        .unwrap();
    let (stored_tx, stored_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (stored_tx, release_rx) = (
        std::sync::Mutex::new(stored_tx),
        std::sync::Mutex::new(release_rx),
    );
    sys.set_ladder_hook(move |point, _id| {
        if point == LadderPoint::StopKillStored {
            let _ = stored_tx.lock().unwrap().send(());
            let _ = release_rx.lock().unwrap().recv_timeout(BOUND);
        }
    });
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut call = Box::pin(h.stop_within(StopReason::Graceful, Duration::from_millis(30)));
    assert!(std::future::Future::poll(call.as_mut(), &mut cx).is_pending());
    stored_rx
        .recv_timeout(BOUND)
        .expect("the stop task stored its step");
    // The actor ends by itself while the step is between its store and its
    // raise.
    h.notify(()).await.unwrap();
    h.wait_stopped().await;
    assert!(
        std::future::Future::poll(call.as_mut(), &mut cx).is_pending(),
        "the caller read a step that was still being taken"
    );
    release_tx.send(()).unwrap();
    let outcome = tokio::time::timeout(BOUND, call)
        .await
        .expect("the call returns")
        .expect("the request is accepted");
    sys.clear_ladder_hook();
    assert_eq!(outcome, StopOutcome::Graceful);
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

/// A first request the lane refuses is an error of the call and owes no
/// step: the stop task spawned for it ends without raising a `Kill`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_first_request_leaves_no_ladder_behind() {
    let sys = ActorSystem::create(unique("refused-first")).unwrap();
    let id = ActorId::from("refused-first-id");
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
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
        Duration::from_millis(50),
        Handle::current(),
    )
    .unwrap();
    let kills = Arc::new(AtomicBool::new(false));
    {
        let kills = kills.clone();
        let lane = stop_lane.clone();
        sys.set_ladder_hook(move |point, _id| match point {
            // The lane is sealed after the call looked at it and its stop
            // task was spawned, and before the call raises: the first
            // request is refused.
            LadderPoint::StopBeforeRaise => {
                lane.seal();
            }
            LadderPoint::StopWithinKill => kills.store(true, Ordering::SeqCst),
            _ => {}
        });
    }
    // Inside a task, so the stop task is spawned before the request is raised.
    let refused = tokio::spawn({
        let registered = registered.clone();
        async move {
            registered
                .stop_within(StopReason::Graceful, Duration::from_millis(30))
                .await
        }
    })
    .await
    .unwrap();
    assert!(matches!(refused, Err(SendError::Closed)));
    until("the stop task ended", || sys.live_helpers() == 0).await;
    // Past the window and the kill grace of the call.
    tokio::time::sleep(Duration::from_millis(200)).await;
    sys.clear_ladder_hook();
    assert!(
        !kills.load(Ordering::SeqCst),
        "a refused call took the Kill step"
    );
    assert_eq!(
        sys.helper_ends(&Handle::current()),
        vec![(HelperKind::StopLadder, HelperEnd::Finished)]
    );
    drop(status_tx);
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

/// A call that is kept without being polled again holds its stop task no
/// longer than its window, whether or not its target has ended by then. The
/// clock is paused, so the window ends in virtual time and the bound on the
/// wait elapses only if the task is never released.
#[tokio::test(start_paused = true)]
async fn a_kept_call_releases_its_stop_task_when_its_window_ends() {
    let sys = ActorSystem::create(unique("kept")).unwrap();
    let h = Idle.spawn().on_system(&sys).await.unwrap();
    let mut call = Box::pin(h.stop_within(StopReason::Graceful, Duration::from_millis(200)));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let _ = std::future::Future::poll(call.as_mut(), &mut cx);
    h.wait_stopped().await;
    until("the stop task ended with the window", || {
        sys.live_helpers() == 0
    })
    .await;
    drop(call);
}

/// A call made from outside every task raises its request first and spawns
/// its stop task after; it stops the actor and gives the task back as a call
/// made inside a task does.
#[test]
fn a_call_made_outside_every_task_stops_the_actor_and_gives_its_task_back() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let sys = ActorSystem::create(unique("outside")).unwrap();
    let h = rt
        .block_on(async { Idle.spawn().on_system(&sys).await })
        .unwrap();
    assert!(tokio::task::try_id().is_none());
    let outcome = rt
        .block_on(h.stop_within(StopReason::Graceful, Duration::from_secs(3600)))
        .expect("the request is accepted");
    assert_eq!(outcome, StopOutcome::Graceful);
    rt.block_on(until("the stop task ended", || sys.live_helpers() == 0));
    assert_eq!(
        sys.helper_ends(rt.handle()),
        vec![(HelperKind::StopLadder, HelperEnd::Finished)]
    );
}

// ---------------------------------------------------------------------------
// The abort after the window of a bounded Kill
// ---------------------------------------------------------------------------

/// Handles a message by waiting on a future that never completes: a wedge
/// an abort cuts at its `.await`, though a `Kill` is never read in it.
struct Hang;

impl Actor for Hang {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// The abort that follows the window of a bounded `Kill` is the system's
/// backstop, armed at the raise: the caller only waits and classifies. With
/// the backstop held back, nothing else aborts the task, and the call reads
/// `Unresponsive` once the settle time has passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bounded_kill_leaves_the_abort_to_the_backstop_the_system_armed() {
    let sys = ActorSystem::create(unique("kill-backstop")).unwrap();
    let h = Hang.spawn().on_system(&sys).await.unwrap();
    h.notify(()).await.unwrap();
    // The loop calls `handle` in the poll that takes the message, so an empty
    // mailbox means the actor is inside its hung turn.
    until("the actor to take the message", || h.mailbox_len() == 0).await;
    let mut held_back = sys.capture_reaper(&Handle::current());
    let outcome = h
        .stop_within(StopReason::Kill, Duration::from_millis(30))
        .await
        .unwrap();
    assert_eq!(outcome, StopOutcome::Unresponsive);
    assert_eq!(
        h.status(),
        ActorStatus::Running,
        "the caller aborted nothing"
    );
    let entry = held_back.try_recv().expect("the system armed its backstop");
    // The entry is the abort: handed to the real reaper it ends the task.
    drop(entry);
    h.wait_stopped().await;
}

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

/// A `Kill` call whose target attaches its abort handle inside the call's
/// window is aborted at the end of the window, not at the longer kill grace.
/// The clock is paused, so no time passes between the call's raise and the
/// attach, however long the host takes, and the abort's instant is the
/// declared deadline.
#[tokio::test(start_paused = true)]
async fn a_kill_call_aborts_at_its_window_when_the_attach_comes_inside_it() {
    let sys = ActorSystem::create(format!("kill-attach-{}", uuid::Uuid::new_v4())).unwrap();
    let id = ActorId::from("kill-attach-id");
    let name = "kill-attach-name";
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
    let seq = sys
        .register_actor::<Idle>(
            &id,
            Some(&Arc::from(name)),
            &registered,
            true,
            TurnClock::default(),
            Duration::from_secs(3),
            Handle::current(),
        )
        .unwrap();
    let handle = sys.get_by_id::<Idle>(&id).unwrap();
    let began = tokio::time::Instant::now();
    let mut call = Box::pin(async move {
        handle
            .stop_within(StopReason::Kill, Duration::from_millis(200))
            .await
    });
    poll_once(call.as_mut());
    assert_eq!(
        stop_lane.reason(),
        Some(StopReason::Kill),
        "the call raised its Kill before the attach"
    );
    let task = tokio::spawn(async move {
        let _owned = (status_tx, stop_rx);
        std::future::pending::<()>().await;
    });
    sys.attach_abort(&id, Some(name), task.abort_handle(), seq);
    let out = tokio::time::timeout(BOUND, call).await.unwrap();
    // The abort cancels the target on the runtime's next turn, which a
    // current-thread runtime takes only when this test yields, so the end is
    // awaited, never read as a flag at the instant the call returns.
    let ended = tokio::time::timeout(BOUND, task)
        .await
        .unwrap_or_else(|_| panic!("the call returned {out:?} and the target never ended"))
        .expect_err("the target ends by abort");
    let took = began.elapsed();
    assert!(ended.is_cancelled(), "the target ended some other way");
    assert!(
        took >= Duration::from_millis(200),
        "the abort came before the window ended: {took:?}"
    );
    assert!(
        took < Duration::from_millis(1500),
        "the abort waited for the grace: {took:?}"
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

/// A `Kill` call with a window of zero is aborted when the attach comes, not
/// after the kill grace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_call_with_a_zero_window_aborts_at_the_attach() {
    let target = Target::new(Duration::from_secs(3));
    let handle = target.handle.clone();
    let mut call =
        Box::pin(async move { handle.stop_within(StopReason::Kill, Duration::ZERO).await });
    poll_once(call.as_mut());
    assert_eq!(
        target.handle.stop_lane().reason(),
        Some(StopReason::Kill),
        "the call raised its Kill before the attach"
    );
    let attached = std::time::Instant::now();
    target.attach();
    let took = target.finished_after(attached).await;
    assert!(
        took < Duration::from_millis(1000),
        "aborted {took:?} after the attach"
    );
    let _ = call.await;
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

/// A stop task lost after the decision hands its steps over itself, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_task_lost_after_an_accepted_request_is_decided_hands_its_steps_over_once() {
    let (result, fed, _) = lost_task_call(LostAt::AfterTheDecision, false).await;
    assert!(result.is_ok(), "the request was accepted: {result:?}");
    assert_eq!(fed, 1, "one hand-off to the reaper");
}

/// A stop task lost before a refused request is decided leaves no step
/// behind: nothing is handed to the reaper and no `Kill` is raised.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_task_lost_before_a_refused_request_is_decided_leaves_no_step() {
    let (result, fed, reason) = lost_task_call(LostAt::BeforeTheDecision, true).await;
    assert!(result.is_err(), "the lane refused the request: {result:?}");
    assert_eq!(fed, 0, "nothing was handed to the reaper");
    assert_eq!(reason, None, "no Kill was raised");
}

/// A stop task lost after a refused request was decided leaves no step
/// behind either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_task_lost_after_a_refused_request_is_decided_leaves_no_step() {
    let (result, fed, reason) = lost_task_call(LostAt::AfterTheDecision, true).await;
    assert!(result.is_err(), "the lane refused the request: {result:?}");
    assert_eq!(fed, 0, "nothing was handed to the reaper");
    assert_eq!(reason, None, "no Kill was raised");
}

/// A call that unwinds between the stop task's spawn and the decision counts
/// as refused: its task takes no step. The caller is a thread outside the
/// runtime's workers, so the stop task runs while the caller waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_that_unwinds_before_the_decision_leaves_no_step() {
    let held = held(Duration::from_millis(100));
    held.sys.set_ladder_hook(move |point, _id| {
        if point == LadderPoint::StopBeforeRaise {
            // Gives the stop task time to pass its window.
            std::thread::sleep(Duration::from_millis(200));
            panic!("the call unwinds");
        }
    });
    let handle = held.handle.clone();
    let rt = Handle::current();
    let result = tokio::task::spawn_blocking(move || {
        rt.block_on(handle.stop_within(StopReason::Graceful, Duration::ZERO))
    })
    .await;
    held.sys.clear_ladder_hook();
    assert!(result.is_err(), "the call panicked");
    // The stop task ends without a step: a call that never decided leaves
    // no task waiting for a decision.
    let rt = Handle::current();
    tokio::time::timeout(BOUND, async {
        while !held
            .sys
            .helper_ends(&rt)
            .contains(&(HelperKind::StopLadder, HelperEnd::Finished))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the stop task ended");
    assert_eq!(held.lane.reason(), None, "no Kill was raised");
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
