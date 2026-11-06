//! The system plane in isolation: the order it answers requests in, how many
//! it queues, and what becomes of the requests and the reserved slots when
//! the run loop ends it. Nothing here is ordered by a sleep.

use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use super::*;
use crate::actor::runtime::end_queue;
use crate::types::{ActorId, ActorStatusInfo, StatusRequest, SystemMessage};

const BOUND: Duration = Duration::from_secs(5);

fn new_plane() -> (AbortRecord, SystemPlane) {
    let record = AbortRecord::new();
    let system = crate::system::ActorSystem::create(format!("plane-{}", uuid::Uuid::new_v4()))
        .expect("a fresh system");
    let plane = SystemPlane::new(
        record.clone(),
        ActorId::from("system-plane".to_string()),
        system,
    );
    (record, plane)
}

/// Polls `submit_status` once: whether the request was queued without
/// waiting. A request that has to wait is dropped, so it queues nothing.
fn submit_now(record: &AbortRecord, reply: oneshot::Sender<ActorStatusInfo>) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    let mut submit = std::pin::pin!(record.submit_status(reply));
    matches!(submit.as_mut().poll(&mut cx), Poll::Ready(Ok(())))
}

/// Takes the next request now, when one is ready.
fn take_request_now(plane: &mut SystemPlane) -> Option<StatusRequest> {
    let mut cx = Context::from_waker(Waker::noop());
    match plane.poll_recv(&mut cx) {
        Poll::Ready(Some(SystemMessage::GetStatus(request))) => Some(request),
        Poll::Ready(None) | Poll::Pending => None,
    }
}

/// Takes the next request's reply now, when one is ready.
fn take_now(plane: &mut SystemPlane) -> Option<oneshot::Sender<ActorStatusInfo>> {
    take_request_now(plane).map(|request| request.reply)
}

/// A request whose requester is gone reads closed, so a query can be told
/// from the next by whether its requester was dropped.
fn marked() -> (
    oneshot::Sender<ActorStatusInfo>,
    oneshot::Receiver<ActorStatusInfo>,
) {
    oneshot::channel()
}

/// Two first queries that arrive together are answered in arrival order,
/// even when both land between the run loop's read of the held bit and its
/// read of the channel bit in one poll.
#[test]
fn two_concurrent_first_queries_are_answered_in_arrival_order() {
    let (record, mut plane) = new_plane();
    let (first_tx, first_rx) = marked();
    let (second_tx, _second_rx) = marked();
    // The first query's requester is gone, which marks it.
    drop(first_rx);
    let slot = (first_tx, second_tx);
    let queued = record.clone();
    BETWEEN_LOADS.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            assert!(submit_now(&queued, slot.0), "the first query is held");
            assert!(submit_now(&queued, slot.1), "the second opens the channel");
        }));
    });
    let served_first = take_now(&mut plane).expect("a query is ready");
    assert!(
        served_first.is_closed(),
        "the second query was answered before the first"
    );
    let served_second = take_now(&mut plane).expect("the second query is ready");
    assert!(!served_second.is_closed());
    assert!(take_now(&mut plane).is_none());
}

/// The plane queues 64 requests in steady state, as the system channel of an
/// actor without a held slot does. Before the run loop has taken the request
/// held ahead of the channel, that request is queued beside the 64.
#[test]
fn the_plane_queues_sixty_four_in_steady_state() {
    let (record, mut plane) = new_plane();
    let mut cold = 0;
    loop {
        let (reply, _requester) = marked();
        if !submit_now(&record, reply) {
            break;
        }
        cold += 1;
    }
    let mut served = 0;
    while take_now(&mut plane).is_some() {
        served += 1;
    }
    assert_eq!(served, cold, "every queued request is served");

    let mut steady = 0;
    loop {
        let (reply, _requester) = marked();
        if !submit_now(&record, reply) {
            break;
        }
        steady += 1;
    }
    assert_eq!(cold, SYSTEM_CAPACITY + 1, "cold bound");
    assert_eq!(steady, SYSTEM_CAPACITY, "steady-state bound");
    assert_eq!(SYSTEM_CAPACITY, 64);
}

/// A request held outside the channel, in either slot state, and a request
/// queued in the channel, are dropped when the plane ends, whether or not the
/// run loop ever polled the plane.
#[tokio::test]
async fn requests_are_dropped_when_the_plane_ends() {
    // One request, held in the slot.
    let (record, plane) = new_plane();
    let (reply, requester) = marked();
    assert!(submit_now(&record, reply));
    drop(plane);
    assert!(timeout(BOUND, requester).await.unwrap().is_err());

    // Two requests: one held beside the channel, one in it; the plane never
    // polled.
    let (record, plane) = new_plane();
    let (first, first_requester) = marked();
    let (second, second_requester) = marked();
    assert!(submit_now(&record, first));
    assert!(submit_now(&record, second));
    drop(plane);
    assert!(timeout(BOUND, first_requester).await.unwrap().is_err());
    assert!(timeout(BOUND, second_requester).await.unwrap().is_err());

    // The receiver already taken by the plane, with a request queued in it.
    let (record, mut plane) = new_plane();
    let (first, first_requester) = marked();
    let (second, second_requester) = marked();
    let (third, third_requester) = marked();
    assert!(submit_now(&record, first));
    assert!(submit_now(&record, second));
    assert!(submit_now(&record, third));
    drop(take_now(&mut plane).expect("the held request"));
    drop(take_now(&mut plane).expect("the second request"));
    assert!(timeout(BOUND, first_requester).await.unwrap().is_err());
    assert!(timeout(BOUND, second_requester).await.unwrap().is_err());
    drop(plane);
    assert!(timeout(BOUND, third_requester).await.unwrap().is_err());

    // A request refused once the plane ended.
    let (reply, _requester) = marked();
    assert!(!submit_now(&record, reply));
}

/// A slot reserved on the channel before the plane ends is used after it: the
/// value is dropped by the crate while the sender that reserved it is still
/// alive, and the requester is answered.
macro_rules! reserved_slot_test {
    ($name:ident, $flavor:literal, $taken:expr) => {
        #[tokio::test(flavor = $flavor)]
        async fn $name() {
            let (record, mut plane) = new_plane();
            let (first, _first_requester) = marked();
            let (second, _second_requester) = marked();
            assert!(submit_now(&record, first));
            assert!(submit_now(&record, second));
            if $taken {
                // The plane's polls take the held request, then the
                // channel's receiver from the slot.
                drop(take_now(&mut plane));
                drop(take_now(&mut plane));
            }
            let tx = record.system_tx_for_test().expect("the channel is open");
            let permit = tx.reserve_owned().await.unwrap();
            drop(plane);
            let (late, late_requester) = marked();
            let _sender = permit.send(SystemMessage::GetStatus(StatusRequest {
                reply: late,
                task: false,
            }));
            assert!(
                timeout(BOUND, late_requester).await.unwrap().is_err(),
                "the late request must be dropped while its sender is alive"
            );
        }
    };
}

reserved_slot_test!(
    a_slot_reserved_before_the_plane_ends_in_the_slot_ct,
    "current_thread",
    false
);
reserved_slot_test!(
    a_slot_reserved_before_the_plane_ends_in_the_slot_mt,
    "multi_thread",
    false
);
reserved_slot_test!(
    a_slot_reserved_before_the_plane_ends_taken_ct,
    "current_thread",
    true
);
reserved_slot_test!(
    a_slot_reserved_before_the_plane_ends_taken_mt,
    "multi_thread",
    true
);

/// A slot given back unused after the plane ended leaves nothing behind that
/// can fail.
#[tokio::test]
async fn a_slot_given_back_after_the_plane_ended_is_harmless() {
    let (record, plane) = new_plane();
    let (first, _first_requester) = marked();
    let (second, _second_requester) = marked();
    assert!(submit_now(&record, first));
    assert!(submit_now(&record, second));
    let tx = record.system_tx_for_test().expect("the channel is open");
    let permit = tx.reserve_owned().await.unwrap();
    drop(plane);
    drop(permit);
    let (reply, _requester) = marked();
    assert!(!submit_now(&record, reply));
}

/// `end_queue` ends a receiver with no slot out without a task.
#[test]
fn a_queue_with_no_slot_out_ends_in_place() {
    let (tx, rx) = tokio::sync::mpsc::channel::<SystemMessage>(4);
    let (reply, _requester) = marked();
    tx.try_send(SystemMessage::GetStatus(StatusRequest {
        reply,
        task: false,
    }))
    .unwrap();
    assert!(end_queue(rx, &ActorId::from("queue".to_string())).is_none());
    assert!(tx.is_closed());
}

/// Queues one status request without waiting, as a requester inside a runtime
/// task (`task`) or outside every task (the body of `block_on`). The returned
/// receiver keeps the requester alive.
fn submit_as(
    rt: &tokio::runtime::Runtime,
    record: &AbortRecord,
    task: bool,
) -> oneshot::Receiver<ActorStatusInfo> {
    let (reply, requester) = marked();
    let queued = if task {
        let record = record.clone();
        rt.block_on(async move { tokio::spawn(async move { submit_now(&record, reply) }).await })
            .unwrap()
    } else {
        rt.block_on(async {
            assert!(tokio::task::try_id().is_none(), "the body is not a task");
            submit_now(record, reply)
        })
    };
    assert!(queued, "the plane has room");
    requester
}

fn current_thread_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

/// The kinds of the requests the plane hands out, in the order it hands them.
fn kinds_taken(plane: &mut SystemPlane) -> Vec<bool> {
    let mut kinds = Vec::new();
    while let Some(request) = take_request_now(plane) {
        kinds.push(request.task);
    }
    kinds
}

/// A request held outside the channel keeps the kind of its own requester.
#[test]
fn a_held_request_keeps_its_requester_kind() {
    let rt = current_thread_rt();
    for task in [true, false] {
        let (record, mut plane) = new_plane();
        let _requester = submit_as(&rt, &record, task);
        assert_eq!(kinds_taken(&mut plane), [task]);
    }
}

/// The request held when the channel opens and the request that opens it each
/// keep their own kind, in either order of kinds.
#[test]
fn the_request_held_before_the_channel_opened_keeps_its_own_kind() {
    let rt = current_thread_rt();
    for (held, opening) in [(true, false), (false, true), (true, true), (false, false)] {
        let (record, mut plane) = new_plane();
        let _held = submit_as(&rt, &record, held);
        let _opening = submit_as(&rt, &record, opening);
        assert!(record.system_tx_for_test().is_some(), "the channel is open");
        assert_eq!(
            kinds_taken(&mut plane),
            [held, opening],
            "held by a {held} requester, then opened by a {opening} one"
        );
    }
}

/// Requests in the channel keep their own kinds, in arrival order.
#[test]
fn queued_requests_keep_their_own_kinds_in_arrival_order() {
    let rt = current_thread_rt();
    let (record, mut plane) = new_plane();
    let kinds = [false, true, true, false, true, false];
    let _requesters: Vec<_> = kinds
        .iter()
        .map(|task| submit_as(&rt, &record, *task))
        .collect();
    assert_eq!(kinds_taken(&mut plane), kinds);
}

/// With an outside request queued before a task request, the reply to the
/// outside one reads outside and the reply to the task one reads task.
#[test]
fn an_outside_request_queued_before_a_task_request_reads_outside() {
    let rt = current_thread_rt();
    let (record, mut plane) = new_plane();
    let _outside = submit_as(&rt, &record, false);
    let _task = submit_as(&rt, &record, true);
    let first = take_request_now(&mut plane).unwrap();
    let second = take_request_now(&mut plane).unwrap();
    assert!(!first.task, "the reply to the outside requester reads task");
    assert!(second.task, "the reply to the task requester reads outside");
}

/// A task's request dropped while it waits for room is queued nowhere, so it
/// leaves nothing for the next reply to read.
#[test]
fn a_dropped_waiting_task_request_leaves_no_kind_behind() {
    let rt = current_thread_rt();
    let (record, mut plane) = new_plane();
    let mut held = Vec::new();
    loop {
        let (reply, requester) = marked();
        if !submit_now(&record, reply) {
            break;
        }
        held.push(requester);
    }
    // The plane is full: the task's request waits, and is dropped.
    let waiting = record.clone();
    rt.block_on(async move {
        tokio::spawn(async move {
            let (reply, _requester) = marked();
            assert!(!submit_now(&waiting, reply), "the plane is full");
        })
        .await
        .unwrap();
    });
    while take_now(&mut plane).is_some() {}
    let _outside = submit_as(&rt, &record, false);
    assert_eq!(
        kinds_taken(&mut plane),
        [false],
        "an outside requester's reply reads the kind of a dropped task request"
    );
}

/// Two first queries that land between the run loop's two reads of the plane
/// keep their own kinds, and the one handed over from the slot first is the
/// older.
#[test]
fn queries_landing_between_the_two_reads_keep_their_own_kinds() {
    for (older, newer) in [(true, false), (false, true)] {
        let (record, mut plane) = new_plane();
        let queued = record.clone();
        BETWEEN_LOADS.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let rt = current_thread_rt();
                let _older = submit_as(&rt, &queued, older);
                let _newer = submit_as(&rt, &queued, newer);
            }));
        });
        assert_eq!(kinds_taken(&mut plane), [older, newer]);
    }
}

// ---------------------------------------------------------------------------
// The requesters' wakers
// ---------------------------------------------------------------------------

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: std::sync::atomic::AtomicUsize,
}

impl LoudWake {
    fn arm(&self, armed: bool) {
        self.armed.store(armed, Ordering::SeqCst);
    }

    fn woken(&self) -> usize {
        self.woken.load(Ordering::SeqCst)
    }
}

impl std::task::Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a requester's waker panics");
        }
    }
}

/// Queues a status request whose requester has polled for the reply with a
/// waker that panics when it is woken.
fn loud_request(record: &AbortRecord) -> (Arc<LoudWake>, oneshot::Receiver<ActorStatusInfo>) {
    let loud = Arc::new(LoudWake::default());
    let (reply, mut requester) = marked();
    assert!(submit_now(record, reply), "the plane has room");
    let waker = Waker::from(loud.clone());
    assert!(std::pin::Pin::new(&mut requester)
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    (loud, requester)
}

/// The plane ends with `requests` loud requests, the first held beside the
/// channel and the others queued in it, while the thread it is dropped on is
/// unwinding when `unwinding`. A panic of a requester's waker stays in the
/// drop, whichever place the request is in.
fn end_the_plane_with_loud_requests(requests: usize, unwinding: bool) {
    let (record, plane) = new_plane();
    let loud: Vec<_> = (0..requests).map(|_| loud_request(&record)).collect();
    for (waker, _) in &loud {
        waker.arm(true);
    }
    if unwinding {
        let unwound = std::thread::spawn(move || {
            let _owned = plane;
            panic!("the thread unwinds");
        })
        .join();
        assert!(unwound.is_err(), "the thread ended by its own panic");
    } else {
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(plane)));
        assert!(
            dropped.is_ok(),
            "a requester's waker unwound out of the plane's drop"
        );
    }
    for (waker, requester) in loud {
        waker.arm(false);
        assert_eq!(waker.woken(), 1, "every requester was woken once");
        let mut requester = requester;
        assert!(
            std::pin::Pin::new(&mut requester)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready(),
            "the requester reads the end of the plane"
        );
    }
}

/// One request held outside the channel, with no channel opened: its
/// requester's waker panics when the plane drops it.
#[test]
fn a_held_request_whose_requesters_waker_panics_is_dropped_without_unwinding_the_plane() {
    end_the_plane_with_loud_requests(1, false);
}

/// One request held beside the channel the second request opened, and that
/// second request queued in it: both requesters' wakers panic. A panic that
/// escaped the drop of the held request would meet the queued request's panic
/// during its unwind and end the process, so the scenario runs in a child
/// process.
#[test]
fn a_request_held_beside_the_channel_and_a_queued_one_whose_wakers_panic_are_dropped_without_unwinding_the_plane(
) {
    if !crate::actor::panic::in_a_child_process(
        module_path!(),
        "a_request_held_beside_the_channel_and_a_queued_one_whose_wakers_panic_are_dropped_without_unwinding_the_plane",
    ) {
        return;
    }
    end_the_plane_with_loud_requests(2, false);
}

/// The plane is dropped by a thread that is already unwinding, with a held
/// request and with a held and a queued one: a panic that escaped the drop
/// would be a second panic during the unwind and end the process, so each
/// scenario runs in a child process.
#[test]
fn a_held_request_whose_requesters_waker_panics_does_not_end_the_process_when_the_plane_drops_during_an_unwind(
) {
    if !crate::actor::panic::in_a_child_process(
        module_path!(),
        "a_held_request_whose_requesters_waker_panics_does_not_end_the_process_when_the_plane_drops_during_an_unwind",
    ) {
        return;
    }
    end_the_plane_with_loud_requests(1, true);
}

/// As above, with the request held beside the channel and one queued in it.
#[test]
fn a_held_and_a_queued_request_whose_requesters_wakers_panic_do_not_end_the_process_when_the_plane_drops_during_an_unwind(
) {
    if !crate::actor::panic::in_a_child_process(
        module_path!(),
        "a_held_and_a_queued_request_whose_requesters_wakers_panic_do_not_end_the_process_when_the_plane_drops_during_an_unwind",
    ) {
        return;
    }
    end_the_plane_with_loud_requests(2, true);
}
