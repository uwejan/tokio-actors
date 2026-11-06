//! What a bounded stop holds: a pending call with a reason other than `Kill`
//! holds a stop task of the system and a `Kill` request holds none, and a
//! call whose target's system is not held by the process map of named systems
//! runs its ladder in the caller's own future, raises its `Kill` and aborts
//! nothing.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

use super::*;
use crate::actor::context::ActorContext;
use crate::error::ActorResult;
use crate::system::ActorSystem;
use crate::types::{
    ActorId, ActorStatus, StatusTx, StatusValue, StopLane, StopOutcome, StopReason, TurnClock,
};

#[derive(Default)]
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Registers a handle by hand in `sys`, with no abort handle and nothing
/// running behind it: the registry entry exists, the actor's task does not.
/// The channel ends behind the handle are returned so they stay open.
fn registered_handle(sys: &Arc<ActorSystem>, id: &str) -> (ActorHandle<Idle>, impl Sized) {
    let id = ActorId::from(id);
    let (tx, rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, stop_rx) = StopLane::new();
    let handle = ActorHandle::<Idle>::new(
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
        &handle,
        true,
        TurnClock::default(),
        Duration::from_millis(100),
        Handle::current(),
    )
    .unwrap();
    (handle, (rx, status_tx, stop_rx))
}

/// A pending call with a reason other than `Kill` spawns its stop task, which
/// the helper count shows; a pending `Kill` call on another registered actor
/// of a system of its own spawns none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_kill_call_holds_no_stop_task() {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());

    let graceful_sys =
        ActorSystem::create(format!("graceful-call-holds-{}", uuid::Uuid::new_v4())).unwrap();
    let (graceful_target, graceful_status) = registered_handle(&graceful_sys, "graceful-target");
    let mut graceful =
        Box::pin(graceful_target.stop_within(StopReason::Graceful, Duration::from_secs(3600)));
    assert!(std::future::Future::poll(graceful.as_mut(), &mut cx).is_pending());
    assert_eq!(
        graceful_sys.live_helpers(),
        1,
        "a pending call with another reason holds its stop task"
    );

    let kill_sys =
        ActorSystem::create(format!("kill-call-holds-{}", uuid::Uuid::new_v4())).unwrap();
    let (kill_target, kill_status) = registered_handle(&kill_sys, "kill-target");
    let mut kill = Box::pin(kill_target.stop_within(StopReason::Kill, Duration::from_secs(3600)));
    assert!(std::future::Future::poll(kill.as_mut(), &mut cx).is_pending());
    assert_eq!(
        kill_sys.live_helpers(),
        0,
        "a pending Kill call spawned a stop task"
    );

    drop(kill);
    drop(graceful);
    drop(kill_status);
    drop(graceful_status);
}

/// The target's system is not in the process map: there is no system to own
/// the steps, so the ladder runs in the call's own future. It raises its
/// `Kill` and, with no registry entry to hold an abort handle, aborts
/// nothing, so a task that never reacts is answered `Unresponsive`.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_bounded_stop_with_no_system_in_the_map_runs_in_the_caller_and_aborts_nothing() {
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, stop_rx) = StopLane::new();
    let name = format!("no-such-system-{}", uuid::Uuid::new_v4());
    let ghost = ActorHandle::<Idle>::new(
        ActorId::from("ghost"),
        tx,
        stop_lane.clone(),
        1,
        status_rx,
        name.as_str().into(),
        None,
        None,
    );
    assert!(ActorSystem::get_named(&name).is_none());
    let owner = tokio::spawn(async move {
        let _keep = (status_tx, stop_rx);
        std::future::pending::<()>().await;
    });

    let outcome = ghost
        .stop_within(StopReason::Graceful, Duration::from_millis(30))
        .await
        .unwrap();
    assert_eq!(outcome, StopOutcome::Unresponsive);
    assert_eq!(
        stop_lane.reason(),
        Some(StopReason::Kill),
        "the ladder in the caller raised its Kill"
    );
    assert!(!owner.is_finished(), "nothing aborted the task");

    // The ladder lives in the future: a call dropped before its window ends
    // takes no later step.
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (second_lane, second_stop_rx) = StopLane::new();
    let second = ActorHandle::<Idle>::new(
        ActorId::from("ghost-two"),
        tx,
        second_lane.clone(),
        1,
        status_rx,
        name.as_str().into(),
        None,
        None,
    );
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut call = Box::pin(second.stop_within(StopReason::Graceful, Duration::from_secs(60)));
    assert!(std::future::Future::poll(call.as_mut(), &mut cx).is_pending());
    drop(call);
    tokio::time::advance(Duration::from_secs(3600)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        second_lane.reason(),
        Some(StopReason::Graceful),
        "a dropped call took a later step"
    );
    drop((status_tx, second_stop_rx));
    owner.abort();
}

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: std::sync::atomic::AtomicUsize,
}

impl std::task::Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a waker's wake panics");
        }
    }
}

/// The stop task answers its caller when its ladder is over. The caller's
/// waker panics when it is woken: the panic stays out of the task, which has
/// done what it owed, and the caller reads the outcome on its next poll.
#[test]
fn a_stop_caller_whose_waker_panics_when_the_task_answers_does_not_unwind_into_the_task() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    let (_status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (lane, _lane_rx) = StopLane::new();
    let (answer_tx, answer_rx) = oneshot::channel();
    let mut task = StopTask::new(
        None,
        ActorId::from("loud-stop-answer"),
        lane,
        status_rx,
        Arc::new(StopStep::accepted()),
        (Instant::now(), Duration::ZERO, Duration::ZERO),
        Some(answer_tx),
    );
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    let mut waiting = Box::pin(answer_rx);
    assert!(waiting
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    loud.armed.store(true, Ordering::SeqCst);

    let answered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        task.answer(StopOutcome::Graceful)
    }));

    assert!(
        answered.is_ok(),
        "the caller's waker unwound out of the task"
    );
    assert_eq!(
        loud.woken.load(Ordering::SeqCst),
        1,
        "the caller was woken once"
    );
    assert!(task.done, "the task is done");
    assert!(matches!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(StopOutcome::Graceful))
    ));
}
