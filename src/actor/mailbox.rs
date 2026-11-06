//! The actor's normal mailbox: a bounded, many-sender FIFO queue.
//!
//! # Structure
//!
//! Every message travels one unbounded FIFO queue (tokio's unbounded
//! channel, whose push takes no lock), so the receiver takes messages in the
//! order their pushes completed: a message whose send returned before another
//! send began is taken first, whichever tasks sent the two. The bound is one
//! exact count of free units (`room`, starting at the capacity): a send takes
//! a unit before its message exists (`reserve`), and the receiver gives the
//! unit back when it takes the message, so the queue never holds more
//! messages than the capacity.
//!
//! A sender that finds no free unit waits in one of up to [`MAILBOX_LANES`]
//! wait lists, picked from its task's id (from a per-thread key outside a
//! task), so senders that wait at the same time spread their bookkeeping
//! over several locks instead of meeting on one. Lists 1 and up are
//! allocated together on the first wait that needs one; a runtime with one
//! worker thread gets one list.
//!
//! # The free unit, the waiters and no barging
//!
//! A unit freed while senders wait goes to a waiter, never to the shared room:
//! a waiter is popped and marked granted under its list's lock, so a grant and
//! a cancellation never overlap (a cancellation that takes the lock after the
//! pop finds the grant and gives the unit back), and it is woken once the lock
//! is released. Lists are served round-robin and each list in arrival order.
//! The room receives only a unit no waiter takes.
//!
//! The release and the wait meet in two steps that each read the other's
//! write, all on `SeqCst` atomics:
//!
//! - Release: if a waiter is counted (`waiting_total`), grant it the unit.
//!   Otherwise add the unit to the room, then read `waiting_total` again and,
//!   if a waiter is counted now, take a unit back from the room and grant it
//!   (`rebalance`).
//! - Wait: take the list's lock, push the entry, count it in `waiting_total`,
//!   unlock, then read the room again and, if it is above zero, run the same
//!   `rebalance`.
//!
//! In the total order of those four accesses either the release's second read
//! counts the new waiter or the waiter's second read sees the new unit, so no
//! unit sleeps in the room beside a sleeping waiter. A sender that takes a
//! unit from the room by compare-exchange can win only a unit that is in the
//! room in the window between the release's add and its take-back, and a
//! waiter that loses it keeps its place in its list.
//!
//! A reservation dropped after it was granted gives the unit back through the
//! same release, which looks at every list; a reservation dropped before a
//! grant removes its entry under the list's lock and leaves no trace.
//!
//! # Close and drain
//!
//! `close` (the receiver's, and its drop) sets the closed flag, then a full
//! fence, then wakes every waiter with `Closed`. It takes the lock of every
//! existing wait list, whatever the list's count reads, because a sender that
//! is parking reads the flag under that lock before it counts itself. It does
//! not stop a push: a permit taken before the close sends after it, and the
//! queue wakes the receiver. The actor's end then closes the queue itself:
//! from there a push fails and its sender drops the message in that push, and
//! only a push already past the queue's own check still lands, for the
//! late-slot helper that took the receiver over. The receiver reports the
//! end (`None`) when every sender is gone and the queue is empty, when the
//! mailbox is closed and every unit is back in the room, or, once the end
//! closed the queue itself, when no push is still landing; each unit return
//! wakes the receiver while closed.
//!
//! A request pushes, runs a full fence and reads the closed flag
//! (`queue_closed_after_push` in the handle). The end sets the flag, runs a
//! full fence, then drains the queue until it finds it empty. In the order of
//! the two fences either the push comes before the end's fence and the drain
//! takes the message, or the close comes before the sender's fence and the
//! sender sees the flag.
//!
//! # Rules for the code
//!
//! No lock here is held while a message is dropped or while a waker of
//! another task is woken or dropped: a message is never dropped under a lock,
//! and such a waker is taken out of its slot and woken (or dropped) after the
//! lock is released. The one call of waker code that does run under a lock is
//! the clone of the polling task's own waker, which a poll makes to store it
//! in its list entry or in the receiver's slot, under that list's or that
//! slot's lock. A clone that panics unwinds out of that poll before the entry
//! or the slot has changed (a first wait has listed nothing yet), and the
//! unwind releases the lock. The locks do not poison: a poisoned lock is used
//! as it is, because nothing under it can leave the data half written.
//!
//! A waker belongs to the task that polls with it. A wake or a drop that the
//! mailbox makes on behalf of another task (a grant, the close and its notice,
//! the wake-up of the receiver for a returned unit, the release of the
//! receiver's waker) is contained where it is made: a panic is caught, its
//! payload is dropped inside a catch, it is logged with the `tracing` feature,
//! and the work goes on. A unit stays counted, every other waiter is still
//! woken, and no message is lost; the panic never reaches the receiver's
//! `poll_recv`, the closer or the sender that returned a unit. (The queue's
//! own wake of the receiver, made inside a push, is not the mailbox's.) A
//! grant can reach the polling task itself: a sender that parks and finds its
//! own entry granted by the rebalance it runs next wakes its own waker, and
//! that wake is contained like any other, so the panic is swallowed there and
//! the poll completes with the unit. A waker that a poll replaces is the
//! polling task's own: a panic of its clone or its drop unwinds into that
//! poll. The reservation stays parked and listed through it, so dropping the
//! reservation cancels the entry, or gives back the unit it was granted. The
//! close notice is tokio's `Notify`, which owns the batch it wakes: a waker of
//! the notice that panics ends tokio's wake of that batch, and the notice
//! waiters it had not yet woken stay asleep (their futures are complete and
//! finish at their next poll). Only the crate's own forwarder tasks wait for
//! the notice, on tokio's wakers, so no waker of a user can reach it. A thread
//! that is ending has no spare waiters to reuse: a wait there allocates its
//! waiter and lets it go.

use std::collections::VecDeque;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{
    fence, AtomicBool, AtomicU64, AtomicU8, AtomicUsize,
    Ordering::{Relaxed, SeqCst},
};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::task::{Context, Poll, Waker};

use tokio::sync::mpsc;

use crate::types::ActorId;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::Notify;

use crate::actor::panic::{catch_sync, contain_waker_panic, discard};

/// The most wait lists a mailbox has.
pub(crate) const MAILBOX_LANES: usize = 8;

/// The number of wait lists for an actor that runs on a runtime with
/// `workers` worker threads: one when the runtime has a single worker (a
/// current-thread runtime among them), where several only add cost, else
/// [`MAILBOX_LANES`].
pub(crate) fn lanes_for(workers: usize) -> usize {
    if workers <= 1 {
        1
    } else {
        MAILBOX_LANES
    }
}

/// Creates a mailbox with `capacity` units (at least 1) and `lanes` wait
/// lists (clamped to 1 through [`MAILBOX_LANES`]) for the actor `owner`.
pub(crate) fn channel<T>(
    capacity: usize,
    lanes: usize,
    owner: Option<ActorId>,
) -> (MailTx<T>, MailRx<T>) {
    assert!(capacity > 0, "a mailbox needs a capacity of at least 1");
    let shared = Arc::new(Shared::new(capacity, lanes.clamp(1, MAILBOX_LANES), owner));
    let (queue_tx, queue_rx) = mpsc::unbounded_channel();
    (
        MailTx {
            shared: shared.clone(),
            queue: queue_tx,
        },
        MailRx {
            shared,
            queue: queue_rx,
        },
    )
}

/// A mailbox with every wait list, for tests that build a handle by hand.
#[cfg(test)]
pub(crate) fn test_channel<T>(capacity: usize) -> (MailTx<T>, MailRx<T>) {
    channel(capacity, MAILBOX_LANES, None)
}

/// Takes a lock, using the data of a poisoned one as it is.
fn lock<V>(mutex: &Mutex<V>) -> MutexGuard<'_, V> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Keeps a hot atomic on its own cache line.
#[repr(align(64))]
struct Padded<V>(V);

const WAITING: u8 = 0;
const GRANTED: u8 = 1;
const CLOSED: u8 = 2;

/// A sender waiting for a unit. Its state changes only under its list's
/// lock, and only from `WAITING`; its waker lives in the list entry.
struct Waiter {
    state: AtomicU8,
}

thread_local! {
    /// Waiters whose wait is over, kept for the next wait on this thread, so
    /// a wait allocates nothing once the thread has one.
    static SPARE_WAITERS: std::cell::RefCell<Vec<Arc<Waiter>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A waiter for a new wait: a spare one from this thread when there is one. A
/// thread whose spares are already gone (it is ending, and a destructor of
/// its own waits) gets a new one.
fn fresh_waiter() -> Arc<Waiter> {
    match SPARE_WAITERS.try_with(|spare| spare.borrow_mut().pop()) {
        Ok(Some(waiter)) => {
            waiter.state.store(WAITING, Relaxed);
            waiter
        }
        _ => Arc::new(Waiter {
            state: AtomicU8::new(WAITING),
        }),
    }
}

/// Keeps a waiter whose wait is over for reuse, when nothing else holds it. A
/// thread whose spares are already gone lets the waiter go.
fn recycle_waiter(waiter: Arc<Waiter>) {
    if Arc::strong_count(&waiter) == 1 {
        let _ = SPARE_WAITERS.try_with(|spare| {
            let mut spare = spare.borrow_mut();
            if spare.len() < 64 {
                spare.push(waiter);
            }
        });
    }
}

/// A wait list: the senders waiting for a unit, in arrival order.
struct Lane {
    waiters: Mutex<VecDeque<(Arc<Waiter>, Waker)>>,
    /// The waiters' count, written under their lock.
    waiting: AtomicUsize,
}

impl Lane {
    fn new() -> Self {
        Self {
            waiters: Mutex::new(VecDeque::new()),
            waiting: AtomicUsize::new(0),
        }
    }
}

/// The receiver's wake-up for units coming back to a closed mailbox: set
/// before the receiver looks once more and goes to sleep, cleared by the
/// first release that wakes it and by the receiver's drop.
struct Reader {
    parked: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

struct Shared<T> {
    capacity: usize,
    lane_count: usize,
    /// Free units. Exact: a unit is taken before a message exists and comes
    /// back when the message is taken or the reservation is dropped.
    room: Padded<AtomicUsize>,
    /// Senders waiting in any list.
    waiting_total: AtomicUsize,
    closed: AtomicBool,
    /// Live senders.
    tx_count: AtomicUsize,
    /// Where the next grant starts its round of the lists.
    next_grant: AtomicUsize,
    reader: Reader,
    closed_notify: Notify,
    /// The actor the mailbox belongs to, named in the log of a panic raised
    /// by the drop of a message that arrived after the end.
    owner: Option<ActorId>,
    /// Runs once inside the next park, after it read the closed flag as open
    /// and before it queued itself.
    #[cfg(test)]
    park_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    lane0: Padded<Lane>,
    /// Lists 1 and up, allocated together on first use.
    more: OnceLock<Box<[Padded<Lane>]>>,
    /// The messages travel the queue; the shared state only counts them.
    _messages: PhantomData<fn() -> T>,
}

impl<T> Shared<T> {
    fn new(capacity: usize, lane_count: usize, owner: Option<ActorId>) -> Self {
        Self {
            capacity,
            lane_count,
            room: Padded(AtomicUsize::new(capacity)),
            waiting_total: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            tx_count: AtomicUsize::new(1),
            next_grant: AtomicUsize::new(0),
            reader: Reader {
                parked: AtomicBool::new(false),
                waker: Mutex::new(None),
            },
            closed_notify: Notify::new(),
            owner,
            #[cfg(test)]
            park_hook: Mutex::new(None),
            lane0: Padded(Lane::new()),
            more: OnceLock::new(),
            _messages: PhantomData,
        }
    }

    /// The list `index` if it exists now.
    fn try_lane(&self, index: usize) -> Option<&Lane> {
        if index == 0 {
            Some(&self.lane0.0)
        } else {
            self.more
                .get()
                .and_then(|lanes| lanes.get(index - 1))
                .map(|lane| &lane.0)
        }
    }

    /// The list `index`, allocating lists 1 and up if they do not exist yet.
    fn lane_or_init(&self, index: usize) -> &Lane {
        if index == 0 {
            return &self.lane0.0;
        }
        let lanes = self.more.get_or_init(|| {
            (1..self.lane_count)
                .map(|_| Padded(Lane::new()))
                .collect::<Vec<_>>()
                .into_boxed_slice()
        });
        &lanes[index - 1].0
    }

    /// The list the calling task (or thread) waits in.
    fn wait_lane(&self) -> usize {
        if self.lane_count == 1 {
            0
        } else {
            lane_of(sender_key(), self.lane_count)
        }
    }

    /// Takes one unit from the room if it holds one.
    fn take_unit(&self) -> bool {
        let mut current = self.room.0.load(SeqCst);
        while current > 0 {
            match self
                .room
                .0
                .compare_exchange_weak(current, current - 1, SeqCst, SeqCst)
            {
                Ok(_) => return true,
                Err(seen) => current = seen,
            }
        }
        false
    }

    /// Gives a unit that came back (a message taken, a permit or a granted
    /// reservation dropped) to a waiter, else to the room. `wake` is set for
    /// every caller but the receiver: a closed mailbox is over when its units
    /// are all back, and the receiver may be waiting for that.
    fn release_unit(&self, wake: bool) {
        if self.waiting_total.load(SeqCst) > 0 && self.grant_one() {
            return;
        }
        self.room.0.fetch_add(1, SeqCst);
        if self.waiting_total.load(SeqCst) > 0 {
            self.rebalance();
        }
        if wake && self.closed.load(SeqCst) {
            self.wake_reader();
        }
    }

    /// Moves units from the room to the waiters while both exist.
    fn rebalance(&self) {
        #[cfg(test)]
        tests::at_rebalance();
        while self.waiting_total.load(SeqCst) > 0 && self.take_unit() {
            if !self.grant_one() {
                self.room.0.fetch_add(1, SeqCst);
                if self.closed.load(SeqCst) {
                    self.wake_reader();
                }
            }
        }
    }

    /// Grants a unit to the first waiter of the next list that has one.
    fn grant_one(&self) -> bool {
        let start = self.next_grant.fetch_add(1, Relaxed);
        for step in 0..self.lane_count {
            let Some(lane) = self.try_lane((start + step) % self.lane_count) else {
                continue;
            };
            #[cfg(test)]
            tests::in_grant_scan();
            if lane.waiting.load(SeqCst) == 0 {
                continue;
            }
            let entry = {
                let mut waiters = lock(&lane.waiters);
                let entry = waiters.pop_front();
                if let Some((waiter, _)) = &entry {
                    waiter.state.store(GRANTED, SeqCst);
                    lane.waiting.fetch_sub(1, SeqCst);
                    self.waiting_total.fetch_sub(1, SeqCst);
                }
                entry
            };
            if let Some((waiter, waker)) = entry {
                // Let go first, so the woken sender can reuse its waiter.
                drop(waiter);
                // The unit is the waiter's from the pop on, whether or not its
                // task takes the wake.
                self.wake_contained(waker);
                return true;
            }
        }
        false
    }

    /// Queues the calling task as a waiter in list `lane_index`.
    fn park(&self, lane_index: usize, cx: &Context<'_>) -> Parked {
        let lane = self.lane_or_init(lane_index);
        // Pairs with the fence in `close`: the list exists for the closer, or
        // the closed flag is visible here.
        fence(SeqCst);
        let waiter = fresh_waiter();
        {
            let mut waiters = lock(&lane.waiters);
            if self.closed.load(SeqCst) {
                return Parked::Closed;
            }
            #[cfg(test)]
            {
                let hook = lock(&self.park_hook).take();
                if let Some(hook) = hook {
                    hook();
                }
            }
            waiters.push_back((waiter.clone(), cx.waker().clone()));
            lane.waiting.fetch_add(1, SeqCst);
            self.waiting_total.fetch_add(1, SeqCst);
        }
        if self.room.0.load(SeqCst) > 0 {
            self.rebalance();
        }
        Parked::Waiting(waiter)
    }

    /// Takes a waiter out of its list, or gives back the unit it was granted.
    fn cancel(&self, lane_index: usize, waiter: &Arc<Waiter>) {
        let Some(lane) = self.try_lane(lane_index) else {
            return;
        };
        let (granted, removed) = {
            let mut waiters = lock(&lane.waiters);
            match waiter.state.load(SeqCst) {
                WAITING => {
                    let removed = waiters
                        .iter()
                        .position(|(queued, _)| Arc::ptr_eq(queued, waiter))
                        .and_then(|at| waiters.remove(at));
                    if removed.is_some() {
                        lane.waiting.fetch_sub(1, SeqCst);
                        self.waiting_total.fetch_sub(1, SeqCst);
                    }
                    (false, removed)
                }
                GRANTED => (true, None),
                _ => (false, None),
            }
        };
        drop(removed);
        if granted {
            self.release_unit(true);
        }
    }

    /// Wakes the receiver if it is parked.
    fn wake_reader(&self) {
        if self.reader.parked.load(SeqCst) && self.reader.parked.swap(false, SeqCst) {
            let waker = lock(&self.reader.waker).take();
            if let Some(waker) = waker {
                self.wake_contained(waker);
            }
        }
    }

    /// Lets go of the receiver's waker: nothing waits for the receiver's
    /// wake-up from here on, and the task it belongs to is not kept alive by
    /// the senders. A panic of the waker's drop stays here.
    fn clear_reader(&self) {
        self.reader.parked.store(false, SeqCst);
        let waker = lock(&self.reader.waker).take();
        if let Some(waker) = waker {
            self.contain_waker_panic(move || drop(waker));
        }
    }

    /// Wakes a waker that belongs to another task. A panic of the wake is
    /// contained (see [`contain_waker_panic`](Self::contain_waker_panic)), so
    /// the caller goes on with the rest of its work.
    fn wake_contained(&self, waker: Waker) {
        self.contain_waker_panic(move || waker.wake());
    }

    /// Runs the wake or the drop of a waker that belongs to another task. A
    /// panic is caught here and its payload dropped inside a catch (a
    /// payload's drop can panic too); with the `tracing` feature it is logged
    /// with the actor's id. It never unwinds into the caller.
    fn contain_waker_panic(&self, work: impl FnOnce()) {
        contain_waker_panic("mailbox", self.owner.as_ref(), work);
    }

    /// Closes the mailbox: the flag, a full fence, then every waiter woken
    /// with `Closed`. Pushes through units taken earlier still go through,
    /// until the actor's end closes the queue itself.
    fn close(&self) {
        self.closed.store(true, SeqCst);
        fence(SeqCst);
        // A waker of the close notice that panics stays here: the lists below
        // are closed and their waiters woken all the same. The notice is
        // tokio's `Notify`, which stops waking its batch at that waker.
        self.contain_waker_panic(|| self.closed_notify.notify_waiters());
        for index in 0..self.lane_count {
            let Some(lane) = self.try_lane(index) else {
                continue;
            };
            // The list's lock is taken whatever its count reads: a sender
            // that is parking has read the closed flag as open under this
            // lock but has not counted itself yet, so a count read without
            // the lock could miss it. With the lock, the closer either follows
            // the sender's unlock and finds its entry, or precedes the
            // sender's lock and the sender reads the flag as closed.
            let drained: Vec<(Arc<Waiter>, Waker)> = {
                let mut waiters = lock(&lane.waiters);
                let drained: Vec<_> = waiters.drain(..).collect();
                for (waiter, _) in &drained {
                    waiter.state.store(CLOSED, SeqCst);
                }
                lane.waiting.fetch_sub(drained.len(), SeqCst);
                self.waiting_total.fetch_sub(drained.len(), SeqCst);
                drained
            };
            // Every waiter taken out of the list is woken, whatever an earlier
            // wake did.
            for (_, waker) in drained {
                self.wake_contained(waker);
            }
        }
    }

    /// Whether the receiver has seen the end: closed (or without a sender)
    /// and every unit back, so nothing is queued.
    fn ended(&self) -> bool {
        (self.closed.load(SeqCst) || self.tx_count.load(SeqCst) == 0)
            && self.room.0.load(SeqCst) == self.capacity
    }
}

enum Parked {
    Closed,
    Waiting(Arc<Waiter>),
}

/// The key that picks the sending task's wait list: the task's id, else a
/// number given to the thread once.
fn sender_key() -> u64 {
    if let Some(id) = tokio::task::try_id() {
        let mut hasher = KeyHasher(0);
        id.hash(&mut hasher);
        return hasher.0.max(1);
    }
    thread_local! {
        static THREAD_KEY: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    static NEXT_THREAD: AtomicU64 = AtomicU64::new(1);
    THREAD_KEY.with(|key| {
        if key.get() == 0 {
            key.set(NEXT_THREAD.fetch_add(1, Relaxed));
        }
        key.get()
    })
}

/// Folds what a task id writes into one number.
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(*byte);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 ^= value;
    }
}

/// Spreads sequential keys over `lanes` lists.
fn lane_of(key: u64, lanes: usize) -> usize {
    ((key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize) % lanes
}

/// The cooperative budget, charged once for a call that completes: yields to
/// the runtime when the task has used its share, as tokio's own channel does.
fn poll_budget(cx: &mut Context<'_>) -> Poll<()> {
    let mut consume = std::pin::pin!(tokio::task::coop::consume_budget());
    consume.as_mut().poll(cx)
}

/// The reason a reservation failed: the mailbox is closed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Closed;

/// The sending half of a mailbox.
pub(crate) struct MailTx<T> {
    shared: Arc<Shared<T>>,
    queue: mpsc::UnboundedSender<T>,
}

impl<T> Clone for MailTx<T> {
    fn clone(&self) -> Self {
        self.shared.tx_count.fetch_add(1, Relaxed);
        Self {
            shared: self.shared.clone(),
            queue: self.queue.clone(),
        }
    }
}

impl<T> Drop for MailTx<T> {
    fn drop(&mut self) {
        if self.shared.tx_count.fetch_sub(1, SeqCst) == 1 {
            self.shared.wake_reader();
        }
    }
}

impl<T> MailTx<T> {
    /// Takes one unit, waiting while none is free; `Closed` once the mailbox
    /// is closed. Cancel safe at every await: a dropped future holds no unit,
    /// or gives back the one it was granted.
    pub(crate) fn reserve(&self) -> Reserve<'_, T> {
        Reserve {
            tx: self,
            step: Step::Fresh,
            #[cfg(test)]
            lane_override: None,
        }
    }

    /// Sends without waiting: `Full` when no unit is free now, `Closed` when
    /// the mailbox is closed. The message comes back in either case.
    pub(crate) fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        let shared = &*self.shared;
        if shared.closed.load(SeqCst) {
            return Err(TrySendError::Closed(message));
        }
        if !shared.take_unit() {
            return Err(TrySendError::Full(message));
        }
        self.push(message);
        Ok(())
    }

    /// Pushes `message`, which the caller holds a unit for; the queue wakes
    /// the receiver. Once the actor's end closed the queue nothing takes it:
    /// the unit goes back and the message is dropped here, outside every lock,
    /// inside a catch; with the `tracing` feature a panic of that drop is
    /// logged with the actor's id.
    fn push(&self, message: T) {
        if let Err(mpsc::error::SendError(message)) = self.queue.send(message) {
            self.shared.release_unit(true);
            if let Some(payload) = crate::actor::runtime::end_value(message) {
                if let Some(owner) = &self.shared.owner {
                    crate::actor::runtime::log_late_drop(&payload, owner);
                }
            }
        }
    }

    /// [`reserve`](Self::reserve) for a sender the permit then owns, so the
    /// permit can outlive the borrow of any handle.
    #[cfg(test)]
    pub(crate) async fn reserve_owned(self) -> Result<OwnedPermit<T>, Closed> {
        self.reserve().await?.keep();
        Ok(OwnedPermit { tx: Some(self) })
    }

    /// The live senders.
    #[cfg(test)]
    pub(crate) fn strong_count(&self) -> usize {
        self.shared.tx_count.load(SeqCst)
    }

    /// The wait lists this mailbox was made with.
    #[cfg(test)]
    pub(crate) fn lane_count(&self) -> usize {
        self.shared.lane_count
    }

    /// Whether the mailbox is closed, by its receiver's close or drop.
    pub(crate) fn is_closed(&self) -> bool {
        self.shared.closed.load(SeqCst)
    }

    /// The free units.
    pub(crate) fn capacity(&self) -> usize {
        self.shared.room.0.load(SeqCst)
    }

    /// Completes once the mailbox is closed.
    pub(crate) async fn closed(&self) {
        let notified = self.shared.closed_notify.notified();
        tokio::pin!(notified);
        // Registered before the flag is read, so a close in between wakes it.
        notified.as_mut().enable();
        if self.shared.closed.load(SeqCst) {
            return;
        }
        notified.await;
    }
}

enum Step {
    Fresh,
    Parked { lane: usize, waiter: Arc<Waiter> },
    Done,
}

/// A wait for one unit; see [`MailTx::reserve`].
pub(crate) struct Reserve<'a, T> {
    tx: &'a MailTx<T>,
    step: Step,
    /// The list a test places its waiter in.
    #[cfg(test)]
    lane_override: Option<usize>,
}

impl<'a, T> Future for Reserve<'a, T> {
    type Output = Result<Permit<'a, T>, Closed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let tx: &'a MailTx<T> = this.tx;
        let shared: &'a Shared<T> = &tx.shared;
        loop {
            // The step changes only where the poll completes or parks, never
            // while it holds a place in a list or a granted unit: a panic of a
            // waker (its clone or its drop) unwinds out of this poll with the
            // step still the one the reservation's drop acts on.
            match &this.step {
                Step::Fresh => {
                    if poll_budget(cx).is_pending() {
                        return Poll::Pending;
                    }
                    if shared.closed.load(SeqCst) {
                        this.step = Step::Done;
                        return Poll::Ready(Err(Closed));
                    }
                    if shared.take_unit() {
                        this.step = Step::Done;
                        return Poll::Ready(Ok(Permit::new(tx)));
                    }
                    #[cfg(test)]
                    let lane = this.lane_override.unwrap_or_else(|| shared.wait_lane());
                    #[cfg(not(test))]
                    let lane = shared.wait_lane();
                    match shared.park(lane, cx) {
                        Parked::Closed => {
                            this.step = Step::Done;
                            return Poll::Ready(Err(Closed));
                        }
                        Parked::Waiting(waiter) => this.step = Step::Parked { lane, waiter },
                    }
                }
                Step::Parked { lane, waiter } => match waiter.state.load(SeqCst) {
                    GRANTED => {
                        // The unit is this reservation's. A permit holds it
                        // before anything else runs, so no step below can lose it.
                        let permit = Permit::new(tx);
                        if let Step::Parked { waiter, .. } =
                            std::mem::replace(&mut this.step, Step::Done)
                        {
                            recycle_waiter(waiter);
                        }
                        if shared.closed.load(SeqCst) {
                            drop(permit);
                            return Poll::Ready(Err(Closed));
                        }
                        return Poll::Ready(Ok(permit));
                    }
                    CLOSED => {
                        this.step = Step::Done;
                        return Poll::Ready(Err(Closed));
                    }
                    _ => {
                        // Polled while still waiting: refresh the entry's
                        // waker under the list's lock, where a grant takes it.
                        let old = shared.try_lane(*lane).and_then(|list| {
                            let mut waiters = lock(&list.waiters);
                            let entry = waiters
                                .iter_mut()
                                .find(|(queued, _)| Arc::ptr_eq(queued, waiter))?;
                            if entry.1.will_wake(cx.waker()) {
                                None
                            } else {
                                Some(std::mem::replace(&mut entry.1, cx.waker().clone()))
                            }
                        });
                        // A grant between the first read and the new waker
                        // took the old one: look again.
                        let settled = waiter.state.load(SeqCst) != WAITING;
                        // The old waker is the polling task's own; its drop
                        // runs with the reservation parked and listed.
                        drop(old);
                        if settled {
                            continue;
                        }
                        return Poll::Pending;
                    }
                },
                Step::Done => panic!("`Reserve` polled after completion"),
            }
        }
    }
}

impl<T> Drop for Reserve<'_, T> {
    fn drop(&mut self) {
        if let Step::Parked { lane, waiter } = &self.step {
            self.tx.shared.cancel(*lane, waiter);
        }
    }
}

/// One unit taken from the mailbox: sending through it cannot wait or fail,
/// and dropping it unused gives the unit back.
pub(crate) struct Permit<'a, T> {
    tx: &'a MailTx<T>,
    live: bool,
}

impl<'a, T> Permit<'a, T> {
    fn new(tx: &'a MailTx<T>) -> Self {
        Self { tx, live: true }
    }

    /// Keeps the unit out of the mailbox's room without sending: the caller
    /// takes over the unit.
    #[cfg(test)]
    fn keep(mut self) {
        self.live = false;
    }

    /// Puts `message` in the queue. It goes in after the mailbox closed too,
    /// for the receiver; once the actor's end closed the queue itself, the
    /// push fails and the message is dropped here.
    pub(crate) fn send(mut self, message: T) {
        self.live = false;
        self.tx.push(message);
    }
}

impl<T> Drop for Permit<'_, T> {
    fn drop(&mut self) {
        if self.live {
            self.tx.shared.release_unit(true);
        }
    }
}

/// One unit taken together with a sender of its own; see
/// [`MailTx::reserve_owned`].
#[cfg(test)]
pub(crate) struct OwnedPermit<T> {
    tx: Option<MailTx<T>>,
}

#[cfg(test)]
impl<T> OwnedPermit<T> {
    /// Puts `message` in the queue and gives the sender back.
    pub(crate) fn send(mut self, message: T) -> MailTx<T> {
        let tx = self.tx.take().expect("the sender is held until the send");
        tx.push(message);
        tx
    }
}

#[cfg(test)]
impl<T> Drop for OwnedPermit<T> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            tx.shared.release_unit(true);
        }
    }
}

/// The receiving half of a mailbox. Dropping it closes the mailbox.
pub(crate) struct MailRx<T> {
    shared: Arc<Shared<T>>,
    queue: mpsc::UnboundedReceiver<T>,
}

impl<T> MailRx<T> {
    /// The next message, waiting for one; `None` once the mailbox has ended.
    /// Cancel safe.
    pub(crate) async fn recv(&mut self) -> Option<T> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// One poll of [`recv`](Self::recv). The queue charges the cooperative
    /// budget when it returns a message.
    pub(crate) fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        match self.queue.poll_recv(cx) {
            Poll::Ready(Some(message)) => {
                self.shared.release_unit(false);
                Poll::Ready(Some(message))
            }
            // Every sender is gone and nothing is queued.
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                // While senders live only a close ends the mailbox, and only
                // the receiver closes it: it waits for the units to come back
                // only then.
                if self.shared.closed.load(SeqCst) {
                    self.register(cx);
                    if self.shared.ended() {
                        return Poll::Ready(None);
                    }
                }
                Poll::Pending
            }
        }
    }

    /// Records the waker and raises the parked flag; the caller looks once
    /// more after this.
    fn register(&self, cx: &Context<'_>) {
        let old = {
            let mut slot = lock(&self.shared.reader.waker);
            match &*slot {
                Some(waker) if waker.will_wake(cx.waker()) => None,
                _ => slot.replace(cx.waker().clone()),
            }
        };
        drop(old);
        self.shared.reader.parked.store(true, SeqCst);
    }

    /// The next message if one is queued now; never waits. `Disconnected`
    /// once the mailbox has ended.
    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> Result<T, mpsc::error::TryRecvError> {
        match self.take() {
            Some(message) => Ok(message),
            None if self.shared.ended() => Err(mpsc::error::TryRecvError::Disconnected),
            None => Err(mpsc::error::TryRecvError::Empty),
        }
    }

    /// Takes the next message if one is queued now, and gives its unit back.
    fn take(&mut self) -> Option<T> {
        let message = self.queue.try_recv().ok()?;
        self.shared.release_unit(false);
        Some(message)
    }

    /// Closes the mailbox. Messages already queued stay for the receiver.
    pub(crate) fn close(&mut self) {
        self.shared.close();
    }

    /// Whether no message is queued now.
    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// The messages queued now.
    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    /// The free units.
    pub(crate) fn capacity(&self) -> usize {
        self.shared.room.0.load(SeqCst)
    }

    /// The units the mailbox has in all.
    pub(crate) fn max_capacity(&self) -> usize {
        self.shared.capacity
    }
}

impl<T> Drop for MailRx<T> {
    fn drop(&mut self) {
        self.shared.close();
        // Later pushes fail and their senders drop the messages; what is
        // queued is dropped here, each message outside every lock.
        self.queue.close();
        while let Some(message) = self.take() {
            if let Err(payload) = catch_sync(move || drop(message)) {
                discard(payload);
            }
        }
        // Nothing polls this receiver again, so the waker it registered (the
        // actor's, or the late drain's task) is not kept for the senders.
        self.shared.clear_reader();
    }
}

/// A receiver the run loop's helpers end: the actor's mailbox, its priority
/// lane and its system channel.
pub(crate) trait QueueRx: Send + 'static {
    type Item: Send + 'static;
    /// Closes the queue; what it holds stays for the receiver.
    fn close(&mut self);
    /// The next value if one is queued now. When none is, whether a value can
    /// still arrive: after the close tokio reports the queue empty only while
    /// a send that passed its check is still landing or a permit is out, and
    /// disconnected once neither is.
    fn try_recv_one(&mut self) -> Result<Self::Item, bool>;
    /// One poll for the next value; `None` once the queue has ended.
    fn poll_recv_one(&mut self, cx: &mut Context<'_>) -> Poll<Option<Self::Item>>;
    /// The free units.
    fn free_units(&self) -> usize;
    /// The units in all.
    fn all_units(&self) -> usize;
    /// The values queued now.
    fn queued(&self) -> usize;
}

impl<T: Send + 'static> QueueRx for MailRx<T> {
    type Item = T;

    fn close(&mut self) {
        MailRx::close(self);
        // The end closes the queue for good: a push from here on fails and its
        // sender drops the message.
        self.queue.close();
    }

    fn try_recv_one(&mut self) -> Result<T, bool> {
        match self.queue.try_recv() {
            Ok(message) => {
                self.shared.release_unit(false);
                Ok(message)
            }
            Err(error) => Err(matches!(error, mpsc::error::TryRecvError::Empty)),
        }
    }

    fn poll_recv_one(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.poll_recv(cx)
    }

    fn free_units(&self) -> usize {
        self.capacity()
    }

    fn all_units(&self) -> usize {
        self.max_capacity()
    }

    fn queued(&self) -> usize {
        self.len()
    }
}

impl<T: Send + 'static> QueueRx for mpsc::Receiver<T> {
    type Item = T;

    fn close(&mut self) {
        mpsc::Receiver::close(self);
    }

    fn try_recv_one(&mut self) -> Result<T, bool> {
        self.try_recv()
            .map_err(|error| matches!(error, mpsc::error::TryRecvError::Empty))
    }

    fn poll_recv_one(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.poll_recv(cx)
    }

    fn free_units(&self) -> usize {
        self.capacity()
    }

    fn all_units(&self) -> usize {
        self.max_capacity()
    }

    fn queued(&self) -> usize {
        self.len()
    }
}

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod tests;
