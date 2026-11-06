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
fn a_mailbox_request_reads_the_mailbox_it_pushed_to() {
    request_between_the_queue_ends(Form::Send);
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
}

// ---------------------------------------------------------------------------
// The answer to a request whose handler failed.
// ---------------------------------------------------------------------------

mod crash_answer {
    use super::*;
    use std::sync::atomic::AtomicBool;

    use tokio::sync::{Notify, Semaphore};

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
