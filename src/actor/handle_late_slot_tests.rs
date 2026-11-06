//! A value pushed through a slot reserved before the queue ended is dropped by
//! the crate when the slot is used or given back, outside the end order, and
//! `wait_stopped` does not wait for it. No step is ordered by a sleep: the
//! actor's end is observed by awaiting its task handle, which completes only
//! after every drop of the task (an abort included). A timeout only detects a
//! failure.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use super::*;
use crate::actor::context::ActorContext;
use crate::actor::runtime::{self, ActorConfig};
use crate::error::{ActorResult, AskError};
use crate::helper::{HelperEnd, HelperKind};
use crate::system::ActorSystem;
use crate::types::{ActorId, Envelope, StatusRequest, StopReason, SystemMessage};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
const BOUND: Duration = Duration::from_secs(5);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

thread_local! {
    static BEFORE_PUSH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs the hook the calling test installed, once, between a request's slot
/// reservation and its push.
pub(super) fn between_reserve_and_push() {
    if let Some(hook) = BEFORE_PUSH.with(|hook| hook.borrow_mut().take()) {
        hook();
    }
}

type QueueEndsHook = Box<dyn FnOnce() + Send>;

static BETWEEN_QUEUE_ENDS: Mutex<Vec<(ActorId, QueueEndsHook)>> = Mutex::new(Vec::new());

/// Runs the hook a test installed for `owner`, once, when the first of the
/// actor's queues has ended: in the window before the second one does.
pub(crate) fn between_queue_ends(owner: &ActorId) {
    let hook = {
        let mut hooks = BETWEEN_QUEUE_ENDS.lock().unwrap();
        hooks
            .iter()
            .position(|(id, _)| id == owner)
            .map(|at| hooks.remove(at).1)
    };
    if let Some(hook) = hook {
        hook();
    }
}

struct Park {
    entered: Mutex<Option<oneshot::Sender<()>>>,
}

#[derive(Debug)]
enum Msg {
    Park,
    /// Holds the actor's loop until the sender of the channel fires.
    Hold(oneshot::Receiver<()>),
    Ping,
    Bomb(Bomb),
}

#[derive(Debug)]
struct Bomb;

impl Drop for Bomb {
    fn drop(&mut self) {
        panic!("late value drop");
    }
}

impl Actor for Park {
    type Message = Msg;
    type Response = ();
    async fn handle(&mut self, message: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match message {
            Msg::Park => {
                if let Some(entered) = self.entered.lock().unwrap().take() {
                    let _ = entered.send(());
                }
                std::future::pending::<()>().await;
            }
            Msg::Hold(release) => {
                if let Some(entered) = self.entered.lock().unwrap().take() {
                    let _ = entered.send(());
                }
                let _ = release.await;
            }
            _ => {}
        }
        Ok(())
    }
}

type Spawned = (
    ActorHandle<Park>,
    tokio::task::JoinHandle<StopReason>,
    oneshot::Receiver<()>,
);

fn spawn_park() -> Spawned {
    spawn_park_in(None)
}

fn spawn_park_in(system: Option<Arc<ActorSystem>>) -> Spawned {
    let (entered_tx, entered_rx) = oneshot::channel();
    let id = format!("late-slot-{}", NEXT_ID.fetch_add(1, Ordering::SeqCst));
    let (handle, join, _subtree) = runtime::spawn_actor(
        id.into(),
        Park {
            entered: Mutex::new(Some(entered_tx)),
        },
        ActorConfig::default().with_priority_lane(4),
        None,
        system,
        None,
        true,
        Duration::ZERO,
        None,
    )
    .unwrap();
    (handle, join, entered_rx)
}

#[derive(Clone, Copy, Debug)]
enum Lane {
    System,
    Mailbox,
    Priority,
}

/// Keeps the actor's loop busy until the returned sender fires, so that the
/// loop takes no request from its system plane meanwhile. Returns once the
/// loop is busy.
async fn hold_loop(
    handle: &ActorHandle<Park>,
    entered: oneshot::Receiver<()>,
) -> oneshot::Sender<()> {
    let (release_tx, release_rx) = oneshot::channel();
    handle.notify(Msg::Hold(release_rx)).await.unwrap();
    entered.await.unwrap();
    release_tx
}

/// Opens the actor's system channel with two status requests, then reserves a
/// slot on it through a sender of its own, which the permit keeps alive. The
/// actor's loop must be busy, or it takes the first request before the second
/// arrives and the channel never opens.
async fn reserve_system(
    handle: &ActorHandle<Park>,
) -> tokio::sync::mpsc::OwnedPermit<SystemMessage> {
    let record = handle.stop_lane().abort_record().clone();
    for _ in 0..2 {
        let (reply_tx, _reply_rx) = oneshot::channel();
        record.submit_status(reply_tx).await.unwrap();
    }
    let tx = record.system_tx_for_test().expect("the channel is open");
    tx.reserve_owned().await.unwrap()
}

/// Ends the actor and returns once its task is gone (all its drops done).
async fn end(
    handle: &ActorHandle<Park>,
    abort: bool,
    join: &mut tokio::task::JoinHandle<StopReason>,
) {
    let reason = if abort {
        StopReason::Kill
    } else {
        StopReason::Graceful
    };
    handle.stop(reason).await.unwrap();
    assert!(
        timeout(BOUND, join).await.is_ok(),
        "the task must end while the slot is held"
    );
}

/// Reserves a slot on `lane`, ends the actor, then pushes a value through the
/// slot; true when the crate dropped that value.
async fn late_value_dropped(lane: Lane, abort: bool) -> bool {
    let (handle, mut join, entered) = spawn_park();
    let mut release = None;
    if abort {
        handle.notify(Msg::Park).await.unwrap();
        // The handler is parked, so the stop below ends the task by abort.
        entered.await.unwrap();
    } else if matches!(lane, Lane::System) {
        release = Some(hold_loop(&handle, entered).await);
    }
    // The reply side closes only when the late value is dropped; the handle
    // is still alive, so nothing else drops it.
    match lane {
        Lane::System => {
            let permit = reserve_system(&handle).await;
            if let Some(release) = release.take() {
                let _ = release.send(());
            }
            end(&handle, abort, &mut join).await;
            let (reply_tx, reply_rx) = oneshot::channel();
            // The sender the permit returns stays alive until the check
            // ends: the value must be dropped by the crate, not by the
            // last sender.
            let _sender = permit.send(SystemMessage::GetStatus(StatusRequest {
                reply: reply_tx,
                task: false,
            }));
            timeout(BOUND, reply_rx).await.is_ok()
        }
        Lane::Mailbox => {
            let permit = handle.tx.reserve().await.unwrap();
            end(&handle, abort, &mut join).await;
            let (reply_tx, reply_rx) = oneshot::channel();
            permit.send(Envelope::Message {
                payload: Msg::Ping,
                responder: Some(reply_tx),
            });
            timeout(BOUND, reply_rx).await.is_ok()
        }
        Lane::Priority => {
            let permit = handle
                .priority_tx
                .as_ref()
                .unwrap()
                .reserve()
                .await
                .unwrap();
            end(&handle, abort, &mut join).await;
            let (reply_tx, reply_rx) = oneshot::channel();
            permit.send(Envelope::Message {
                payload: Msg::Ping,
                responder: Some(reply_tx),
            });
            timeout(BOUND, reply_rx).await.is_ok()
        }
    }
}

macro_rules! late_value_test {
    ($name:ident, $lane:expr, $abort:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            assert!(
                late_value_dropped($lane, $abort).await,
                "a late value must be dropped"
            );
        }
    };
}

late_value_test!(graceful_system, Lane::System, false);
late_value_test!(graceful_mailbox, Lane::Mailbox, false);
late_value_test!(graceful_priority, Lane::Priority, false);
late_value_test!(kill_abort_system, Lane::System, true);
late_value_test!(kill_abort_mailbox, Lane::Mailbox, true);
late_value_test!(kill_abort_priority, Lane::Priority, true);

/// A slot given back unused must not hold the end, and the actor reads as
/// stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn given_back_slot_does_not_hold_the_end() {
    for lane in [Lane::System, Lane::Mailbox, Lane::Priority] {
        let (handle, mut join, entered) = spawn_park();
        match lane {
            Lane::System => {
                let release = hold_loop(&handle, entered).await;
                let permit = reserve_system(&handle).await;
                let _ = release.send(());
                end(&handle, false, &mut join).await;
                drop(permit);
            }
            Lane::Mailbox => {
                let permit = handle.tx.reserve().await.unwrap();
                end(&handle, false, &mut join).await;
                drop(permit);
            }
            Lane::Priority => {
                let permit = handle
                    .priority_tx
                    .as_ref()
                    .unwrap()
                    .reserve()
                    .await
                    .unwrap();
                end(&handle, false, &mut join).await;
                drop(permit);
            }
        }
        assert!(
            timeout(BOUND, handle.wait_stopped()).await.is_ok(),
            "{lane:?}"
        );
    }
}

/// A late value whose drop panics is contained: the stop reason stays what it
/// was, and a value pushed through a second slot after it is still dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_late_drop_does_not_stop_the_drain() {
    let (handle, mut join, _entered) = spawn_park();
    let first = handle.tx.reserve().await.unwrap();
    let second = handle.tx.reserve().await.unwrap();
    end(&handle, false, &mut join).await;
    first.send(Envelope::Message {
        payload: Msg::Bomb(Bomb),
        responder: None,
    });
    let (reply_tx, reply_rx) = oneshot::channel();
    second.send(Envelope::Message {
        payload: Msg::Ping,
        responder: Some(reply_tx),
    });
    assert!(
        timeout(BOUND, reply_rx).await.is_ok(),
        "the drain must survive a panicking drop"
    );
    assert!(matches!(
        timeout(BOUND, handle.wait_stopped()).await,
        Ok(StopReason::Graceful)
    ));
}

fn runtime_without_time() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap()
}

/// On a runtime built without the time driver a late value is dropped and
/// nothing panics. The wait for the drop is a plain thread poll with a
/// deadline, since a tokio timer is not available there.
#[test]
fn no_time_driver_drops_a_late_value() {
    let rt = runtime_without_time();
    let (handle, reply_rx) = rt.block_on(async {
        let (handle, mut join, _entered) = spawn_park();
        let permit = handle.tx.reserve().await.unwrap();
        handle.stop(StopReason::Graceful).await.unwrap();
        (&mut join).await.unwrap();
        let (reply_tx, reply_rx) = oneshot::channel();
        permit.send(Envelope::Message {
            payload: Msg::Ping,
            responder: Some(reply_tx),
        });
        (handle, reply_rx)
    });
    let mut reply_rx = reply_rx;
    let deadline = std::time::Instant::now() + BOUND;
    let dropped = loop {
        match reply_rx.try_recv() {
            Err(oneshot::error::TryRecvError::Empty) => {}
            _ => break true,
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::yield_now();
    };
    assert!(dropped, "the late value must be dropped without a timer");
    drop(handle);
}

/// The same runtime with no slot out: the plain stop path is unchanged.
#[test]
fn no_time_driver_clean_path() {
    let rt = runtime_without_time();
    rt.block_on(async {
        let (handle, join, _entered) = spawn_park();
        handle.stop(StopReason::Graceful).await.unwrap();
        assert!(join.await.is_ok());
    });
}

fn fresh_system(tag: &str) -> Arc<ActorSystem> {
    ActorSystem::create(format!("late-slot-{tag}-{}", uuid::Uuid::new_v4())).unwrap()
}

/// Yields until `condition` holds; a timeout only detects a failure. The bound only limits how long a real hang takes to report.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let waited = timeout(HANG_BOUND, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(waited.is_ok(), "{what}");
}

/// A mailbox request: the message and the side its reply comes back on.
fn request() -> (
    Envelope<Msg, ()>,
    oneshot::Receiver<crate::error::ActorResult<()>>,
) {
    let (reply_tx, reply_rx) = oneshot::channel();
    (
        Envelope::Message {
            payload: Msg::Ping,
            responder: Some(reply_tx),
        },
        reply_rx,
    )
}

fn runtime_of(multi: bool) -> tokio::runtime::Runtime {
    if multi {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
}

/// Waits, on a plain thread, until the side of a reply is closed or answered.
fn reply_closed<T>(mut reply_rx: oneshot::Receiver<T>) -> bool {
    let deadline = std::time::Instant::now() + BOUND;
    loop {
        match reply_rx.try_recv() {
            Err(oneshot::error::TryRecvError::Empty) => {}
            _ => return true,
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::yield_now();
    }
}

/// The ends the helper slot of the current runtime recorded for `kind`.
fn ends_of(system: &ActorSystem, kind: HelperKind) -> Vec<HelperEnd> {
    system
        .helper_ends(&tokio::runtime::Handle::current())
        .into_iter()
        .filter(|(k, _)| *k == kind)
        .map(|(_, end)| end)
        .collect()
}

/// Many queues end with a permit out at once: each queue gets a task of its
/// own through the system's slot, and the slot counts every one of them down
/// when its value has been dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_queues_drain_through_the_slot() {
    const QUEUES: usize = 32;
    let system = fresh_system("many");
    let id = ActorId::from("many".to_string());
    let mut held = Vec::new();
    for _ in 0..QUEUES {
        let (tx, rx) = tokio::sync::mpsc::channel::<oneshot::Sender<()>>(2);
        let permit = tx.clone().try_reserve_owned().unwrap();
        assert!(runtime::end_queue_late(rx, &system, &id).is_none());
        held.push((tx, permit));
    }
    assert_eq!(system.live_helpers(), QUEUES, "one task for each queue");
    let mut replies = Vec::new();
    let mut senders = Vec::new();
    for (tx, permit) in held {
        let (reply_tx, reply_rx) = oneshot::channel();
        senders.push((tx, permit.send(reply_tx)));
        replies.push(reply_rx);
    }
    for reply_rx in replies {
        let dropped = timeout(BOUND, reply_rx).await;
        assert!(matches!(dropped, Ok(Err(_))), "every late value is dropped");
    }
    until("the slot counts every task down", || {
        system.live_helpers() == 0
    })
    .await;
    assert_eq!(
        ends_of(&system, HelperKind::LateDrain),
        vec![HelperEnd::Finished; QUEUES],
        "each task's end is recorded once"
    );
    drop(senders);
}

/// Many actors end with a mailbox unit out at once: each late value is
/// dropped inside the push that fails, and no task starts for any of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_actors_with_a_mailbox_unit_out_start_no_helper() {
    const ACTORS: usize = 32;
    let system = fresh_system("many-mailbox");
    let mut held = Vec::new();
    for _ in 0..ACTORS {
        let (handle, mut join, _entered) = spawn_park_in(Some(system.clone()));
        let permit = handle.tx.clone().reserve_owned().await.unwrap();
        end(&handle, false, &mut join).await;
        held.push((handle, permit));
    }
    assert_eq!(system.live_helpers(), 0, "a unit out starts no task");
    let mut senders = Vec::new();
    for (handle, permit) in held {
        let (envelope, mut reply_rx) = request();
        senders.push((handle, permit.send(envelope)));
        assert!(
            matches!(
                reply_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Closed)
            ),
            "the value is dropped before the push returns"
        );
    }
    assert_eq!(system.live_helpers(), 0);
    assert!(ends_of(&system, HelperKind::LateDrain).is_empty());
    drop(senders);
}

/// A late value whose drop blocks delays only its own queue's task: an
/// unrelated actor's late value is dropped while the first one is still held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocking_late_drop_delays_only_its_own_queue() {
    struct Slow(std::sync::mpsc::Receiver<()>);
    impl Drop for Slow {
        fn drop(&mut self) {
            let _ = self.0.recv_timeout(Duration::from_secs(20));
        }
    }
    let system = fresh_system("slow");
    let id = ActorId::from("slow".to_string());
    let (first_tx, first_rx) = tokio::sync::mpsc::channel::<Slow>(2);
    let (second_tx, second_rx) = tokio::sync::mpsc::channel::<oneshot::Sender<()>>(2);
    let first_permit = first_tx.clone().try_reserve_owned().unwrap();
    let second_permit = second_tx.clone().try_reserve_owned().unwrap();
    assert!(runtime::end_queue_late(first_rx, &system, &id).is_none());
    assert!(runtime::end_queue_late(second_rx, &system, &id).is_none());
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let _first_sender = first_permit.send(Slow(release_rx));
    let (probe_tx, probe_rx) = oneshot::channel();
    let _second_sender = second_permit.send(probe_tx);
    // The first task is inside its blocking drop; the second is not behind it.
    assert!(
        matches!(timeout(BOUND, probe_rx).await, Ok(Err(_))),
        "an unrelated late value waited behind a blocking drop"
    );
    until("only the first task is left", || system.live_helpers() == 1).await;
    release_tx.send(()).unwrap();
    until("the first task ends", || system.live_helpers() == 0).await;
}

/// A task dropped before it took the value waiting in its receiver drops that
/// value, and the slot records the cancellation once, with the count of
/// values dropped.
#[test]
fn a_cancelled_drain_records_the_values_it_dropped() {
    let rt = runtime_of(false);
    let system = fresh_system("cancel");
    let (tx, rx) = tokio::sync::mpsc::channel::<oneshot::Sender<()>>(2);
    let (reply_tx, reply_rx) = oneshot::channel();
    rt.block_on(async {
        let id = ActorId::from("cancel".to_string());
        let permit = tx.clone().try_reserve_owned().unwrap();
        assert!(runtime::end_queue_late(rx, &system, &id).is_none());
        // The task has not been polled: the value waits in the receiver.
        let _sender = permit.send(reply_tx);
        assert_eq!(system.live_helpers(), 1);
    });
    let handle = rt.handle().clone();
    drop(rt);
    assert_eq!(system.live_helpers(), 0, "the cancelled task counted down");
    assert_eq!(
        system.helper_ends(&handle),
        vec![(HelperKind::LateDrain, HelperEnd::Cancelled { dropped: 1 })],
        "one record, with the value it dropped"
    );
    assert!(reply_closed(reply_rx), "the held value was dropped");
    drop(tx);
}

/// A runtime that ends while an actor of it is still running with a slot
/// out: the actor's queue ends inside the runtime's shutdown, without a panic
/// or a hang, and the request that was on its way reads the end.
#[test]
fn a_runtime_that_ends_with_a_live_actor_and_a_slot_out() {
    for multi in [false, true] {
        let rt = runtime_of(multi);
        let system = fresh_system("rt-live");
        let (handle, permit, _join) = rt.block_on(async {
            let (handle, join, _entered) = spawn_park_in(Some(system.clone()));
            let permit = handle.tx.clone().reserve_owned().await.unwrap();
            (handle, permit, join)
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(rt);
            let _ = done_tx.send(());
        });
        assert!(done_rx.recv_timeout(BOUND).is_ok(), "multi_thread: {multi}");
        assert_eq!(system.live_helpers(), 0, "multi_thread: {multi}");
        let (envelope, reply_rx) = request();
        let sender = permit.send(envelope);
        drop((sender, handle));
        assert!(reply_closed(reply_rx), "multi_thread: {multi}");
    }
}

/// An actor on each of two runtimes, each with a priority-lane permit and a
/// mailbox unit out: each runtime has a slot of its own for the lane's task,
/// the mailbox starts none, and ending one runtime leaves the other serving.
#[test]
fn two_runtimes_have_two_slots() {
    let system = fresh_system("two-rt");
    let first = runtime_of(true);
    let second = runtime_of(true);
    let mut pending = Vec::new();
    for rt in [&first, &second] {
        let system = system.clone();
        pending.push(rt.block_on(async move {
            let (handle, mut join, _entered) = spawn_park_in(Some(system.clone()));
            let lane = handle.priority_tx.clone().unwrap();
            let permit = lane.reserve_owned().await.unwrap();
            let unit = handle.tx.clone().reserve_owned().await.unwrap();
            end(&handle, false, &mut join).await;
            (handle, permit, unit)
        }));
    }
    assert_eq!(
        system.live_helpers(),
        2,
        "one task for each runtime's lane, none for a mailbox"
    );
    drop(first);
    assert_eq!(system.live_helpers(), 1, "only the first slot's task ended");
    let (handle, permit, unit) = pending.remove(1);
    let (envelope, reply_rx) = request();
    let sender = permit.send(envelope);
    assert!(reply_closed(reply_rx), "the second runtime's task drops it");
    let (envelope, mut mailbox_reply) = request();
    let unit_sender = unit.send(envelope);
    assert!(
        matches!(
            mailbox_reply.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ),
        "the mailbox's value is dropped in the push"
    );
    drop((sender, unit_sender, handle));
    drop(second);
    assert_eq!(system.live_helpers(), 0);
}

/// A system that shuts down leaves its queues' tasks serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_system_shutdown_leaves_the_drain_serving() {
    let system = fresh_system("shutdown");
    let (handle, mut join, _entered) = spawn_park_in(Some(system.clone()));
    let permit = handle.tx.clone().reserve_owned().await.unwrap();
    end(&handle, false, &mut join).await;
    let _ = timeout(BOUND, system.shutdown()).await.unwrap();
    let (envelope, reply_rx) = request();
    let _sender = permit.send(envelope);
    assert!(timeout(BOUND, reply_rx).await.unwrap().is_err());
    drop(handle);
}

/// A late value whose drop panics is reported once, naming the actor it was
/// meant for, and the task's end is recorded as finished.
#[cfg(feature = "tracing")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_late_drop_is_logged_with_the_owner() {
    let lines = crate::actor::supervision::lost_runner::capture::install();
    let system = fresh_system("bomb");
    let (handle, mut join, _entered) = spawn_park_in(Some(system.clone()));
    let permit = handle.tx.clone().reserve_owned().await.unwrap();
    end(&handle, false, &mut join).await;
    let _sender = permit.send(Envelope::Message {
        payload: Msg::Bomb(Bomb),
        responder: None,
    });
    let token = format!("actor_id={} ", handle.id());
    until("the panic is logged with the owner's id", || {
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(&token) && line.contains("arrived after its queue closed"))
    })
    .await;
}

struct Gate {
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Actor for Gate {
    type Message = Msg;
    type Response = ();
    async fn handle(&mut self, message: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match message {
            Msg::Park => {
                if let Some(entered) = self.entered.lock().unwrap().take() {
                    let _ = entered.send(());
                }
                let release = self.release.lock().unwrap().take().unwrap();
                let _ = release.await;
                Ok(())
            }
            _ => Err(crate::error::ActorError::User("stop".to_string())),
        }
    }
}

/// A sender that was handed the unit the actor's last receive freed from the
/// mailbox, and gives it back after the actor ended, starts no task: the
/// unit goes back to the room.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_mailbox_sender_handed_the_last_freed_unit_starts_no_task() {
    let system = fresh_system("assigned");
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let id = format!("late-slot-gate-{}", NEXT_ID.fetch_add(1, Ordering::SeqCst));
    let (handle, join, _subtree) = runtime::spawn_actor(
        id.into(),
        Gate {
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(Some(release_rx)),
        },
        ActorConfig::default().with_mailbox_capacity(1),
        None,
        Some(system.clone()),
        None,
        true,
        Duration::ZERO,
        None,
    )
    .unwrap();
    handle.notify(Msg::Park).await.unwrap();
    entered_rx.await.unwrap();
    // The queue holds one message; a third waits for room.
    handle.notify(Msg::Ping).await.unwrap();
    let mut waiting = Box::pin(handle.notify(Msg::Ping));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(waiting.as_mut(), &mut cx).is_pending());
    // The handler returns; the next receive frees the unit for the waiting
    // sender and ends the actor.
    release_tx.send(()).unwrap();
    assert!(timeout(BOUND, join).await.is_ok());
    assert_eq!(system.live_helpers(), 0, "the mailbox starts no task");
    drop(waiting);
    drop(handle);
    assert_eq!(system.live_helpers(), 0);
    assert!(ends_of(&system, HelperKind::LateDrain).is_empty());
}

/// A sender that was handed the permit a bounded lane's last receive freed,
/// and gives it back after the lane ended, leaves no task behind once the
/// last sender is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_lane_sender_that_gives_back_a_freed_slot_leaves_no_task() {
    let system = fresh_system("assigned-lane");
    let id = ActorId::from("assigned-lane".to_string());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<oneshot::Sender<()>>(1);
    let (filler, _kept) = oneshot::channel();
    tx.try_send(filler).unwrap();
    let mut waiting = Box::pin(tx.clone().reserve_owned());
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(waiting.as_mut(), &mut cx).is_pending());
    // The last receive hands its permit to the waiting sender, and the lane
    // ends before that sender runs.
    drop(rx.try_recv().unwrap());
    assert!(runtime::end_queue_late(rx, &system, &id).is_none());
    assert_eq!(system.live_helpers(), 1, "the lane's task is waiting");
    drop(waiting);
    drop(tx);
    until("the task ends with the last sender", || {
        system.live_helpers() == 0
    })
    .await;
}

/// Spawns a `Park` actor with a one-slot mailbox and, when `lane` is set, a
/// one-slot priority lane.
fn spawn_one_slot(lane: bool) -> Spawned {
    let (entered_tx, entered_rx) = oneshot::channel();
    let id = format!("late-slot-one-{}", NEXT_ID.fetch_add(1, Ordering::SeqCst));
    let mut config = ActorConfig::default().with_mailbox_capacity(1);
    if lane {
        config = config.with_priority_lane(1);
    }
    let (handle, join, _subtree) = runtime::spawn_actor(
        id.into(),
        Park {
            entered: Mutex::new(Some(entered_tx)),
        },
        config,
        None,
        None,
        None,
        true,
        Duration::ZERO,
        None,
    )
    .unwrap();
    (handle, join, entered_rx)
}

/// One deadline covers the wait for room and the wait for the reply. The call
/// waits for a slot until half the deadline, then its message is enqueued and
/// handled by a handler that never answers: the call ends at the deadline, not
/// at the deadline after the enqueue.
async fn one_deadline_spans_wait_and_reply(priority: bool) {
    const DEADLINE: Duration = Duration::from_millis(800);
    let (handle, _join, entered) = spawn_one_slot(true);
    let release = hold_loop(&handle, entered).await;
    if priority {
        handle.notify_priority(Msg::Ping).await.unwrap();
    } else {
        handle.notify(Msg::Ping).await.unwrap();
    }
    let started = tokio::time::Instant::now();
    let call = {
        let handle = handle.clone();
        tokio::spawn(async move {
            if priority {
                handle.send_priority_timeout(Msg::Park, DEADLINE).await
            } else {
                handle.send_timeout(Msg::Park, DEADLINE).await
            }
        })
    };
    tokio::time::sleep(DEADLINE / 2).await;
    release.send(()).unwrap();
    let reply = timeout(BOUND, call)
        .await
        .expect("the call must end")
        .unwrap();
    let took = started.elapsed();
    assert!(
        matches!(reply, Err(AskError::Timeout { enqueued: true })),
        "priority={priority}: {reply:?}"
    );
    assert!(
        took >= DEADLINE && took < DEADLINE + DEADLINE / 4,
        "priority={priority}: took {took:?}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn send_timeout_has_one_deadline_for_the_wait_and_the_reply() {
    one_deadline_spans_wait_and_reply(false).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn send_priority_timeout_has_one_deadline_for_the_wait_and_the_reply() {
    one_deadline_spans_wait_and_reply(true).await;
}

/// Without a priority lane, `send_priority_timeout` waits for room in the
/// mailbox for at most its bound.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn send_priority_timeout_without_a_lane_stops_waiting_at_its_bound() {
    const WITHIN: Duration = Duration::from_millis(50);
    let (handle, _join, entered) = spawn_one_slot(false);
    assert!(handle.priority_tx.is_none());
    let _release = hold_loop(&handle, entered).await;
    handle.notify(Msg::Ping).await.unwrap();
    let started = tokio::time::Instant::now();
    let reply = timeout(BOUND, handle.send_priority_timeout(Msg::Ping, WITHIN)).await;
    assert!(
        matches!(reply, Ok(Err(AskError::Timeout { enqueued: false }))),
        "{reply:?}"
    );
    assert!(started.elapsed() < WITHIN * 2, "{:?}", started.elapsed());
}

/// Many senders each hold a slot reserved before the actor ends and push
/// while it ends: every value is dropped, none waits.
async fn concurrent_pushers(repeats: usize) {
    const PUSHERS: usize = 16;
    let system = fresh_system("pushers");
    for attempt in 0..repeats {
        let (handle, mut join, _entered) = spawn_park_in(Some(system.clone()));
        let mut permits = Vec::new();
        for _ in 0..PUSHERS {
            permits.push(handle.tx.clone().reserve_owned().await.unwrap());
        }
        let ender = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.stop(StopReason::Graceful).await })
        };
        let mut pushers = Vec::new();
        let mut replies = Vec::new();
        for permit in permits {
            let (envelope, reply_rx) = request();
            replies.push(reply_rx);
            pushers.push(tokio::spawn(async move {
                if attempt % 2 == 0 {
                    tokio::task::yield_now().await;
                }
                permit.send(envelope)
            }));
        }
        let _ = ender.await;
        assert!(timeout(BOUND, &mut join).await.is_ok());
        let mut senders = Vec::new();
        for pusher in pushers {
            senders.push(pusher.await.unwrap());
        }
        for reply_rx in replies {
            let settled = timeout(BOUND, reply_rx).await;
            assert!(settled.is_ok(), "attempt {attempt}: a value must not wait");
        }
        drop((handle, senders));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pushers_are_never_left_waiting_multi_thread() {
    concurrent_pushers(300).await;
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_pushers_are_never_left_waiting_current_thread() {
    concurrent_pushers(300).await;
}

/// Requests made through the handle while the actor ends: each is answered by
/// its reply or by the actor's final reason, none waits.
async fn requests_racing_the_end(repeats: usize, priority: bool) {
    const CALLERS: usize = 8;
    let system = fresh_system("racing");
    for attempt in 0..repeats {
        let (handle, mut join, _entered) = spawn_park_in(Some(system.clone()));
        let mut callers = Vec::new();
        for _ in 0..CALLERS {
            let handle = handle.clone();
            callers.push(tokio::spawn(async move {
                if priority {
                    handle.send_priority(Msg::Ping).await
                } else {
                    handle.send(Msg::Ping).await
                }
            }));
        }
        if attempt % 2 == 0 {
            tokio::task::yield_now().await;
        }
        handle.stop(StopReason::Graceful).await.unwrap();
        assert!(timeout(BOUND, &mut join).await.is_ok());
        for caller in callers {
            let outcome = timeout(BOUND, caller)
                .await
                .unwrap_or_else(|_| panic!("attempt {attempt}: a request waited"))
                .unwrap();
            assert!(
                matches!(
                    outcome,
                    Ok(())
                        | Err(AskError::Closed)
                        | Err(AskError::ResponseDropped {
                            reason: StopReason::Graceful
                        })
                ),
                "attempt {attempt}: {outcome:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_racing_the_end_are_answered() {
    requests_racing_the_end(200, false).await;
    requests_racing_the_end(200, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn requests_racing_the_end_are_answered_current_thread() {
    requests_racing_the_end(200, false).await;
    requests_racing_the_end(200, true).await;
}

// ---------------------------------------------------------------------------
// A request whose slot was reserved before the actor's queue ended is answered
// with the actor's final reason when the slot is used after the end, however
// the actor's runtime ended. The hook runs between the reservation and the
// push, so the window a preempted sender leaves open is held on purpose.
// ---------------------------------------------------------------------------

fn host_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

/// How the actor's end is caused while a request holds its slot.
#[derive(Clone, Copy, Debug)]
enum End {
    /// The actor's runtime shuts down; the task's drop glue writes `Kill`.
    RuntimeShutdown,
    /// The actor stops gracefully and is `Stopped` before the push.
    GracefulStop,
}

/// The four forms of a request that waits for a reply.
#[derive(Clone, Copy, Debug)]
enum Form {
    Send,
    SendPriority,
    SendTimeout,
    SendPriorityTimeout,
}

impl Form {
    const ALL: [Form; 4] = [
        Form::Send,
        Form::SendPriority,
        Form::SendTimeout,
        Form::SendPriorityTimeout,
    ];

    /// Whether the form pushes into the priority lane.
    fn priority(self) -> bool {
        matches!(self, Form::SendPriority | Form::SendPriorityTimeout)
    }

    /// Makes the request; `within` is the deadline of a timed form.
    async fn call<A: Actor>(
        self,
        handle: &ActorHandle<A>,
        msg: A::Message,
        within: Duration,
    ) -> Result<A::Response, AskError> {
        match self {
            Form::Send => handle.send(msg).await,
            Form::SendPriority => handle.send_priority(msg).await,
            Form::SendTimeout => handle.send_timeout(msg, within).await,
            Form::SendPriorityTimeout => handle.send_priority_timeout(msg, within).await,
        }
    }
}

/// A deadline far beyond what a test waits: an answer within `BOUND` came
/// long before it.
const FAR: Duration = Duration::from_secs(600);

/// Spawns an idle actor on its own runtime, ends it by `end` between the
/// reservation and the push of a request made through `form`, and returns
/// that request's outcome, awaited on another runtime.
fn request_across_the_end(
    form: Form,
    end: End,
) -> Result<Result<(), AskError>, tokio::time::error::Elapsed> {
    let actor_rt = host_runtime();
    let caller_rt = host_runtime();
    let system = fresh_system("hook");
    let (handle, _join) = {
        let _guard = actor_rt.enter();
        let (handle, join, _entered) = spawn_park_in(Some(system));
        (handle, join)
    };
    // The loop runs: a status reply is only produced from inside it.
    actor_rt.block_on(handle.get_status()).unwrap();
    let ender = handle.clone();
    BEFORE_PUSH.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            // Another thread: this one is inside the caller's runtime.
            std::thread::spawn(move || {
                if let End::GracefulStop = end {
                    actor_rt.block_on(async {
                        ender.stop(StopReason::Graceful).await.unwrap();
                        ender.wait_stopped().await;
                    });
                }
                drop(actor_rt);
            })
            .join()
            .unwrap();
        }));
    });
    caller_rt.block_on(async { timeout(BOUND, form.call(&handle, Msg::Ping, FAR)).await })
}

macro_rules! runtime_shutdown_case {
    ($name:ident, $form:expr) => {
        #[test]
        fn $name() {
            let outcome = request_across_the_end($form, End::RuntimeShutdown);
            assert!(
                matches!(
                    outcome,
                    Ok(Err(AskError::ResponseDropped {
                        reason: StopReason::Kill
                    }))
                ),
                "the request was not answered with the actor's final reason: {outcome:?}"
            );
        }
    };
}

runtime_shutdown_case!(runtime_shutdown_mailbox, Form::Send);
runtime_shutdown_case!(runtime_shutdown_priority, Form::SendPriority);
runtime_shutdown_case!(runtime_shutdown_timed_mailbox, Form::SendTimeout);
runtime_shutdown_case!(runtime_shutdown_timed_priority, Form::SendPriorityTimeout);

/// The system lane holds no sender across its wait, so it needs no check: a
/// status request pushed after the actor's runtime shut down is freed with
/// the last sender.
#[test]
fn runtime_shutdown_system() {
    let actor_rt = host_runtime();
    let caller_rt = host_runtime();
    let (handle, _join, entered) = {
        let _guard = actor_rt.enter();
        spawn_park_in(Some(fresh_system("shutdown-system")))
    };
    // The loop is busy, so it takes no status request before the channel
    // opens with the second one.
    let _release = caller_rt.block_on(hold_loop(&handle, entered));
    let record = handle.stop_lane().abort_record().clone();
    for _ in 0..2 {
        let (reply, _requester) = oneshot::channel();
        caller_rt.block_on(record.submit_status(reply)).unwrap();
    }
    let tx = record.system_tx_for_test().expect("the channel is open");
    let permit = caller_rt.block_on(tx.reserve_owned()).unwrap();
    drop(actor_rt);
    let (reply_tx, reply_rx) = oneshot::channel();
    drop(permit.send(SystemMessage::GetStatus(StatusRequest {
        reply: reply_tx,
        task: false,
    })));
    assert!(
        caller_rt.block_on(async { timeout(BOUND, reply_rx).await.is_ok() }),
        "a status request pushed after the runtime shut down never resolved"
    );
}

/// The answer is the actor's final reason as the actor ended, never a reason
/// that was still open.
#[test]
fn a_request_across_the_end_reads_the_final_reason() {
    for form in Form::ALL {
        let graceful = request_across_the_end(form, End::GracefulStop);
        assert!(
            matches!(
                graceful,
                Ok(Err(AskError::ResponseDropped {
                    reason: StopReason::Graceful
                }))
            ),
            "{form:?}: {graceful:?}"
        );
        let kill = request_across_the_end(form, End::RuntimeShutdown);
        assert!(
            matches!(
                kill,
                Ok(Err(AskError::ResponseDropped {
                    reason: StopReason::Kill
                }))
            ),
            "{form:?}: {kill:?}"
        );
    }
}

/// A request that finds its queue closed after its push waits for the
/// actor's reason to be final: it answers nothing while the actor still runs,
/// and with that reason once the actor has stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_answer_waits_for_the_reason_to_be_final() {
    let (handle, mut join, _entered) = spawn_park();
    let (_reply_tx, reply_rx) = oneshot::channel();
    let mut answer = Box::pin(handle.answer_after_close(reply_rx));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(answer.as_mut(), &mut cx).is_pending());
    end(&handle, false, &mut join).await;
    let outcome = timeout(BOUND, answer).await.unwrap();
    assert!(matches!(
        outcome,
        Err(AskError::ResponseDropped {
            reason: StopReason::Graceful
        })
    ));
}

/// A reply the actor sent before it closed its queue is read, not replaced by
/// the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_sent_before_the_close_is_read() {
    let (handle, _join, _entered) = spawn_park();
    let (reply_tx, reply_rx) = oneshot::channel();
    reply_tx.send(Ok(())).unwrap();
    assert!(matches!(handle.answer_after_close(reply_rx).await, Ok(())));
    let (reply_tx, reply_rx) = oneshot::channel();
    reply_tx
        .send(Err(crate::error::ActorError::User("refused".to_string())))
        .unwrap();
    assert!(matches!(
        handle.answer_after_close(reply_rx).await,
        Err(AskError::Actor(_))
    ));
}

/// A queue ended with a slot out and no runtime to run a task ends in
/// place: nothing is spawned and a value pushed later waits for the last
/// sender.
#[test]
fn a_queue_with_a_slot_out_and_no_runtime_ends_in_place() {
    let system = fresh_system("no-rt");
    let (tx, rx) = tokio::sync::mpsc::channel::<oneshot::Sender<()>>(2);
    let permit = tx.clone().try_reserve_owned().unwrap();
    let id = ActorId::from("no-runtime".to_string());
    assert!(runtime::end_queue_late(rx, &system, &id).is_none());
    assert_eq!(system.live_helpers(), 0);
    let (reply_tx, reply_rx) = oneshot::channel();
    let _sender = permit.send(reply_tx);
    // Nothing runs to drop the value: it waits for the last sender.
    let mut reply_rx = reply_rx;
    assert!(matches!(
        reply_rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    drop(tx);
}

// ---------------------------------------------------------------------------
// Many queues, real receivers, senders racing the end.
// ---------------------------------------------------------------------------

mod stress {
    use super::*;
    use tokio::sync::mpsc;

    type Reply = oneshot::Sender<()>;

    async fn closed(rx: oneshot::Receiver<()>) -> bool {
        matches!(timeout(BOUND, rx).await, Ok(Err(_)))
    }

    /// Many repeats of: reserve slots, end the queue, race pushes and
    /// give-backs from threads and inline.
    async fn real_queues() {
        let system = fresh_system("stress");
        let id = ActorId::from("stress".to_string());
        for iteration in 0..3000usize {
            const N: usize = 9;
            let (tx, rx) = mpsc::channel::<Reply>(N);
            let mut permits = Vec::new();
            for _ in 0..N {
                permits.push(tx.clone().try_reserve_owned().unwrap());
            }
            assert!(runtime::end_queue_late(rx, &system, &id).is_none());
            let mut replies = Vec::new();
            let mut threads = Vec::new();
            let keep = Arc::new(Mutex::new(Vec::new()));
            for (i, permit) in permits.into_iter().enumerate() {
                let keep = keep.clone();
                match i % 3 {
                    0 => {
                        let (reply, closed) = oneshot::channel();
                        replies.push(closed);
                        threads.push(std::thread::spawn(move || {
                            if iteration % 2 == 0 {
                                std::thread::yield_now();
                            }
                            let sender = permit.send(reply);
                            keep.lock().unwrap().push(sender);
                        }));
                    }
                    1 => threads.push(std::thread::spawn(move || drop(permit))),
                    _ => {
                        let (reply, closed) = oneshot::channel();
                        replies.push(closed);
                        let sender = permit.send(reply);
                        keep.lock().unwrap().push(sender);
                    }
                }
            }
            for reply in replies {
                assert!(
                    closed(reply).await,
                    "iteration {iteration}: a value was not dropped"
                );
            }
            for thread in threads {
                thread.join().unwrap();
            }
            // Every slot is used or given back and the sender is alive: the
            // queue's task has ended.
            until("the queue's task ends", || system.live_helpers() == 0).await;
            drop(tx);
            drop(keep);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn queues_ended_with_slots_out_settle_multi_thread() {
        real_queues().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queues_ended_with_slots_out_settle_current_thread() {
        real_queues().await;
    }

    /// Thousands of queues at once, then every value pushed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn many_queues_at_once() {
        let system = fresh_system("stress-many");
        let id = ActorId::from("stress".to_string());
        let mut held = Vec::new();
        for _ in 0..3000 {
            let (tx, rx) = mpsc::channel::<Reply>(2);
            let permit = tx.clone().try_reserve_owned().unwrap();
            assert!(runtime::end_queue_late(rx, &system, &id).is_none());
            held.push((tx, permit));
        }
        assert_eq!(system.live_helpers(), 3000);
        let mut replies = Vec::new();
        let mut keep = Vec::new();
        for (tx, permit) in held {
            let (reply, closed) = oneshot::channel();
            replies.push(closed);
            keep.push((tx, permit.send(reply)));
        }
        for reply in replies {
            assert!(closed(reply).await);
        }
        until("every task ends", || system.live_helpers() == 0).await;
    }

    static NEXT: Mutex<Vec<(mpsc::Sender<Reply>, mpsc::OwnedPermit<Reply>)>> =
        Mutex::new(Vec::new());
    static SYSTEM: Mutex<Option<Arc<ActorSystem>>> = Mutex::new(None);

    /// A value whose drop ends another queue with a slot out, from inside the
    /// task that drops it.
    struct EndsAnother;
    impl Drop for EndsAnother {
        fn drop(&mut self) {
            let system = SYSTEM.lock().unwrap().clone().unwrap();
            let id = ActorId::from("reentrant".to_string());
            let (tx, rx) = mpsc::channel::<Reply>(2);
            let permit = tx.clone().try_reserve_owned().unwrap();
            assert!(runtime::end_queue_late(rx, &system, &id).is_none());
            NEXT.lock().unwrap().push((tx, permit));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_drop_that_ends_another_queue() {
        let system = fresh_system("stress-re");
        *SYSTEM.lock().unwrap() = Some(system.clone());
        let id = ActorId::from("stress".to_string());
        let (tx, rx) = mpsc::channel::<EndsAnother>(2);
        let permit = tx.clone().try_reserve_owned().unwrap();
        assert!(runtime::end_queue_late(rx, &system, &id).is_none());
        let _sender = permit.send(EndsAnother);
        until("the second queue is ended", || {
            NEXT.lock().unwrap().len() == 1
        })
        .await;
        let (second_tx, second_permit) = NEXT.lock().unwrap().pop().unwrap();
        let (reply, closed_rx) = oneshot::channel();
        let _second_sender = second_permit.send(reply);
        assert!(closed(closed_rx).await);
        drop(second_tx);
    }
}

// ---------------------------------------------------------------------------
// Which queue a request reads, and a reason that is not yet final.
// ---------------------------------------------------------------------------

/// A request pushed after the actor's priority lane ended and before its
/// mailbox does reads the queue it pushed to. The actor's runtime shuts down
/// and the task's drop glue ends the lane first, then the mailbox; the
/// shutdown is held between the two while the request is made. A request on
/// the lane finds it closed and is answered at once, with the reason the drop
/// glue wrote. A request on the mailbox finds it open, is drained by the
/// mailbox's own end, and is answered after it.
fn request_between_the_queue_ends(form: Form) {
    let actor_rt = host_runtime();
    let caller_rt = host_runtime();
    let (handle, _join, _entered) = {
        let _guard = actor_rt.enter();
        spawn_park_in(Some(fresh_system("between")))
    };
    actor_rt.block_on(handle.get_status()).unwrap();
    let (ended_tx, ended_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    BETWEEN_QUEUE_ENDS.lock().unwrap().push((
        handle.id().clone(),
        Box::new(move || {
            let _ = ended_tx.send(());
            let _ = release_rx.recv_timeout(BOUND);
        }),
    ));
    let shutdown = std::rc::Rc::new(std::cell::RefCell::new(None));
    let started = shutdown.clone();
    BEFORE_PUSH.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            *started.borrow_mut() = Some(std::thread::spawn(move || drop(actor_rt)));
            ended_rx
                .recv_timeout(BOUND)
                .expect("the first queue of the actor ended");
        }));
    });
    let (first, rest) = caller_rt.block_on(async {
        let mut call = Box::pin(form.call(&handle, Msg::Ping, FAR));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let first = std::future::Future::poll(call.as_mut(), &mut cx);
        let _ = release_tx.send(());
        match first {
            std::task::Poll::Ready(outcome) => (Some(outcome), None),
            std::task::Poll::Pending => (None, Some(timeout(BOUND, call).await)),
        }
    });
    let thread = shutdown.borrow_mut().take().expect("the shutdown started");
    thread.join().unwrap();
    let reason_kill = |outcome: &Result<(), AskError>| {
        matches!(
            outcome,
            Err(AskError::ResponseDropped {
                reason: StopReason::Kill
            })
        )
    };
    if form.priority() {
        let first = first.expect("a request on the closed lane was not answered at once");
        assert!(reason_kill(&first), "{form:?}: {first:?}");
    } else {
        assert!(
            first.is_none(),
            "{form:?}: answered before the mailbox ended"
        );
        let rest = rest.unwrap().expect("never answered");
        assert!(reason_kill(&rest), "{form:?}: {rest:?}");
    }
}

#[test]
fn a_priority_request_reads_the_lane_it_pushed_to() {
    request_between_the_queue_ends(Form::SendPriority);
}

#[test]
fn a_timed_priority_request_reads_the_lane_it_pushed_to() {
    request_between_the_queue_ends(Form::SendPriorityTimeout);
}

#[test]
fn a_mailbox_request_reads_the_mailbox_it_pushed_to() {
    request_between_the_queue_ends(Form::Send);
}

#[test]
fn a_timed_mailbox_request_reads_the_mailbox_it_pushed_to() {
    request_between_the_queue_ends(Form::SendTimeout);
}

/// The receiver closes the mailbox while it still holds the queue (the actor
/// is tearing down its children), and a request made just then pushes into it.
/// The request reads the closed mailbox after its push and is answered with
/// the reason the actor's status plane carries, without waiting for the
/// receiver to drop the queue.
#[test]
fn a_mailbox_request_pushed_after_the_close_is_answered_while_the_queue_is_still_held() {
    use crate::types::{ActorStatus, StatusTx, StatusValue, StopLane};
    let (tx, rx) = crate::actor::mailbox::test_channel(2);
    let rx = std::rc::Rc::new(std::cell::RefCell::new(rx));
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    StatusValue::write_reason(&status_tx, StopReason::Graceful, true);
    let (stop_lane, _stop_rx) = StopLane::new();
    let handle = ActorHandle::<Park>::new(
        ActorId::from("closed-mailbox-request".to_string()),
        tx,
        stop_lane,
        2,
        status_rx,
        "closed-mailbox-request".into(),
        None,
        None,
    );
    let closing = rx.clone();
    BEFORE_PUSH.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || closing.borrow_mut().close()));
    });
    let mut call = Box::pin(Form::Send.call(&handle, Msg::Ping, FAR));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let outcome = std::future::Future::poll(call.as_mut(), &mut cx);
    assert!(
        matches!(
            outcome,
            std::task::Poll::Ready(Err(AskError::ResponseDropped {
                reason: StopReason::Graceful
            }))
        ),
        "the request waited for its own reply to be dropped: {outcome:?}"
    );
    drop(call);
    assert_eq!(rx.borrow().len(), 1, "the request is still in the queue");
}

mod held_teardown {
    use super::*;
    use crate::types::{RestartType, Shutdown};
    use std::sync::atomic::AtomicBool;

    /// A child whose `on_stopped` returns only once its gate opens.
    struct SlowChild {
        gate: Arc<AtomicBool>,
    }

    impl Actor for SlowChild {
        type Message = ();
        type Response = ();
        async fn handle(&mut self, _m: (), _c: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
        async fn on_stopped(
            &mut self,
            _r: &StopReason,
            _c: &mut ActorContext<Self>,
        ) -> ActorResult<()> {
            while !self.gate.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Ok(())
        }
    }

    struct Sup {
        gate: Arc<AtomicBool>,
        started: bool,
    }

    impl Actor for Sup {
        type Message = ();
        type Response = ();
        async fn handle(&mut self, _m: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if !self.started {
                self.started = true;
                let gate = self.gate.clone();
                ctx.spawn_child(move || SlowChild { gate: gate.clone() })
                    .named("slow-child".to_string())
                    .restart_type(RestartType::Temporary)
                    .shutdown(Shutdown::Timeout(Duration::from_secs(8)))
                    .await?;
            }
            Ok(())
        }
    }

    /// A supervisor whose child is held at its `on_stopped`, and the runtimes
    /// around it. The hook installed for the next request stops the supervisor
    /// between that request's reservation and its push, and returns once both
    /// of its queues are closed: the request then finds a closed queue while
    /// the teardown is held and the reason is still open.
    struct Held {
        actor_rt: tokio::runtime::Runtime,
        caller_rt: tokio::runtime::Runtime,
        handle: ActorHandle<Sup>,
        gate: Arc<AtomicBool>,
    }

    fn held() -> Held {
        let actor_rt = host_runtime();
        let caller_rt = host_runtime();
        let gate = Arc::new(AtomicBool::new(false));
        let (handle, _join) = {
            let _guard = actor_rt.enter();
            let (handle, join, _subtree) = runtime::spawn_actor(
                format!("held-{}", NEXT_ID.fetch_add(1, Ordering::SeqCst)).into(),
                Sup {
                    gate: gate.clone(),
                    started: false,
                },
                ActorConfig::default().with_priority_lane(4).supervisor(),
                None,
                Some(fresh_system("held")),
                None,
                true,
                Duration::ZERO,
                None,
            )
            .unwrap();
            (handle, join)
        };
        actor_rt.block_on(handle.send(())).unwrap();
        let ender = handle.clone();
        let rt_handle = actor_rt.handle().clone();
        BEFORE_PUSH.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                std::thread::spawn(move || {
                    rt_handle.block_on(async {
                        ender.stop(StopReason::Graceful).await.unwrap();
                        let waited = timeout(BOUND, async {
                            while !(ender.tx.is_closed()
                                && ender
                                    .priority_tx
                                    .as_ref()
                                    .is_none_or(|lane| lane.is_closed()))
                            {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await;
                        assert!(waited.is_ok(), "the supervisor's queues never closed");
                    });
                })
                .join()
                .unwrap();
            }));
        });
        Held {
            actor_rt,
            caller_rt,
            handle,
            gate,
        }
    }

    impl Held {
        /// Opens the gate and waits for the supervisor to be stopped.
        fn finish(&self) {
            self.gate.store(true, Ordering::SeqCst);
            let stopped = self
                .actor_rt
                .block_on(async { timeout(BOUND, self.handle.wait_stopped()).await });
            assert!(stopped.is_ok(), "the supervisor never stopped");
        }
    }

    /// The request waits for the reason to be final: it is not answered
    /// while the child holds the teardown, and it is answered with the reason
    /// the supervisor ends with once the child lets go.
    fn waits_for_the_reason(form: Form, kill_meanwhile: bool) {
        let held = held();
        let outcome = held.caller_rt.block_on(async {
            let mut call = Box::pin(form.call(&held.handle, (), FAR));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            let first = std::future::Future::poll(call.as_mut(), &mut cx);
            assert!(
                first.is_pending(),
                "{form:?}: answered before the reason was final"
            );
            assert!(held.handle.status_rx.borrow().reason.is_none());
            if kill_meanwhile {
                held.handle.stop(StopReason::Kill).await.ok();
            }
            held.gate.store(true, Ordering::SeqCst);
            timeout(BOUND, call).await.expect("never answered")
        });
        held.finish();
        let want = if kill_meanwhile {
            StopReason::Kill
        } else {
            StopReason::Graceful
        };
        match outcome {
            Err(AskError::ResponseDropped { reason }) => {
                assert_eq!(reason, want, "{form:?}");
            }
            other => panic!("{form:?}: {other:?}"),
        }
    }

    /// A timed request that finds its queue closed while the reason is still
    /// open ends at its own deadline with `Timeout { enqueued: true }`, not
    /// later. The wait is bounded by the deadline, not by the child. The
    /// elapsed bounds only detect a failure.
    fn times_out_at_its_deadline(form: Form) {
        const DEADLINE: Duration = Duration::from_millis(400);
        let held = held();
        let (outcome, elapsed) = held.caller_rt.block_on(async {
            let started = std::time::Instant::now();
            let outcome = timeout(BOUND, form.call(&held.handle, (), DEADLINE)).await;
            (outcome, started.elapsed())
        });
        // Opened before the assertions, so a failure leaves nothing parked.
        held.finish();
        assert!(
            matches!(outcome, Ok(Err(AskError::Timeout { enqueued: true }))),
            "{form:?}: {outcome:?}"
        );
        assert!(elapsed >= DEADLINE, "{form:?}: answered at {elapsed:?}");
        assert!(
            elapsed < DEADLINE + Duration::from_secs(2),
            "{form:?}: answered at {elapsed:?}"
        );
    }

    #[test]
    fn closed_but_not_final_mailbox() {
        waits_for_the_reason(Form::Send, false);
    }

    #[test]
    fn closed_but_not_final_priority() {
        waits_for_the_reason(Form::SendPriority, false);
    }

    #[test]
    fn closed_but_not_final_kill_mailbox() {
        waits_for_the_reason(Form::Send, true);
    }

    #[test]
    fn closed_but_not_final_kill_priority() {
        waits_for_the_reason(Form::SendPriority, true);
    }

    #[test]
    fn closed_but_not_final_timed_mailbox() {
        waits_for_the_reason(Form::SendTimeout, false);
    }

    #[test]
    fn closed_but_not_final_timed_priority() {
        waits_for_the_reason(Form::SendPriorityTimeout, false);
    }

    #[test]
    fn closed_but_not_final_kill_timed_mailbox() {
        waits_for_the_reason(Form::SendTimeout, true);
    }

    #[test]
    fn closed_but_not_final_timed_mailbox_at_the_deadline() {
        times_out_at_its_deadline(Form::SendTimeout);
    }

    #[test]
    fn closed_but_not_final_timed_priority_at_the_deadline() {
        times_out_at_its_deadline(Form::SendPriorityTimeout);
    }
}

// ---------------------------------------------------------------------------
// Requests racing a stop, in every form.
// ---------------------------------------------------------------------------
mod settlement {
    use super::*;
    use std::collections::HashSet;

    struct Count {
        done: Arc<Mutex<HashSet<u64>>>,
    }

    impl Actor for Count {
        type Message = u64;
        type Response = u64;
        async fn handle(
            &mut self,
            message: u64,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<u64> {
            self.done.lock().unwrap().insert(message);
            Ok(message)
        }
    }

    fn spawn_count(
        system: Arc<ActorSystem>,
        done: Arc<Mutex<HashSet<u64>>>,
    ) -> (ActorHandle<Count>, tokio::task::JoinHandle<StopReason>) {
        let id = format!("settle-{}", NEXT_ID.fetch_add(1, Ordering::SeqCst));
        let (handle, join, _subtree) = runtime::spawn_actor(
            id.into(),
            Count { done },
            ActorConfig::default().with_priority_lane(4),
            None,
            Some(system),
            None,
            true,
            Duration::ZERO,
            None,
        )
        .unwrap();
        (handle, join)
    }

    /// A handled message is answered with its reply, and under a graceful stop
    /// a handled message is never answered with an error. Senders in all four
    /// forms run flat out while the actor stops.
    async fn handled_iff_ok(repeats: usize, stop: StopReason, strict: bool) {
        let system = fresh_system("settle");
        for iteration in 0..repeats {
            let done = Arc::new(Mutex::new(HashSet::new()));
            let (handle, mut join) = spawn_count(system.clone(), done.clone());
            let mut senders = Vec::new();
            for s in 0..8u64 {
                let handle = handle.clone();
                senders.push(tokio::spawn(async move {
                    let form = Form::ALL[(s % 4) as usize];
                    let mut out = Vec::new();
                    for i in 0..200u64 {
                        let id = s * 1000 + i;
                        let r = form.call(&handle, id, BOUND).await;
                        let failed = r.is_err();
                        out.push((id, r));
                        if failed {
                            break;
                        }
                    }
                    out
                }));
            }
            for _ in 0..(iteration % 5) {
                tokio::task::yield_now().await;
            }
            let _ = handle.stop(stop.clone()).await;
            assert!(timeout(BOUND, &mut join).await.is_ok());
            let done = done.lock().unwrap().clone();
            for sender in senders {
                let out = timeout(BOUND, sender)
                    .await
                    .expect("a sender waited")
                    .unwrap();
                for (id, r) in out {
                    match r {
                        Ok(v) => {
                            assert_eq!(v, id);
                            assert!(
                                done.contains(&id),
                                "iteration {iteration}: ok but not handled"
                            );
                        }
                        Err(AskError::ResponseDropped { .. }) | Err(AskError::Closed) => {
                            assert!(
                                !strict || !done.contains(&id),
                                "iteration {iteration}: handled {id} but answered an error"
                            );
                        }
                        Err(other) => panic!("iteration {iteration}: {other:?}"),
                    }
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn handled_iff_ok_graceful() {
        handled_iff_ok(300, StopReason::Graceful, true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn handled_iff_ok_kill() {
        handled_iff_ok(300, StopReason::Kill, false).await;
    }

    /// A request future dropped at its await points leaves the actor ending.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dropped_senders_do_not_hold_the_end() {
        let system = fresh_system("settle-drop");
        for iteration in 0..300usize {
            let done = Arc::new(Mutex::new(HashSet::new()));
            let (handle, mut join) = spawn_count(system.clone(), done);
            for k in 0..8usize {
                let handle = handle.clone();
                tokio::spawn(async move {
                    let form = Form::ALL[k % 4];
                    let within = Duration::from_micros((iteration % 7 + k) as u64 * 20);
                    let _ = timeout(within, form.call(&handle, 1, BOUND)).await;
                });
            }
            tokio::task::yield_now().await;
            let _ = handle.stop(StopReason::Graceful).await;
            assert!(
                timeout(BOUND, &mut join).await.is_ok(),
                "iteration {iteration}"
            );
        }
        until("helpers settle", || system.live_helpers() == 0).await;
    }
}

// ---------------------------------------------------------------------------
// The answer to a request whose handler failed.
// ---------------------------------------------------------------------------

mod crash_answer {
    use super::*;
    use std::sync::atomic::AtomicBool;

    use tokio::sync::{Notify, Semaphore};

    use crate::actor::runtime::{end_hooks, fast_path_turns, UNCAUGHT_PANIC};
    use crate::actor::ActorExt;
    use crate::error::ActorError;

    /// What a test holds and watches of an actor's `on_stopped`.
    struct Stopped {
        entered: Notify,
        gate: Semaphore,
        returned: AtomicBool,
    }

    impl Stopped {
        /// A hook that waits at its gate until a test adds a permit.
        fn held() -> Arc<Stopped> {
            Arc::new(Stopped {
                entered: Notify::new(),
                gate: Semaphore::new(0),
                returned: AtomicBool::new(false),
            })
        }

        /// A hook that never waits.
        fn open() -> Arc<Stopped> {
            let stopped = Stopped::held();
            stopped.gate.add_permits(1 << 20);
            stopped
        }

        async fn entered(&self) {
            timeout(BOUND, self.entered.notified())
                .await
                .expect("on_stopped begins");
        }
    }

    enum Turn {
        /// Holds the turn until a test releases it.
        Block,
        /// Records how many fast path turns the thread has taken.
        Mark,
        /// Records the same, then fails.
        Fail,
    }

    struct Crasher {
        stopped: Arc<Stopped>,
        in_turn: Arc<Notify>,
        release: Arc<Notify>,
        marks: Arc<Mutex<Vec<u32>>>,
    }

    impl Crasher {
        fn new(stopped: Arc<Stopped>) -> Crasher {
            Crasher {
                stopped,
                in_turn: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
                marks: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Actor for Crasher {
        type Message = Turn;
        type Response = u32;

        async fn handle(&mut self, turn: Turn, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
            match turn {
                Turn::Block => {
                    self.in_turn.notify_one();
                    self.release.notified().await;
                    Ok(0)
                }
                Turn::Mark => {
                    self.marks.lock().unwrap().push(fast_path_turns::read());
                    Ok(1)
                }
                Turn::Fail => {
                    self.marks.lock().unwrap().push(fast_path_turns::read());
                    Err(ActorError::User("the handler failed".to_string()))
                }
            }
        }

        async fn on_stopped(
            &mut self,
            _reason: &StopReason,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<()> {
            self.stopped.entered.notify_one();
            self.stopped.gate.acquire().await.unwrap().forget();
            self.stopped.returned.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn unique_name(tag: &str) -> String {
        format!("{tag}-{}", NEXT_ID.fetch_add(1, Ordering::SeqCst))
    }

    /// A failing request taken from a backlog at a turn boundary, by the fast
    /// path, is answered after the actor has ended like any other: while
    /// `on_stopped` is held the caller is not answered, and when it is
    /// answered `on_stopped` has returned and the name is free.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failing_request_taken_from_a_backlog_is_answered_after_the_end() {
        let stopped = Stopped::held();
        let crasher = Crasher::new(stopped.clone());
        let (in_turn, release, marks) = (
            crasher.in_turn.clone(),
            crasher.release.clone(),
            crasher.marks.clone(),
        );
        let system = fresh_system("backlog");
        let name = unique_name("backlog");
        let handle = crasher
            .spawn()
            .named(name.clone())
            .on_system(&system)
            .await
            .unwrap();

        handle.notify(Turn::Block).await.unwrap();
        timeout(BOUND, in_turn.notified())
            .await
            .expect("the first turn begins");
        handle.notify(Turn::Mark).await.unwrap();
        let mut asker = {
            let (handle, system, name, stopped) = (
                handle.clone(),
                system.clone(),
                name.clone(),
                stopped.clone(),
            );
            tokio::spawn(async move {
                let answer = handle.send(Turn::Fail).await;
                (
                    answer,
                    stopped.returned.load(Ordering::SeqCst),
                    system.get::<Crasher>(&name).is_some(),
                )
            })
        };
        // The request is queued behind the mark while the first turn holds.
        tokio::task::yield_now().await;
        release.notify_one();
        stopped.entered().await;

        let marks = marks.lock().unwrap().clone();
        assert_eq!(marks.len(), 2, "the mark and the failing turn ran");
        assert_eq!(
            marks[1],
            marks[0] + 1,
            "the failing turn was taken by the fast path"
        );
        assert!(
            timeout(Duration::from_millis(100), &mut asker)
                .await
                .is_err(),
            "the failing request was answered before the actor ended"
        );
        stopped.gate.add_permits(1);
        let (answer, returned, registered) =
            timeout(BOUND, asker).await.expect("answered").unwrap();
        assert!(matches!(answer, Err(AskError::Actor(_))), "{answer:?}");
        assert!(returned, "answered before on_stopped returned");
        assert!(!registered, "answered with the name registered");
    }

    /// What the hooks at the two points of the end saw of a request's reply
    /// channel and of the status plane.
    #[derive(Default)]
    struct Observed {
        /// At the point just before the name is freed: the channel was empty.
        unanswered_before_the_name_is_freed: Option<bool>,
        /// At the point just after the answer is sent: the channel held the
        /// handler's error, the plane carried no reason yet, and the actor read
        /// as stopping.
        answered: Option<bool>,
        reason_not_yet_written: Option<bool>,
        stopping: Option<bool>,
    }

    /// The answer to a request whose handler failed is sent after the registry
    /// guard freed the name and before the reason is written, and the plane
    /// reads stopping until the end: a window of a few instructions, observed
    /// from the two points of the end that bound it. The request is pushed by
    /// hand to keep its reply channel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_crash_answer_is_sent_after_the_name_is_freed_and_before_the_reason_is_written() {
        use crate::types::ActorStatus;
        let system = fresh_system("order");
        let name = unique_name("order");
        let handle = Crasher::new(Stopped::open())
            .spawn()
            .named(name.clone())
            .on_system(&system)
            .await
            .unwrap();
        let (reply_tx, reply_rx) = oneshot::channel();
        let reply_rx = Arc::new(Mutex::new(reply_rx));
        let observed = Arc::new(Mutex::new(Observed::default()));
        let id = ActorId::from(name.clone());
        {
            let (reply_rx, observed) = (reply_rx.clone(), observed.clone());
            end_hooks::attach(end_hooks::NAME, id.clone(), move || {
                let empty = matches!(
                    reply_rx.lock().unwrap().try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                );
                observed.lock().unwrap().unanswered_before_the_name_is_freed = Some(empty);
            });
        }
        {
            let (reply_rx, observed, status) =
                (reply_rx.clone(), observed.clone(), handle.status_rx());
            end_hooks::attach(end_hooks::ANSWER, id, move || {
                let answered = matches!(
                    reply_rx.lock().unwrap().try_recv(),
                    Ok(Err(ActorError::User(_)))
                );
                let mut seen = observed.lock().unwrap();
                seen.answered = Some(answered);
                seen.reason_not_yet_written = Some(status.borrow().reason.is_none());
                seen.stopping = Some(status.status() == ActorStatus::Stopping);
            });
        }
        let permit = handle.tx.reserve().await.map_err(|_| ()).unwrap();
        permit.send(Envelope::Message {
            payload: Turn::Fail,
            responder: Some(reply_tx),
        });
        timeout(BOUND, handle.wait_stopped())
            .await
            .expect("the actor ends");
        let seen = observed.lock().unwrap();
        assert_eq!(
            seen.unanswered_before_the_name_is_freed,
            Some(true),
            "the answer was sent before the name was freed"
        );
        assert_eq!(
            seen.answered,
            Some(true),
            "the answer was not sent by the seal"
        );
        assert_eq!(
            seen.reason_not_yet_written,
            Some(true),
            "the reason was written before the answer"
        );
        assert_eq!(seen.stopping, Some(true));
        assert!(system.get::<Crasher>(&name).is_none());
    }

    /// The task's own poll unwinds right after the name is freed, past a held
    /// answer: the caller reads the final reason the unwind writes, the
    /// uncaught panic, with the name free.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_task_that_unwinds_after_the_handler_failed_leaves_its_caller_the_final_reason() {
        let system = fresh_system("unwind");
        let name = unique_name("unwind");
        let handle = Crasher::new(Stopped::open())
            .spawn()
            .named(name.clone())
            .on_system(&system)
            .await
            .unwrap();
        end_hooks::attach(end_hooks::NAME_FREED, ActorId::from(name.clone()), || {
            panic!("the crate's own code panics once the name is freed")
        });
        let answer = timeout(BOUND, handle.send(Turn::Fail))
            .await
            .expect("the caller is released");
        assert!(
            matches!(
                &answer,
                Err(AskError::ResponseDropped {
                    reason: StopReason::Failure(ActorError::Panic(text))
                }) if text == UNCAUGHT_PANIC
            ),
            "{answer:?}"
        );
        assert!(
            system.get::<Crasher>(&name).is_none(),
            "the name was not free when the call returned"
        );
    }

    /// A reply that arrives after the first read, while the call waits for
    /// the actor's final reason, is read once more after that reason: the
    /// answer to a request whose handler failed is never replaced by the
    /// reason.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reply_that_arrives_after_the_first_read_is_read_after_the_reason() {
        let (handle, mut join, _entered) = spawn_park();
        let (reply_tx, reply_rx) = oneshot::channel();
        let mut answer = Box::pin(handle.answer_after_close(reply_rx));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(answer.as_mut(), &mut cx).is_pending());
        reply_tx
            .send(Err(ActorError::User("the handler failed".to_string())))
            .unwrap();
        end(&handle, false, &mut join).await;
        let outcome = timeout(BOUND, answer).await.unwrap();
        assert!(matches!(outcome, Err(AskError::Actor(_))), "{outcome:?}");
    }

    /// A reply the actor already sent is read at once, while the actor still
    /// runs: the call does not wait for a reason that is not final.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reply_already_sent_is_read_without_waiting_for_the_reason() {
        let (handle, _join, _entered) = spawn_park();
        let (reply_tx, reply_rx) = oneshot::channel();
        reply_tx
            .send(Err(ActorError::User("the handler failed".to_string())))
            .unwrap();
        let outcome = timeout(BOUND, handle.answer_after_close(reply_rx))
            .await
            .expect("a reply already sent was not read at once");
        assert!(matches!(outcome, Err(AskError::Actor(_))), "{outcome:?}");
    }

    /// Each form of request that finds its queue closed after its push waits
    /// for the actor's final reason, then reads the answer the actor sent it
    /// meanwhile: the answer to a request whose handler failed is sent at the
    /// lane's seal, before the reason is written.
    #[tokio::test(flavor = "current_thread")]
    async fn a_request_across_the_close_reads_the_answer_sent_before_the_reason() {
        use crate::types::{ActorStatus, StatusTx, StatusValue, StopLane};
        for form in Form::ALL {
            let (tx, rx) = crate::actor::mailbox::test_channel(2);
            let rx = std::rc::Rc::new(std::cell::RefCell::new(rx));
            let (priority_tx, priority_rx) = tokio::sync::mpsc::channel(2);
            let priority_rx = std::rc::Rc::new(std::cell::RefCell::new(priority_rx));
            let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
            let (stop_lane, _stop_rx) = StopLane::new();
            let handle = ActorHandle::<Park>::new(
                ActorId::from("crash-answer-across-the-close".to_string()),
                tx,
                stop_lane,
                2,
                status_rx,
                "crash-answer-across-the-close".into(),
                Some(priority_tx),
                None,
            );
            let (closing, closing_lane) = (rx.clone(), priority_rx.clone());
            BEFORE_PUSH.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    closing.borrow_mut().close();
                    closing_lane.borrow_mut().close();
                }));
            });
            let mut call = Box::pin(form.call(&handle, Msg::Ping, FAR));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(
                std::future::Future::poll(call.as_mut(), &mut cx).is_pending(),
                "{form:?}: the call waits for the reason"
            );

            // The actor takes the request, answers it with the handler's error
            // at its seal, and writes the reason.
            let taken = if form.priority() {
                priority_rx.borrow_mut().try_recv().ok()
            } else {
                rx.borrow_mut().try_recv().ok()
            };
            let Some(Envelope::Message {
                responder: Some(reply),
                ..
            }) = taken
            else {
                panic!("{form:?}: the request was queued");
            };
            reply
                .send(Err(ActorError::User("the handler failed".to_string())))
                .unwrap();
            StatusValue::write_reason(
                &status_tx,
                StopReason::Failure(ActorError::User("the handler failed".to_string())),
                false,
            );
            let outcome = std::future::Future::poll(call.as_mut(), &mut cx);
            assert!(
                matches!(outcome, std::task::Poll::Ready(Err(AskError::Actor(_)))),
                "{form:?}: {outcome:?}"
            );
        }
    }
}
