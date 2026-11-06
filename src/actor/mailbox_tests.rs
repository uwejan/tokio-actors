//! Tests of the lane mailbox: the exact bound, the order each task keeps, the
//! grants of freed units, cancellation at every await, the close and the
//! drain, the late-slot end of a queue with a unit still out, wakers that
//! panic, and ends that race pushes still landing.
//!
//! No step is ordered by a sleep: a wait for a state polls it, and a timeout
//! only detects a hang.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::time::timeout;

use super::*;
use crate::actor::panic::payload_into_string;
use crate::actor::runtime;
use crate::system::ActorSystem;
use crate::types::ActorId;

/// The longest a wait may take; it only limits how long a real hang takes to
/// report.
const HANG: Duration = Duration::from_secs(60);

/// Polls a future once with a waker that does nothing.
fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    let mut cx = Context::from_waker(Waker::noop());
    Pin::new(future).poll(&mut cx)
}

/// A reservation that takes the given lane, so a test places its waiters.
fn reserve_on<T>(tx: &MailTx<T>, lane: usize) -> Reserve<'_, T> {
    let mut reserve = tx.reserve();
    reserve.lane_override = Some(lane);
    reserve
}

/// A unit taken at once for the given lane.
fn permit_on<T>(tx: &MailTx<T>, _lane: usize) -> Option<Permit<'_, T>> {
    tx.shared.take_unit().then(|| Permit::new(tx))
}

/// Counts how often it is woken.
struct CountWake(AtomicUsize);

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A waker of its own and the count of its wakes.
fn counting_waker() -> (Arc<CountWake>, Waker) {
    let count = Arc::new(CountWake(AtomicUsize::new(0)));
    (count.clone(), Waker::from(count))
}

fn wakes(count: &CountWake) -> usize {
    count.0.load(Ordering::SeqCst)
}

/// Polls a future once with the given waker.
fn poll_with<F: Future + Unpin>(future: &mut F, waker: &Waker) -> Poll<F::Output> {
    let mut cx = Context::from_waker(waker);
    Pin::new(future).poll(&mut cx)
}

type Hook = Box<dyn FnOnce()>;

thread_local! {
    static AT_REBALANCE: RefCell<Option<Hook>> = const { RefCell::new(None) };
    static IN_GRANT_SCAN: RefCell<Option<(usize, Hook)>> = const { RefCell::new(None) };
}

/// Runs the hook a test set for the next rebalance of this thread, once, as
/// the rebalance starts: a sender can arrive at that instant.
pub(super) fn at_rebalance() {
    // A thread that is ending has no hooks left.
    if let Ok(Some(hook)) = AT_REBALANCE.try_with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

/// Runs the hook a test set for the grant scan of this thread, once, when the
/// scan reaches the list it names by its position: the lists before it are
/// behind the scan, the lists from it on are still ahead.
pub(super) fn in_grant_scan() {
    let due = IN_GRANT_SCAN.try_with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.take() {
            Some((1, hook)) => Some(hook),
            Some((left, hook)) => {
                *slot = Some((left.saturating_sub(1), hook));
                None
            }
            None => None,
        }
    });
    if let Ok(Some(hook)) = due {
        hook();
    }
}

/// Removes the hooks this thread's test set.
fn clear_hooks() {
    AT_REBALANCE.with(|slot| slot.borrow_mut().take());
    IN_GRANT_SCAN.with(|slot| slot.borrow_mut().take());
}

async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let waited = timeout(HANG, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(waited.is_ok(), "{what}");
}

fn waiting<T>(tx: &MailTx<T>) -> usize {
    tx.shared.waiting_total.load(Ordering::SeqCst)
}

/// Whether the mailbox is whole again: every unit in the room, no waiter, no
/// message.
fn at_rest<T>(shared: &Shared<T>) -> bool {
    shared.room.0.load(Ordering::SeqCst) == shared.capacity
        && shared.waiting_total.load(Ordering::SeqCst) == 0
        && (0..shared.lane_count)
            .filter_map(|index| shared.try_lane(index))
            .all(|lane| lane.waiting.load(Ordering::SeqCst) == 0)
}

/// Whether no lock of the mailbox is held (or poisoned) by anyone now.
fn no_lock_is_held<T>(shared: &Shared<T>) -> bool {
    (0..shared.lane_count)
        .filter_map(|index| shared.try_lane(index))
        .all(|lane| lane.waiters.try_lock().is_ok())
        && shared.reader.waker.try_lock().is_ok()
}

// ---------------------------------------------------------------------------
// The bound, the lanes and the memory
// ---------------------------------------------------------------------------

#[test]
fn lanes_follow_the_runtime() {
    let lanes = |workers: Option<usize>| {
        let mut builder = match workers {
            None => tokio::runtime::Builder::new_current_thread(),
            Some(count) => {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.worker_threads(count);
                builder
            }
        };
        let runtime = builder.enable_all().build().unwrap();
        runtime.block_on(async {
            lanes_for(tokio::runtime::Handle::current().metrics().num_workers())
        })
    };
    assert_eq!(lanes(None), 1, "a current-thread runtime gets one lane");
    assert_eq!(lanes(Some(1)), 1, "one worker gets one lane");
    assert_eq!(lanes(Some(4)), MAILBOX_LANES);
}

// ---------------------------------------------------------------------------
// Order and totals under many senders
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_task_keeps_its_order_under_many_senders() {
    const TASKS: usize = 24;
    const EACH: usize = 3000;
    let (tx, mut rx) = channel::<(usize, usize)>(8, MAILBOX_LANES, None);
    let mut joins = Vec::new();
    for task in 0..TASKS {
        let tx = tx.clone();
        joins.push(tokio::spawn(async move {
            for seq in 0..EACH {
                tx.reserve().await.unwrap().send((task, seq));
            }
        }));
    }
    let probe = tx.shared.clone();
    drop(tx);
    let mut next = [0usize; TASKS];
    let mut total = 0;
    timeout(HANG, async {
        while let Some((task, seq)) = rx.recv().await {
            assert_eq!(next[task], seq, "task {task} is out of order");
            next[task] += 1;
            total += 1;
        }
    })
    .await
    .expect("the mailbox ends once the senders are gone");
    for join in joins {
        join.await.unwrap();
    }
    assert_eq!(total, TASKS * EACH);
    assert!(at_rest(&probe));
}

// ---------------------------------------------------------------------------
// Grants, cancellation and the waiters' queues
// ---------------------------------------------------------------------------

#[test]
fn waiters_of_one_lane_are_granted_in_arrival_order() {
    let (tx, mut rx) = channel::<u32>(1, MAILBOX_LANES, None);
    tx.try_send(0).unwrap();
    let mut a = reserve_on(&tx, 2);
    let mut b = reserve_on(&tx, 2);
    let mut c = reserve_on(&tx, 2);
    assert!(poll_once(&mut a).is_pending());
    assert!(poll_once(&mut b).is_pending());
    assert!(poll_once(&mut c).is_pending());
    assert_eq!(waiting(&tx), 3);

    assert_eq!(rx.try_recv().unwrap(), 0);
    assert_eq!(tx.capacity(), 0, "the freed unit went to the first waiter");
    let Poll::Ready(Ok(permit)) = poll_once(&mut a) else {
        panic!("the first waiter is granted");
    };
    assert!(poll_once(&mut b).is_pending());
    permit.send(1);
    assert_eq!(rx.try_recv().unwrap(), 1);
    let Poll::Ready(Ok(permit)) = poll_once(&mut b) else {
        panic!("the second waiter is granted");
    };
    assert!(poll_once(&mut c).is_pending());
    permit.send(2);
    assert_eq!(rx.try_recv().unwrap(), 2);
    let Poll::Ready(Ok(permit)) = poll_once(&mut c) else {
        panic!("the third waiter is granted");
    };
    drop(permit);
    assert!(at_rest(&tx.shared));
}

#[test]
fn a_granted_waiter_dropped_passes_the_unit_to_a_waiter_of_another_lane() {
    let (tx, _rx) = channel::<u32>(1, MAILBOX_LANES, None);
    let held = permit_on(&tx, 0).unwrap();
    let mut a = reserve_on(&tx, 3);
    let mut b = reserve_on(&tx, 5);
    assert!(poll_once(&mut a).is_pending());
    assert!(poll_once(&mut b).is_pending());
    drop(held);
    // One of the two holds the grant; dropping its reservation passes the unit
    // on to the other lane's waiter.
    let a_has_it = matches!(
        &a.step,
        Step::Parked { waiter, .. } if waiter.state.load(Ordering::SeqCst) == GRANTED
    );
    let (granted, mut other) = if a_has_it { (a, b) } else { (b, a) };
    drop(granted);
    assert!(
        matches!(poll_once(&mut other), Poll::Ready(Ok(_))),
        "the unit went on to the other lane's waiter"
    );
}

// ---------------------------------------------------------------------------
// Close and drain
// ---------------------------------------------------------------------------

/// A sender that has read the flag as open under its lane's lock but has not
/// queued itself yet is woken with `Closed` by a close that runs in that
/// window, whatever the lane's count reads then.
///
/// The closer runs on its own thread from inside the park. It sets the flag at
/// once; if it also finishes while the park still holds the lane's lock, it
/// passed the lane without the lock, and the entry queued afterwards would
/// wait for a unit that never comes back. The bounded poll for that only
/// decides how long a closer that is blocked on the lock is given.
#[test]
fn a_close_between_the_parks_flag_read_and_its_entry_wakes_the_waiter() {
    let (tx, _rx) = channel::<u32>(1, MAILBOX_LANES, None);
    let held = permit_on(&tx, 0).unwrap();
    let closer: Arc<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let shared = tx.shared.clone();
    let hook_closer = closer.clone();
    *shared.park_hook.lock().unwrap() = Some(Box::new({
        let shared = shared.clone();
        move || {
            let done = Arc::new(AtomicBool::new(false));
            let flag = done.clone();
            let target = shared.clone();
            let handle = std::thread::spawn(move || {
                target.close();
                flag.store(true, Ordering::SeqCst);
            });
            *hook_closer.lock().unwrap() = Some(handle);
            let start = std::time::Instant::now();
            while !done.load(Ordering::SeqCst) && start.elapsed() < Duration::from_millis(200) {
                std::hint::spin_loop();
            }
        }
    }));
    let mut reserve = reserve_on(&tx, 0);
    let first = poll_once(&mut reserve);
    let handle = closer.lock().unwrap().take().expect("the hook ran");
    handle.join().unwrap();
    let outcome = match first {
        Poll::Ready(outcome) => outcome.map(|_| ()),
        Poll::Pending => match poll_once(&mut reserve) {
            Poll::Ready(outcome) => outcome.map(|_| ()),
            Poll::Pending => panic!("the waiter queued during the close was never woken"),
        },
    };
    assert_eq!(outcome, Err(Closed));
    drop(reserve);
    assert_eq!(waiting(&tx), 0);
    drop(held);
    assert!(at_rest(&tx.shared));
}

#[test]
fn a_waiter_granted_before_the_close_returns_its_unit() {
    let (tx, mut rx) = channel::<u32>(1, MAILBOX_LANES, None);
    let held = permit_on(&tx, 0).unwrap();
    let mut waiter = reserve_on(&tx, 6);
    assert!(poll_once(&mut waiter).is_pending());
    drop(held);
    rx.close();
    assert!(matches!(poll_once(&mut waiter), Poll::Ready(Err(Closed))));
    assert!(at_rest(&tx.shared), "the granted unit came back");
    assert!(rx.shared.ended());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_racing_pushes_into_unused_lanes_loses_nothing() {
    for round in 0..3000 {
        let (tx, mut rx) = channel::<usize>(MAILBOX_LANES, MAILBOX_LANES, None);
        let permits: Vec<_> = (0..MAILBOX_LANES)
            .map(|lane| permit_on(&tx, lane).unwrap())
            .collect();
        let seen_closed: Vec<AtomicBool> =
            (0..MAILBOX_LANES).map(|_| AtomicBool::new(false)).collect();
        let seen_closed = Arc::new(seen_closed);
        let tx_ref = &tx;
        let mut drained = Vec::new();
        std::thread::scope(|scope| {
            for (lane, permit) in permits.into_iter().enumerate() {
                let seen_closed = seen_closed.clone();
                scope.spawn(move || {
                    for _ in 0..round % 5 {
                        std::thread::yield_now();
                    }
                    permit.send(lane);
                    std::sync::atomic::fence(Ordering::SeqCst);
                    if tx_ref.is_closed() {
                        seen_closed[lane].store(true, Ordering::SeqCst);
                    }
                });
            }
            rx.close();
            std::sync::atomic::fence(Ordering::SeqCst);
            while let Ok(message) = rx.try_recv() {
                drained.push(message);
            }
        });
        for lane in 0..MAILBOX_LANES {
            assert!(
                drained.contains(&lane) || seen_closed[lane].load(Ordering::SeqCst),
                "round {round}: the message of lane {lane} was left unseen by both sides"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Wakeups of the reader
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The cooperative budget
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_sender_loop_with_room_gives_other_tasks_their_turn() {
    let (tx, rx) = channel::<u64>(1 << 22, 1, None);
    let ran = Arc::new(AtomicBool::new(false));
    let other = tokio::spawn({
        let ran = ran.clone();
        async move { ran.store(true, Ordering::SeqCst) }
    });
    let mut sent = 0u64;
    while !ran.load(Ordering::SeqCst) && sent < 1_000_000 {
        tx.reserve().await.unwrap().send(sent);
        sent += 1;
    }
    assert!(
        ran.load(Ordering::SeqCst),
        "the other task never ran in {sent} sends"
    );
    other.await.unwrap();
    drop(rx);
}

// ---------------------------------------------------------------------------
// User code never runs under a lock of the mailbox
// ---------------------------------------------------------------------------

/// A message that checks, when it is dropped, that no lock of its mailbox is
/// held, and can panic in its drop.
struct Probe {
    shared: Option<Arc<Shared<Probe>>>,
    panics: bool,
    clean: Arc<AtomicUsize>,
    dirty: Arc<AtomicUsize>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            let counter = if no_lock_is_held(&shared) {
                &self.clean
            } else {
                &self.dirty
            };
            counter.fetch_add(1, Ordering::SeqCst);
        }
        if self.panics {
            panic!("a message's drop panics");
        }
    }
}

struct Counts {
    clean: Arc<AtomicUsize>,
    dirty: Arc<AtomicUsize>,
}

impl Counts {
    fn new() -> Self {
        Self {
            clean: Arc::new(AtomicUsize::new(0)),
            dirty: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn probe(&self, tx: &MailTx<Probe>, panics: bool) -> Probe {
        Probe {
            shared: Some(tx.shared.clone()),
            panics,
            clean: self.clean.clone(),
            dirty: self.dirty.clone(),
        }
    }
}

#[test]
fn a_message_is_dropped_outside_every_lock_on_every_path() {
    let counts = Counts::new();
    let (tx, mut rx) = channel::<Probe>(2, MAILBOX_LANES, None);
    tx.try_send(counts.probe(&tx, true)).unwrap();
    tx.try_send(counts.probe(&tx, true)).unwrap();
    // A refused message comes back to the caller.
    let Err(TrySendError::Full(refused)) = tx.try_send(counts.probe(&tx, true)) else {
        panic!("the mailbox is full");
    };
    assert!(catch_unwind(AssertUnwindSafe(move || drop(refused))).is_err());
    // A taken message is the caller's to drop.
    let taken = rx.try_recv().unwrap();
    assert!(catch_unwind(AssertUnwindSafe(move || drop(taken))).is_err());
    assert!(
        no_lock_is_held(&tx.shared),
        "a panicking drop left a lock behind"
    );
    // The rest drops with the receiver, every panic caught.
    assert!(catch_unwind(AssertUnwindSafe(move || drop(rx))).is_ok());
    assert_eq!(counts.dirty.load(Ordering::SeqCst), 0);
    assert_eq!(counts.clean.load(Ordering::SeqCst), 3);
    // The lanes still work after the panics.
    assert!(matches!(
        tx.try_send(counts.probe(&tx, false)),
        Err(TrySendError::Closed(_))
    ));
}

// ---------------------------------------------------------------------------
// The end of a queue with a unit still out
// ---------------------------------------------------------------------------

fn fresh_system(tag: &str) -> Arc<ActorSystem> {
    ActorSystem::create(format!("mailbox-{tag}-{}", uuid::Uuid::new_v4())).unwrap()
}

/// The ends the helper slot of the current runtime recorded for late drains.
fn late_drain_ends(system: &ActorSystem) -> usize {
    system
        .helper_ends(&tokio::runtime::Handle::current())
        .into_iter()
        .filter(|(kind, _)| *kind == crate::helper::HelperKind::LateDrain)
        .count()
}

struct Bomb;

impl Drop for Bomb {
    fn drop(&mut self) {
        panic!("a late value's drop panics");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_value_whose_drop_panics_is_contained_in_the_push() {
    let system = fresh_system("bomb");
    let id = ActorId::from("bomb".to_string());
    let (tx, rx) = channel::<Bomb>(1, MAILBOX_LANES, None);
    let permit = tx.reserve().await.unwrap();
    assert!(runtime::end_queue_late(rx, &system, &id).is_none());
    // The push returns: the panic of the drop stays inside it.
    permit.send(Bomb);
    assert!(at_rest(&tx.shared), "the unit is back in the room");
    assert_eq!(system.live_helpers(), 0);
    assert_eq!(late_drain_ends(&system), 0);
}

// ---------------------------------------------------------------------------
// Grant order, the race windows of a release and the receiver's end
// ---------------------------------------------------------------------------

#[test]
fn grants_rotate_over_the_wait_lists_so_a_later_list_is_not_passed_over() {
    let (tx, _rx) = channel::<u32>(1, MAILBOX_LANES, None);
    let held = permit_on(&tx, 0).unwrap();
    let mut first = reserve_on(&tx, 0);
    let mut second = reserve_on(&tx, 0);
    let mut other = reserve_on(&tx, 3);
    assert!(poll_once(&mut first).is_pending());
    assert!(poll_once(&mut second).is_pending());
    assert!(poll_once(&mut other).is_pending());
    drop(held);
    let Poll::Ready(Ok(permit)) = poll_once(&mut first) else {
        panic!("the first grant goes to the first waiter of list 0");
    };
    // The next freed unit continues the round at the next list: list 3 has a
    // waiter, and list 0 has one too that arrived earlier.
    drop(permit);
    let Poll::Ready(Ok(reached)) = poll_once(&mut other) else {
        panic!("the second grant must reach list 3, not return to list 0");
    };
    assert!(poll_once(&mut second).is_pending());
    drop(reached);
}

#[test]
fn a_sender_arriving_as_a_unit_is_freed_cannot_take_it_from_a_waiter() {
    let (tx, _rx) = channel::<u32>(1, MAILBOX_LANES, None);
    let tx: &'static MailTx<u32> = Box::leak(Box::new(tx));
    let held = permit_on(tx, 0).unwrap();
    let mut waiter = reserve_on(tx, 2);
    assert!(poll_once(&mut waiter).is_pending());
    let passed = Rc::new(Cell::new(false));
    AT_REBALANCE.with(|slot| {
        let passed = passed.clone();
        *slot.borrow_mut() = Some(Box::new(move || passed.set(tx.try_send(9).is_ok())));
    });
    drop(held);
    clear_hooks();
    assert!(
        !passed.get(),
        "a sender that arrived while the unit was freed took it ahead of the waiter"
    );
    assert!(
        matches!(poll_once(&mut waiter), Poll::Ready(Ok(_))),
        "the waiter holds the freed unit"
    );
}

#[test]
fn a_freed_unit_finds_a_sender_that_parked_in_a_list_the_grant_scan_had_passed() {
    let (tx, _rx) = channel::<u32>(1, MAILBOX_LANES, None);
    let tx: &'static MailTx<u32> = Box::leak(Box::new(tx));
    let held = permit_on(tx, 0).unwrap();
    let leaving = Rc::new(RefCell::new(Some(reserve_on(tx, 5))));
    let late: Rc<RefCell<Option<Reserve<'static, u32>>>> = Rc::new(RefCell::new(None));
    assert!(poll_once(leaving.borrow_mut().as_mut().unwrap()).is_pending());
    // The grant scan starts at list 3 and reaches list 5 as its third list.
    tx.shared.next_grant.store(3, Ordering::SeqCst);
    IN_GRANT_SCAN.with(|slot| {
        let leaving = leaving.clone();
        let late = late.clone();
        *slot.borrow_mut() = Some((
            3,
            Box::new(move || {
                // The waiter the scan is heading for leaves, and a sender
                // parks in a list the scan has already passed.
                drop(leaving.borrow_mut().take());
                let mut parked = reserve_on(tx, 3);
                assert!(poll_once(&mut parked).is_pending());
                *late.borrow_mut() = Some(parked);
            }),
        ));
    });
    drop(held);
    clear_hooks();
    let mut late = late.borrow_mut().take().expect("the hook ran");
    assert!(
        matches!(poll_once(&mut late), Poll::Ready(Ok(_))),
        "the freed unit sleeps in the room beside a sender that waits"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn senders_that_wait_together_spread_over_the_wait_lists() {
    const SENDERS: usize = 64;
    let (tx, mut rx) = channel::<u32>(1, MAILBOX_LANES, None);
    let held = permit_on(&tx, 0).unwrap();
    let mut joins = Vec::new();
    for _ in 0..SENDERS {
        let tx = tx.clone();
        joins.push(tokio::spawn(async move { tx.reserve().await.is_err() }));
    }
    until("every sender waits", || waiting(&tx) == SENDERS).await;
    let used = (0..MAILBOX_LANES)
        .filter_map(|index| tx.shared.try_lane(index))
        .filter(|lane| lane.waiting.load(Ordering::SeqCst) > 0)
        .count();
    rx.close();
    for join in joins {
        assert!(timeout(HANG, join).await.unwrap().unwrap());
    }
    drop(held);
    assert!(
        used >= MAILBOX_LANES / 2,
        "{SENDERS} waiting tasks used {used} of {MAILBOX_LANES} lists"
    );
}

#[test]
fn a_sender_that_parks_after_the_close_is_refused_by_the_list_it_parks_in() {
    let (tx, mut rx) = channel::<u32>(1, MAILBOX_LANES, None);
    let held = permit_on(&tx, 0).unwrap();
    // The sender read the flag as open, the close ran, and only then does it
    // reach the list.
    rx.close();
    let cx = Context::from_waker(Waker::noop());
    let parked = tx.shared.park(3, &cx);
    assert!(
        matches!(parked, Parked::Closed),
        "a sender queued itself in a list the close had already walked"
    );
    assert_eq!(waiting(&tx), 0);
    drop(held);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_end_of_a_queue_gives_back_the_unit_of_every_message_it_drains() {
    let id = ActorId::from("drain-units".to_string());
    let (tx, rx) = channel::<u32>(4, MAILBOX_LANES, None);
    for message in 0..3 {
        tx.try_send(message).unwrap();
    }
    assert_eq!(tx.capacity(), 1);
    assert!(runtime::end_queue(rx, &id).is_none());
    assert_eq!(tx.capacity(), 4, "the drained messages kept their units");

    let system = fresh_system("drain-units");
    let (tx, rx) = channel::<u32>(4, MAILBOX_LANES, None);
    for message in 0..3 {
        tx.try_send(message).unwrap();
    }
    assert!(runtime::end_queue_late(rx, &system, &id).is_none());
    assert_eq!(tx.capacity(), 4, "the late end kept the units of its drain");
    assert!(at_rest(&tx.shared));
}

#[test]
fn a_closed_mailbox_with_a_unit_out_is_not_the_end_for_the_receiver() {
    let (tx, mut rx) = channel::<u32>(2, MAILBOX_LANES, None);
    let permit = permit_on(&tx, 0).unwrap();
    rx.close();
    let (count, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(
        rx.poll_recv(&mut cx).is_pending(),
        "the receiver read the end while a permit could still push"
    );
    permit.send(7);
    assert_eq!(wakes(&count), 1, "the push wakes the receiver");
    assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(Some(7))));
    assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(None)));
}

// ---------------------------------------------------------------------------
// Wakers that panic
// ---------------------------------------------------------------------------

/// A waker that counts its wakes and panics in them while it is armed.
struct LoudWake {
    armed: AtomicBool,
    woken: AtomicUsize,
}

impl LoudWake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(false),
            woken: AtomicUsize::new(0),
        })
    }

    fn arm(&self, armed: bool) {
        self.armed.store(armed, Ordering::SeqCst);
    }

    fn woken(&self) -> usize {
        self.woken.load(Ordering::SeqCst)
    }
}

impl Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a waker's wake panics");
        }
    }
}

/// A waker whose drop panics while the flag is set.
struct LoudDrop {
    armed: Arc<AtomicBool>,
}

impl Drop for LoudDrop {
    fn drop(&mut self) {
        if self.armed.load(Ordering::SeqCst) {
            panic!("a waker's drop panics");
        }
    }
}

// Only the drop matters here: the waker is never woken.
#[allow(clippy::manual_noop_waker)]
impl Wake for LoudDrop {
    fn wake(self: Arc<Self>) {}
}

/// The same unwound poll, then the reservation is polled again: it still holds
/// its place with the waker the unwound poll installed, so the grant wakes that
/// waker and the next poll completes.
#[test]
fn a_poll_that_unwound_in_the_waker_swap_leaves_the_reservation_to_be_polled_again() {
    let armed = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = channel::<u32>(1, MAILBOX_LANES, None);
    tx.try_send(1).unwrap();
    let mut parked = tx.reserve();
    let first = Waker::from(Arc::new(LoudDrop {
        armed: armed.clone(),
    }));
    assert!(poll_with(&mut parked, &first).is_pending());
    drop(first);
    let (count, second) = counting_waker();
    armed.store(true, Ordering::SeqCst);
    let unwound = catch_unwind(AssertUnwindSafe(|| poll_with(&mut parked, &second)));
    armed.store(false, Ordering::SeqCst);
    assert!(unwound.is_err());
    assert_eq!(waiting(&tx), 1, "the reservation keeps its place");
    assert_eq!(rx.try_recv(), Ok(1));
    assert_eq!(
        wakes(&count),
        1,
        "the grant woke the waker the unwound poll installed"
    );
    assert!(matches!(
        poll_with(&mut parked, &second),
        Poll::Ready(Ok(_))
    ));
    assert!(at_rest(&tx.shared));
}

/// The tasks that wait for the close notice are woken before the lists are
/// closed. A waker among them that panics stays out of the close: the senders
/// parked in the lists are woken with `Closed` all the same.
#[test]
fn a_panicking_wake_of_the_close_notice_does_not_leave_the_parked_senders_asleep() {
    let (tx, mut rx) = channel::<u32>(1, MAILBOX_LANES, None);
    tx.try_send(1).unwrap();
    let loud = LoudWake::new();
    let mut notice = Box::pin(tx.closed());
    assert!(poll_with(&mut notice, &Waker::from(loud.clone())).is_pending());
    let (count, waker) = counting_waker();
    let mut waiter = reserve_on(&tx, 0);
    assert!(poll_with(&mut waiter, &waker).is_pending());
    loud.arm(true);
    let closed = catch_unwind(AssertUnwindSafe(|| rx.close()));
    loud.arm(false);
    assert!(closed.is_ok(), "a waker's panic unwound out of the close");
    assert_eq!(loud.woken(), 1, "the notice's waiter was woken once");
    assert_eq!(wakes(&count), 1, "the parked sender is woken");
    assert!(matches!(poll_once(&mut waiter), Poll::Ready(Err(Closed))));
    assert!(poll_once(&mut notice).is_ready());
}

/// A sender that gives its unit back is not the one whose waiter it wakes: a
/// panic of that waiter's wake stays out of the sender's drop of its permit.
#[test]
fn a_unit_given_back_to_a_waiter_whose_wake_panics_does_not_unwind_into_the_sender() {
    let (tx, _rx) = channel::<u32>(1, MAILBOX_LANES, None);
    let held = permit_on(&tx, 0).unwrap();
    let loud = LoudWake::new();
    let mut waiter = tx.reserve();
    assert!(poll_with(&mut waiter, &Waker::from(loud.clone())).is_pending());
    loud.arm(true);
    let dropped = catch_unwind(AssertUnwindSafe(move || drop(held)));
    loud.arm(false);
    assert!(dropped.is_ok(), "a waker's panic unwound into the sender");
    assert_eq!(loud.woken(), 1);
    assert!(matches!(poll_once(&mut waiter), Poll::Ready(Ok(_))));
    assert!(at_rest(&tx.shared));
}

/// A unit coming back to a closed mailbox wakes the receiver. When the
/// receiver's wake panics, the sender that returned the unit is not unwound and
/// the unit is back in the room.
#[test]
fn a_panicking_wake_of_the_receiver_does_not_unwind_into_the_sender_that_returns_a_unit() {
    let (tx, mut rx) = channel::<u32>(1, MAILBOX_LANES, None);
    let held = permit_on(&tx, 0).unwrap();
    rx.close();
    let loud = LoudWake::new();
    let waker = Waker::from(loud.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(rx.poll_recv(&mut cx).is_pending(), "a unit is still out");
    loud.arm(true);
    let dropped = catch_unwind(AssertUnwindSafe(move || drop(held)));
    loud.arm(false);
    assert!(
        dropped.is_ok(),
        "the receiver's panic unwound into the sender"
    );
    assert_eq!(loud.woken(), 1, "the receiver was woken once");
    assert!(at_rest(&tx.shared), "the unit is back in the room");
    assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(None)));
}

/// The receiver's registered waker is released when the receiver drops. When
/// that waker's drop panics, the panic stays inside the receiver's drop.
#[test]
fn a_receivers_waker_whose_drop_panics_does_not_unwind_out_of_the_receivers_drop() {
    let armed = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = channel::<u32>(2, 1, None);
    let held = permit_on(&tx, 0).unwrap();
    rx.close();
    let waker = Waker::from(Arc::new(LoudDrop {
        armed: armed.clone(),
    }));
    let mut cx = Context::from_waker(&waker);
    assert!(rx.poll_recv(&mut cx).is_pending(), "a unit is still out");
    drop(waker);
    // The push wakes the queue's own copy of the waker and takes it out; the
    // mailbox's copy is the last one left.
    held.send(1);
    armed.store(true, Ordering::SeqCst);
    let dropped = catch_unwind(AssertUnwindSafe(move || drop(rx)));
    armed.store(false, Ordering::SeqCst);
    assert!(
        dropped.is_ok(),
        "the waker's panic unwound out of the receiver's drop"
    );
    assert!(at_rest(&tx.shared));
}

/// A panic payload whose own drop panics.
struct LoudPayload;

impl Drop for LoudPayload {
    fn drop(&mut self) {
        panic!("a panic payload's drop panics");
    }
}

/// A waker that panics with a payload whose drop panics too.
struct LoudPayloadWake;

impl Wake for LoudPayloadWake {
    fn wake(self: Arc<Self>) {
        std::panic::panic_any(LoudPayload);
    }
}

/// The panic of a wake is caught and its payload dropped inside a catch, so a
/// payload whose drop panics too leaves the close and the waiters behind it as
/// they would be without any panic.
#[test]
fn a_wake_whose_panic_payload_panics_when_dropped_is_contained_too() {
    let (tx, mut rx) = channel::<u32>(1, MAILBOX_LANES, None);
    tx.try_send(1).unwrap();
    let mut first = reserve_on(&tx, 0);
    let mut second = reserve_on(&tx, 0);
    let (count, waker) = counting_waker();
    assert!(poll_with(&mut first, &Waker::from(Arc::new(LoudPayloadWake))).is_pending());
    assert!(poll_with(&mut second, &waker).is_pending());
    let closed = catch_unwind(AssertUnwindSafe(|| rx.close()));
    if let Err(payload) = closed {
        // Its drop would panic inside the unwinding of this failure.
        std::mem::forget(payload);
        panic!("the payload's panic unwound out of the close");
    }
    assert_eq!(wakes(&count), 1, "the waiter behind it is woken");
    assert!(matches!(poll_once(&mut first), Poll::Ready(Err(Closed))));
    assert!(matches!(poll_once(&mut second), Poll::Ready(Err(Closed))));
}

thread_local! {
    static AT_THREAD_EXIT: RefCell<Option<AtThreadExit>> = const { RefCell::new(None) };
}

/// What the work at a thread's exit found.
#[derive(Default)]
struct ExitReport {
    /// The destructor ran.
    ran: AtomicBool,
    /// The thread's spare waiters were already gone.
    spares_gone: AtomicBool,
    /// The text of the panic the work raised, if it raised one.
    failure: Mutex<Option<String>>,
}

/// Work that runs in the destructor of a thread-local at the exit of a thread.
/// The destructor is registered before the thread first waits, so it runs after
/// the destructor of the thread's spare waiters.
struct AtThreadExit {
    tx: &'static MailTx<u32>,
    granted: Option<Reserve<'static, u32>>,
    report: Arc<ExitReport>,
}

impl Drop for AtThreadExit {
    fn drop(&mut self) {
        let Some(mut granted) = self.granted.take() else {
            return;
        };
        let tx = self.tx;
        self.report.ran.store(true, Ordering::SeqCst);
        self.report
            .spares_gone
            .store(SPARE_WAITERS.try_with(|_| ()).is_err(), Ordering::SeqCst);
        let work = catch_unwind(AssertUnwindSafe(|| {
            let Poll::Ready(Ok(permit)) = poll_once(&mut granted) else {
                panic!("the reservation granted a unit did not complete");
            };
            // The permit holds the only unit, so a new wait parks.
            let mut next = tx.reserve();
            assert!(poll_once(&mut next).is_pending(), "the new wait parks");
            drop(permit);
            let Poll::Ready(Ok(permit)) = poll_once(&mut next) else {
                panic!("the new wait was not granted the unit");
            };
            drop(permit);
        }));
        if let Err(payload) = work {
            *self.report.failure.lock().unwrap() = Some(payload_into_string(payload));
        }
    }
}

/// A destructor that runs while a thread ends can still wait for a unit and
/// complete a wait it was granted: the thread's spare waiters are gone, so the
/// wait allocates its waiter and lets it go, and no unit is lost.
#[test]
fn a_wait_during_thread_exit_neither_panics_nor_loses_a_unit() {
    let (tx, mut rx) = channel::<u32>(1, MAILBOX_LANES, None);
    let tx: &'static MailTx<u32> = Box::leak(Box::new(tx));
    tx.try_send(1).unwrap();
    let report = Arc::new(ExitReport::default());
    let rx = std::thread::spawn({
        let report = report.clone();
        move || {
            AT_THREAD_EXIT.with(|_| {});
            let mut granted = tx.reserve();
            // The first wait of this thread starts its spare waiters.
            assert!(poll_once(&mut granted).is_pending());
            // The freed unit is granted to the parked reservation.
            assert_eq!(rx.try_recv(), Ok(1));
            AT_THREAD_EXIT.with(|slot| {
                *slot.borrow_mut() = Some(AtThreadExit {
                    tx,
                    granted: Some(granted),
                    report,
                });
            });
            rx
        }
    })
    .join()
    .unwrap();
    assert!(
        report.ran.load(Ordering::SeqCst),
        "the thread's exit did not run the work"
    );
    if cfg!(target_os = "linux") {
        assert!(
            report.spares_gone.load(Ordering::SeqCst),
            "the work ran before the thread's spare waiters were gone"
        );
    }
    assert_eq!(*report.failure.lock().unwrap(), None);
    assert_eq!(tx.capacity(), 1, "a unit was lost at the thread's exit");
    assert!(at_rest(&tx.shared));
    drop(rx);
}

// ---------------------------------------------------------------------------
// The receiver's waker
// ---------------------------------------------------------------------------

/// The receiver registers its waker in the shared state when it polls a closed
/// mailbox; dropping the receiver releases it, so the task it belongs to is not
/// kept alive by the senders that outlive the receiver.
#[test]
fn the_receivers_waker_is_released_when_the_receiver_drops() {
    let (tx, mut rx) = channel::<u32>(2, 1, None);
    rx.close();
    let (count, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);
    // Closed, nothing queued, every unit back: the receiver reads the end.
    assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(None)));
    drop(waker);
    drop(rx);
    assert_eq!(
        Arc::strong_count(&count),
        1,
        "the senders' shared state still holds the receiver's waker"
    );
    assert!(!tx.shared.reader.parked.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------------------
// Ends that race pushes still landing
// ---------------------------------------------------------------------------

/// The messages the stress below made and the messages it dropped, over all of
/// its mailboxes.
static MADE: AtomicUsize = AtomicUsize::new(0);
static DROPPED: AtomicUsize = AtomicUsize::new(0);

/// A message that counts its drop.
struct Counted;

impl Drop for Counted {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

/// The end of an actor's queue closes it, drains it, and hands the receiver to
/// a helper when a push is still landing. Tokio's receive tells the two cases
/// apart: a closed queue with a push in flight answers `Empty`, and answers
/// `Disconnected` only when nothing more can arrive. Many ends in a row, each
/// racing senders on other threads, find every push either taken by the end's
/// drain, dropped by the helper, or refused to its sender: no message stays in
/// an ended queue, and every unit is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ends_racing_pushes_that_are_landing_drop_every_message_and_return_every_unit() {
    const CAPACITY: usize = 64;
    const SENDERS: usize = 6;
    // The loop ends once this many ends have met a push that was landing and
    // at least `MIN_ENDS` ends were made, or at `MAX_ENDS` (the states of the
    // ended mailboxes are kept to read at the end), or after `LONGEST`.
    const WANTED: usize = 5;
    const MIN_ENDS: usize = 20_000;
    const MAX_ENDS: usize = 200_000;
    const LONGEST: Duration = Duration::from_millis(4000);
    MADE.store(0, Ordering::SeqCst);
    DROPPED.store(0, Ordering::SeqCst);
    let system = fresh_system("end-race");
    let id = ActorId::from("end-race".to_string());
    let (tx, mut rx) = channel::<Counted>(CAPACITY, MAILBOX_LANES, Some(id.clone()));
    // The shared state of every mailbox the loop ends, to read its units once
    // the senders are done.
    let mut ended_states = vec![tx.shared.clone()];
    let current = Arc::new(std::sync::RwLock::new(Arc::new(tx)));
    let stop = Arc::new(AtomicBool::new(false));
    let senders: Vec<_> = (0..SENDERS)
        .map(|_| {
            let current = current.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let tx = current.read().unwrap().clone();
                    let mut made = 0;
                    for _ in 0..64 {
                        made += 1;
                        match tx.try_send(Counted) {
                            Ok(()) => {}
                            Err(TrySendError::Full(message)) => {
                                drop(message);
                                std::hint::spin_loop();
                            }
                            Err(TrySendError::Closed(message)) => {
                                drop(message);
                                break;
                            }
                        }
                    }
                    MADE.fetch_add(made, Ordering::SeqCst);
                }
            })
        })
        .collect();
    let started = std::time::Instant::now();
    let mut ends = 0usize;
    loop {
        // The receiver takes messages for a short, varying while, so the queue
        // is nearly empty when it ends.
        for _ in 0..10 + (ends * 7) % 40 {
            if rx.try_recv().is_err() {
                std::hint::spin_loop();
            }
        }
        let (next_tx, next_rx) = channel::<Counted>(CAPACITY, MAILBOX_LANES, Some(id.clone()));
        ended_states.push(next_tx.shared.clone());
        *current.write().unwrap() = Arc::new(next_tx);
        let ended = std::mem::replace(&mut rx, next_rx);
        assert!(runtime::end_queue_late(ended, &system, &id).is_none());
        ends += 1;
        if ends % 256 == 0 {
            let helpers = system.live_helpers() + late_drain_ends(&system);
            if started.elapsed() >= LONGEST
                || ends >= MAX_ENDS
                || (ends >= MIN_ENDS && helpers >= WANTED)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    }
    stop.store(true, Ordering::Relaxed);
    for sender in senders {
        sender.join().unwrap();
    }
    // No push is landing any more.
    assert!(runtime::end_queue_late(rx, &system, &id).is_none());
    until("every helper ends", || system.live_helpers() == 0).await;
    assert_eq!(
        MADE.load(Ordering::SeqCst),
        DROPPED.load(Ordering::SeqCst),
        "a message stayed in an ended queue"
    );
    for (index, state) in ended_states.iter().enumerate() {
        assert_eq!(
            state.room.0.load(Ordering::SeqCst),
            CAPACITY,
            "end {index}: a unit is still out"
        );
        assert_eq!(
            state.waiting_total.load(Ordering::SeqCst),
            0,
            "end {index}: a waiter is left"
        );
    }
    let helpers = late_drain_ends(&system);
    println!("{ends} ends, {helpers} of them met a push that was landing");
    if std::thread::available_parallelism().map_or(1, |cores| cores.get()) >= 2 {
        assert!(
            helpers > 0,
            "none of {ends} ends met a push that was landing"
        );
    }
}

// ---------------------------------------------------------------------------
// The log of a contained panic
// ---------------------------------------------------------------------------

#[cfg(feature = "tracing")]
mod logged {
    use std::fmt::Write as _;

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    use super::{Arc, Mutex};

    /// Collects every WARN event as one line of `name=value` pairs.
    #[derive(Clone, Default)]
    pub(super) struct Warnings {
        pub(super) lines: Arc<Mutex<Vec<String>>>,
    }

    struct Text(String);

    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    impl Subscriber for Warnings {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == Level::WARN
        }

        fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut text = Text(String::new());
            event.record(&mut text);
            self.lines.lock().unwrap().push(text.0);
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }
}

/// A panic of a waker that the mailbox contains leaves one warning naming the
/// actor, whose payload is the panic's text.
#[cfg(feature = "tracing")]
#[test]
fn a_contained_waker_panic_is_logged_with_the_actors_id() {
    let warnings = logged::Warnings::default();
    let lines = warnings.lines.clone();
    tracing::subscriber::with_default(warnings, || {
        let id = ActorId::from("waker-log".to_string());
        let (tx, mut rx) = channel::<u32>(1, MAILBOX_LANES, Some(id));
        tx.try_send(1).unwrap();
        let loud = LoudWake::new();
        let mut waiter = tx.reserve();
        assert!(poll_with(&mut waiter, &Waker::from(loud.clone())).is_pending());
        loud.arm(true);
        rx.close();
        loud.arm(false);
        assert_eq!(loud.woken(), 1);
    });
    let lines = lines.lock().unwrap();
    assert_eq!(lines.len(), 1, "one warning for the one contained panic");
    assert!(
        lines[0].contains("actor_id=waker-log ") && lines[0].contains("a waker's wake panics"),
        "the warning names the actor and the panic: {}",
        lines[0]
    );
}
