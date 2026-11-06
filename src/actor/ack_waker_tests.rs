//! The actor's task answers the ack of its spawn from its own run. The
//! caller's waker belongs to the caller's task: when the ack is a failed
//! initialization and the wake panics, the panic stays in the answer, so the
//! task ends by its own return with the reason the initialization failed
//! with, and not by an unwind past every catch. The same holds when a `Kill`
//! cuts the initialization and the ack is dropped unanswered: the task ends as
//! `Kill`.
//!
//! It holds too when the task's own poll unwinds from a panic of the crate's
//! own code, which drops the start ack, the answer held for a failed request
//! and the sender of the request in hand as part of the unwind. A panic of a
//! caller's waker raised there would be a second panic in an unwind and would
//! end the process, so each sender is released inside a catch, and the task
//! ends with the reason an unwind carries. Those scenarios run in a child
//! process of the test binary, where the process ending fails one test.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use super::bounded_run::{self, Flavor};
use super::own_unwind_tests::{panic_at_the_end_of, panic_in_the_turn_of};
use crate::actor::context::ActorContext;
use crate::actor::runtime::{spawn_actor, ActorConfig, UNCAUGHT_PANIC};
use crate::actor::supervision::DEFAULT_KILL_GRACE;
use crate::actor::Actor;
use crate::error::{ActorError, ActorResult, AskError};
use crate::system::ActorSystem;
use crate::types::{ActorId, StopReason};

/// The longest a wait may take. It only limits how long a real hang takes to
/// report.
const HANG: Duration = Duration::from_secs(60);

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: AtomicUsize,
}

impl Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a waker's wake panics");
        }
    }
}

/// An actor whose initialization says it began, waits for a gate and then
/// fails.
struct FailsInit {
    entered: Mutex<Option<oneshot::Sender<()>>>,
    gate: Mutex<Option<oneshot::Receiver<()>>>,
}

impl FailsInit {
    fn gated() -> (Self, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (gate_tx, gate_rx) = oneshot::channel();
        (
            Self {
                entered: Mutex::new(Some(entered_tx)),
                gate: Mutex::new(Some(gate_rx)),
            },
            entered_rx,
            gate_tx,
        )
    }
}

impl Actor for FailsInit {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let entered = self.entered.lock().unwrap().take();
        if let Some(entered) = entered {
            let _ = entered.send(());
        }
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        Err(ActorError::user("init"))
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// The caller waits on the ack with a waker that panics when woken, and the
/// initialization fails: the task's join reads the failed initialization's
/// reason, not a panic of the task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_init_whose_ack_waker_panics_still_ends_the_task_with_its_reason() {
    let system = ActorSystem::create(format!("ack-waker-{}", uuid::Uuid::new_v4())).unwrap();
    let (ack_tx, ack_rx) = oneshot::channel();
    let (actor, _entered, gate_tx) = FailsInit::gated();
    let name = format!("ack-waker-{}", uuid::Uuid::new_v4());
    let (_handle, join, _subtree) = spawn_actor(
        ActorId::from(name.clone()),
        actor,
        ActorConfig::default(),
        Some(Arc::from(name.as_str())),
        Some(system),
        Some(ack_tx),
        true,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the actor spawns");
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    let mut ack: Pin<Box<_>> = Box::pin(ack_rx);
    assert!(ack
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    loud.armed.store(true, Ordering::SeqCst);

    gate_tx.send(()).unwrap();
    let joined = timeout(HANG, join).await.expect("the task ends");
    loud.armed.store(false, Ordering::SeqCst);

    assert_eq!(
        loud.woken.load(Ordering::SeqCst),
        1,
        "the ack woke the caller"
    );
    assert_eq!(
        joined.expect("a caller's waker unwound the actor's task"),
        StopReason::Failure(ActorError::user("init"))
    );
    assert!(matches!(
        ack.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(Err(err))) if err == ActorError::user("init")
    ));
}

/// A `Kill` reaches the actor while its initialization runs, and the
/// initialization then fails: the ack is dropped unanswered, and the spawn
/// caller's waker panics when it is woken. The panic stays out of the actor's
/// task, which ends by its own return with the `Kill`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_cut_init_whose_ack_waker_panics_still_ends_the_task_as_kill() {
    let system = ActorSystem::create(format!("ack-waker-{}", uuid::Uuid::new_v4())).unwrap();
    let (ack_tx, ack_rx) = oneshot::channel();
    let (actor, entered, gate_tx) = FailsInit::gated();
    let name = format!("ack-waker-{}", uuid::Uuid::new_v4());
    let (handle, join, _subtree) = spawn_actor(
        ActorId::from(name.clone()),
        actor,
        ActorConfig::default(),
        Some(Arc::from(name.as_str())),
        Some(system),
        Some(ack_tx),
        true,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the actor spawns");
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    let mut ack: Pin<Box<_>> = Box::pin(ack_rx);
    assert!(ack
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    timeout(HANG, entered)
        .await
        .expect("the initialization begins")
        .unwrap();
    loud.armed.store(true, Ordering::SeqCst);
    handle
        .stop(StopReason::Kill)
        .await
        .expect("the kill is taken");
    gate_tx.send(()).unwrap();
    let joined = timeout(HANG, join).await.expect("the task ends");
    loud.armed.store(false, Ordering::SeqCst);

    assert_eq!(
        loud.woken.load(Ordering::SeqCst),
        1,
        "the dropped ack woke the caller"
    );
    assert_eq!(
        joined.expect("a caller's waker unwound the actor's task"),
        StopReason::Kill
    );
    assert!(matches!(
        ack.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(_))
    ));
}

/// A `Kill` is raised before the actor's task first runs, so the initialization
/// never starts and the ack is dropped unanswered at the end. The spawn
/// caller's waker panics when it is woken, and the task still ends as `Kill`.
#[tokio::test(flavor = "current_thread")]
async fn a_kill_before_init_whose_ack_waker_panics_still_ends_the_task_as_kill() {
    let system = ActorSystem::create(format!("ack-waker-{}", uuid::Uuid::new_v4())).unwrap();
    let (ack_tx, ack_rx) = oneshot::channel();
    let (actor, _entered, _gate_tx) = FailsInit::gated();
    let name = format!("ack-waker-{}", uuid::Uuid::new_v4());
    let (handle, join, _subtree) = spawn_actor(
        ActorId::from(name.clone()),
        actor,
        ActorConfig::default(),
        Some(Arc::from(name.as_str())),
        Some(system),
        Some(ack_tx),
        true,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the actor spawns");
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    let mut ack: Pin<Box<_>> = Box::pin(ack_rx);
    assert!(ack
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    // The task has not run: nothing on this thread has yielded since the spawn.
    handle
        .stop(StopReason::Kill)
        .await
        .expect("the kill is taken");
    loud.armed.store(true, Ordering::SeqCst);
    let joined = timeout(HANG, join).await.expect("the task ends");
    loud.armed.store(false, Ordering::SeqCst);

    assert_eq!(
        loud.woken.load(Ordering::SeqCst),
        1,
        "the dropped ack woke the caller"
    );
    assert_eq!(
        joined.expect("a caller's waker unwound the actor's task"),
        StopReason::Kill
    );
}

/// The reason a task that unwound from a panic of its own carries.
fn unwound() -> StopReason {
    StopReason::Failure(ActorError::Panic(UNCAUGHT_PANIC.to_string()))
}

/// Runs the test `name` of this module alone in a child process of the test
/// binary; true in the child.
fn in_a_child(name: &str) -> bool {
    crate::actor::panic::in_a_child_process(module_path!(), name)
}

/// An actor whose handler waits for a gate, and then answers or fails the
/// request it is given. The caller's first poll of the request is made while
/// the handler waits, so the caller's waker is registered before the actor can
/// answer.
struct GatedTurn {
    gate: Mutex<Option<oneshot::Receiver<()>>>,
    fails: bool,
}

impl GatedTurn {
    fn new(fails: bool) -> (Self, oneshot::Sender<()>) {
        let (gate_tx, gate_rx) = oneshot::channel();
        (
            Self {
                gate: Mutex::new(Some(gate_rx)),
                fails,
            },
            gate_tx,
        )
    }
}

impl Actor for GatedTurn {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        if self.fails {
            Err(ActorError::user("turn"))
        } else {
            Ok(())
        }
    }
}

/// The task's own poll unwinds at the release of its name, at the end of a
/// failed initialization whose ack is still held. The unwind drops the ack and
/// wakes the spawn caller, whose waker panics: that panic stays in the drop, so
/// the process goes on, the task ends with the reason an unwind carries, and
/// the caller finds the ack closed.
async fn a_start_ack_held_by_an_unwinding_task_is_dropped_without_a_second_panic() {
    let system = ActorSystem::create(format!("ack-waker-{}", uuid::Uuid::new_v4())).unwrap();
    let (ack_tx, ack_rx) = oneshot::channel();
    let (actor, _entered, gate_tx) = FailsInit::gated();
    let name = format!("ack-waker-{}", uuid::Uuid::new_v4());
    panic_at_the_end_of(&name);
    let (handle, join, _subtree) = spawn_actor(
        ActorId::from(name.clone()),
        actor,
        ActorConfig::default(),
        Some(Arc::from(name.as_str())),
        Some(system),
        Some(ack_tx),
        true,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the actor spawns");
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    let mut ack: Pin<Box<_>> = Box::pin(ack_rx);
    assert!(ack
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    loud.armed.store(true, Ordering::SeqCst);

    gate_tx.send(()).unwrap();
    let joined = timeout(HANG, join).await.expect("the task ends");
    loud.armed.store(false, Ordering::SeqCst);

    assert!(
        joined.expect_err("the task unwound").is_panic(),
        "the task ended by the unwind of its own poll"
    );
    assert!(
        loud.woken.load(Ordering::SeqCst) >= 1,
        "the unwind dropped the ack and woke the caller"
    );
    assert_eq!(
        timeout(HANG, handle.wait_stopped())
            .await
            .expect("the plane ends"),
        unwound()
    );
    assert!(matches!(
        ack.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(_))
    ));
}

#[test]
fn a_start_ack_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_current_thread() {
    if !in_a_child(
        "a_start_ack_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_current_thread",
    ) {
        return;
    }
    bounded_run::run(Flavor::CurrentThread, || async {
        a_start_ack_held_by_an_unwinding_task_is_dropped_without_a_second_panic().await;
    });
}

#[test]
fn a_start_ack_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_multi_thread() {
    if !in_a_child(
        "a_start_ack_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_multi_thread",
    ) {
        return;
    }
    bounded_run::run(Flavor::MultiThread, || async {
        a_start_ack_held_by_an_unwinding_task_is_dropped_without_a_second_panic().await;
    });
}

/// The handler fails a request, so its answer is held until the lane is sealed,
/// and the task's own poll unwinds at the release of its name before it is
/// sent. The unwind drops the held answer and wakes the caller, whose waker
/// panics: that panic stays in the drop, and the caller reads the reason the
/// unwind left.
async fn an_answer_held_by_an_unwinding_task_is_dropped_without_a_second_panic() {
    let system = ActorSystem::create(format!("held-waker-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("held-waker-{}", uuid::Uuid::new_v4());
    panic_at_the_end_of(&name);
    let (actor, gate_tx) = GatedTurn::new(true);
    let (handle, join, _subtree) = spawn_actor(
        ActorId::from(name.clone()),
        actor,
        ActorConfig::default(),
        Some(Arc::from(name.as_str())),
        Some(system),
        None,
        true,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the actor spawns");
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    let mut request = Box::pin(handle.send(()));
    assert!(request
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    loud.armed.store(true, Ordering::SeqCst);

    gate_tx.send(()).unwrap();
    let joined = timeout(HANG, join).await.expect("the task ends");
    loud.armed.store(false, Ordering::SeqCst);

    assert!(
        joined.expect_err("the task unwound").is_panic(),
        "the task ended by the unwind of its own poll"
    );
    assert!(
        loud.woken.load(Ordering::SeqCst) >= 1,
        "the unwind dropped the held answer and woke the caller"
    );
    match timeout(HANG, request.as_mut()).await {
        Ok(Err(AskError::ResponseDropped { reason })) => assert_eq!(reason, unwound()),
        other => panic!("the caller reads the unwind's reason, got {other:?}"),
    }
}

#[test]
fn an_answer_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_current_thread() {
    if !in_a_child(
        "an_answer_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_current_thread",
    ) {
        return;
    }
    bounded_run::run(Flavor::CurrentThread, || async {
        an_answer_held_by_an_unwinding_task_is_dropped_without_a_second_panic().await;
    });
}

#[test]
fn an_answer_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_multi_thread() {
    if !in_a_child(
        "an_answer_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_multi_thread",
    ) {
        return;
    }
    bounded_run::run(Flavor::MultiThread, || async {
        an_answer_held_by_an_unwinding_task_is_dropped_without_a_second_panic().await;
    });
}

/// The task's own poll unwinds in the turn of a request, after the handler
/// returned and before the answer is sent, while the request's sender is held.
/// The unwind drops the sender and wakes the caller, whose waker panics: that
/// panic stays in the drop, and the caller reads the reason the unwind left.
async fn a_request_in_hand_of_an_unwinding_task_is_dropped_without_a_second_panic() {
    let system = ActorSystem::create(format!("turn-waker-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("turn-waker-{}", uuid::Uuid::new_v4());
    panic_in_the_turn_of(&name);
    let (actor, gate_tx) = GatedTurn::new(false);
    let (handle, join, _subtree) = spawn_actor(
        ActorId::from(name.clone()),
        actor,
        ActorConfig::default(),
        Some(Arc::from(name.as_str())),
        Some(system),
        None,
        true,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the actor spawns");
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    let mut request = Box::pin(handle.send(()));
    assert!(request
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    loud.armed.store(true, Ordering::SeqCst);

    gate_tx.send(()).unwrap();
    let joined = timeout(HANG, join).await.expect("the task ends");
    loud.armed.store(false, Ordering::SeqCst);

    assert!(
        joined.expect_err("the task unwound").is_panic(),
        "the task ended by the unwind of its own poll"
    );
    assert!(
        loud.woken.load(Ordering::SeqCst) >= 1,
        "the unwind dropped the request's sender and woke the caller"
    );
    match timeout(HANG, request.as_mut()).await {
        Ok(Err(AskError::ResponseDropped { reason })) => assert_eq!(reason, unwound()),
        other => panic!("the caller reads the unwind's reason, got {other:?}"),
    }
}

#[test]
fn a_request_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_in_its_turn_current_thread(
) {
    if !in_a_child(
        "a_request_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_in_its_turn_current_thread",
    ) {
        return;
    }
    bounded_run::run(Flavor::CurrentThread, || async {
        a_request_in_hand_of_an_unwinding_task_is_dropped_without_a_second_panic().await;
    });
}

#[test]
fn a_request_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_in_its_turn_multi_thread(
) {
    if !in_a_child(
        "a_request_whose_waker_panics_does_not_end_the_process_when_the_task_unwinds_in_its_turn_multi_thread",
    ) {
        return;
    }
    bounded_run::run(Flavor::MultiThread, || async {
        a_request_in_hand_of_an_unwinding_task_is_dropped_without_a_second_panic().await;
    });
}
