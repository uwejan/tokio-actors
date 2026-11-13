//! Tests of the lane mailbox: the exact bound, the order each task keeps, the
//! grants of freed units, cancellation at every await, the close and the
//! drain, the late-slot end of a queue with a unit still out, wakers that
//! panic, and ends that race pushes still landing.
//!
//! No step is ordered by a sleep: a wait for a state polls it, and a timeout
//! only detects a hang.

use std::cell::RefCell;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::{timeout, timeout_at, Instant};

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

/// The most bytes the shared state of an idle mailbox takes.
const IDLE_BUDGET: usize = 448;

// ---------------------------------------------------------------------------
// Order and totals under many senders
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_task_keeps_its_order_under_many_senders() {
    const TASKS: usize = 24;
    const EACH: usize = 3000;
    let (tx, mut rx) = channel::<(usize, usize)>(8, MAILBOX_LANES, None, None);
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

#[test]
fn the_receiver_takes_messages_in_push_order() {
    let (tx, mut rx) = channel::<(usize, usize)>(16, MAILBOX_LANES, None, None);
    let mut pushed = Vec::new();
    for seq in 0..3 {
        for lane in 0..4 {
            permit_on(&tx, lane).unwrap().send((lane, seq));
            pushed.push((lane, seq));
        }
    }
    let taken: Vec<(usize, usize)> = (0..12).map(|_| rx.try_recv().unwrap()).collect();
    assert_eq!(taken, pushed);
}

// ---------------------------------------------------------------------------
// Grants, cancellation and the waiters' queues
// ---------------------------------------------------------------------------

/// Cancels a reservation at a chosen point.
async fn cancelled_reserve(tx: &MailTx<u32>, how: usize, wait: u64) {
    match how {
        0 => {
            let deadline = Instant::now() + Duration::from_micros(wait * 400);
            if let Ok(Ok(permit)) = timeout_at(deadline, tx.reserve()).await {
                if wait % 2 == 0 {
                    permit.send(0);
                }
            }
        }
        1 => {
            let mut reserve = tx.reserve();
            std::future::poll_fn(|cx| {
                let _ = Pin::new(&mut reserve).poll(cx);
                Poll::Ready(())
            })
            .await;
        }
        _ => {
            tokio::select! {
                biased;
                result = tx.reserve() => {
                    if let Ok(permit) = result {
                        permit.send(1);
                    }
                }
                () = async {
                    for _ in 0..wait % 5 {
                        tokio::task::yield_now().await;
                    }
                } => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Close and drain
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_racing_pushes_into_unused_lanes_loses_nothing() {
    for round in 0..3000 {
        let (tx, mut rx) = channel::<usize>(MAILBOX_LANES, MAILBOX_LANES, None, None);
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

/// Whether the side of a reply is closed now, without waiting.
fn reply_is_closed(reply: &mut oneshot::Receiver<()>) -> bool {
    matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Closed))
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
    let (tx, rx) = channel::<Bomb>(1, MAILBOX_LANES, None, None);
    let permit = tx.reserve().await.unwrap();
    assert!(
        runtime::end_queue_late(rx, &system, &runtime::Owner::new(id.clone(), false), None)
            .is_none()
    );
    // The push returns: the panic of the drop stays inside it.
    permit.send(Bomb);
    assert!(at_rest(&tx.shared), "the unit is back in the room");
    assert_eq!(system.live_helpers(), 0);
    assert_eq!(late_drain_ends(&system), 0);
}

// ---------------------------------------------------------------------------
// Grant order, the race windows of a release and the receiver's end
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// The receiver's waker
// ---------------------------------------------------------------------------

/// A queue that stays open after the mailbox closes, so a push can still arrive
/// at the late drain.
struct OpenQueue<T>(MailRx<T>);

impl<T: Send + 'static> QueueRx for OpenQueue<T> {
    type Item = T;

    fn close(&mut self) {
        MailRx::close(&mut self.0);
    }

    fn try_recv_one(&mut self) -> Result<T, bool> {
        match self.0.queue.try_recv() {
            Ok(message) => {
                self.0.shared.release_unit(false);
                Ok(message)
            }
            Err(_) => Err(true),
        }
    }

    fn poll_recv_one(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.poll_recv(cx)
    }

    fn free_units(&self) -> usize {
        self.0.capacity()
    }

    fn all_units(&self) -> usize {
        self.0.max_capacity()
    }

    fn queued(&self) -> usize {
        self.0.len()
    }
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
    let (tx, mut rx) = channel::<Counted>(CAPACITY, MAILBOX_LANES, Some(id.clone()), None);
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
        let (next_tx, next_rx) =
            channel::<Counted>(CAPACITY, MAILBOX_LANES, Some(id.clone()), None);
        ended_states.push(next_tx.shared.clone());
        *current.write().unwrap() = Arc::new(next_tx);
        let ended = std::mem::replace(&mut rx, next_rx);
        assert!(runtime::end_queue_late(
            ended,
            &system,
            &runtime::Owner::new(id.clone(), false),
            None
        )
        .is_none());
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
    assert!(
        runtime::end_queue_late(rx, &system, &runtime::Owner::new(id.clone(), false), None)
            .is_none()
    );
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
