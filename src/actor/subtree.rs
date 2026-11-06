//! Subtree tokens: how a restart, a `terminate_child`, a `spawn_child` under
//! the id of a removed child and a subtree observer learn that every actor an
//! incarnation spawned has ended.
//!
//! Every spawned actor holds a token. A token is held by the actor's run
//! loop, its context, its registry guard and its timer and stream
//! forwarders, and by the token of each actor it spawned as their parent, so
//! a token is dropped only when the actor, everything it owned and everything
//! below it have ended. Its drop glue signals `gone`, and drop glue runs when
//! a task is aborted too, so the signal fires on every way an actor can end.
//! The hold also signals `released` as the last step of its own drop, after it
//! let go of its reference to the token: the actor let go of everything it
//! held, and the actors below it may still run. A wait that needs no timer for
//! the actor's own end waits for `released`, and one that finds `released`
//! without `gone` knows that something else still holds the token.
//!
//! A token also carries the budget of its subtree: the time that subtree can
//! outlive a killed root. Creating a child raises its parent's budget to at
//! least the child's own budget plus the child's subtree, and a raise
//! propagates to the root. The budget never decreases, so the bound stays
//! conservative when a child ends.
//!
//! The count of live supervisor children whose orderly stop has no bound is
//! kept beside the budget. A wait is unbounded only while that count is above
//! zero or the budget is too large to represent, and bounded again, counted
//! from the moment the count reaches zero, by what the subtree still carries.
//!
//! A token also carries what the aborts of its subtree's actors say about a
//! verdict on it: how many live actors below it (itself included) are owed an
//! abort that was not issued yet, and the latest instant an abort was issued
//! against one of them. A wait on the subtree gives no Unresponsive verdict
//! while an abort is owed, up to its own bound once more past its deadline,
//! nor before the latest abort plus the settle time: an abort that is late
//! holds the wait for at most the bound once more plus one more settle time,
//! and one recorded past that bound holds it no further. The end of an actor
//! that was owed an abort counts as that abort. The actor's own end releases
//! its mark, and so does each of these when it sees the task end: the join
//! arm of the supervisor's stop ladder and of its restart attempt, the two
//! observers of a lost runner, the reaper's entry, and the drop of the hold,
//! because a hold a forwarder keeps can outlive the task.
//!
//! Two lists of waiters sit on one token, and a change wakes them one after
//! the other, each under a catch of its own. The first is the library's own: a
//! restart attempt's wait for the previous incarnation's subtree, and the
//! waits of `terminate_child` and `spawn_child`; every task that parks there is
//! one the library polls, with tokio's own waker. The second is the subtree
//! observers' (see `SubtreeObserver`), whose tasks are users' and whose wakers
//! can panic. Tokio owns each list's batch and ends its wake at a waker that
//! panics, leaving the waiters behind it in that list unwoken; the library's
//! list holds no user's waker, so a panicking observer's waker can strand no
//! wait of the library, and strands only the observers behind it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use tokio::task::AbortHandle;
use tokio::time::{sleep_until, Instant};

use crate::actor::panic::contain_waker_panic;
use crate::actor::runtime::{bounded_wait, saturating_deadline};
use crate::actor::supervision::rules;

/// The state a token and every watch of it share.
#[derive(Debug)]
pub(crate) struct SubtreeShared {
    /// The budget of the subtree below this token, in nanoseconds. A value of
    /// `u64::MAX` is unbounded.
    below_nanos: AtomicU64,
    /// Whether the actor's own hold has been released: written once, by the
    /// last step of the hold's drop (see [`ReleaseSignal`]), after the hold's
    /// reference to the token dropped. A reader that finds it set and `gone`
    /// not set knows that something else still holds the token: an actor
    /// below the observed one, or a restart of one that is not dropped yet.
    released: AtomicBool,
    /// The token's signal (`gone`, the live unbounded-supervisor count and
    /// the generations a wait compares) and the aborts owed to and issued
    /// against the live actors of the subtree, under one lock.
    state: Mutex<SubtreeState>,
    /// The library's own waits (a restart attempt's, `terminate_child`'s and
    /// `spawn_child`'s), woken after every change of `state`. A wait creates
    /// its `Notified` before it reads `state`, and `notify_waiters` reaches
    /// every `Notified` created before the call, so a change after the read
    /// wakes it. No user's waker parks here.
    changed: tokio::sync::Notify,
    /// The subtree observers' waits, woken after every change of `state`,
    /// after `changed`, and read the same way. A user's waker parks here, and
    /// a panic of one ends tokio's wake of this list only.
    observers: tokio::sync::Notify,
    /// Set, never cleared, when the first observer's wait is created (before
    /// it registers and reads `state`). A change wakes the observers' list
    /// only once it is set, so a subtree nobody observes pays nothing for the
    /// list. An observer sets it before it reads `state`, and a change reads
    /// it after it wrote `state`: either the observer's read sees the change,
    /// or the change sees the flag and wakes the list the observer is on.
    observed: AtomicBool,
}

#[derive(Debug, Default)]
struct SubtreeState {
    signal: SubtreeSignal,
    aborts: SubtreeAborts,
}

/// The fields of a token's one signal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubtreeSignal {
    gone: bool,
    inf_live: u32,
    /// Bumped on every change of `inf_live`, so a net-zero change between
    /// two reads still re-arms a wait.
    inf_gen: u64,
    below_gen: u64,
}

/// What a subtree's aborts say about a verdict on it.
#[derive(Debug, Default, Clone, Copy)]
struct SubtreeAborts {
    /// Live actors of the subtree, its root included, that are owed an abort
    /// none has issued yet.
    owed: u32,
    /// The latest first abort issued against an actor of the subtree.
    latest: Option<Instant>,
}

/// The site the log names when a subtree observer's waker panics.
const OBSERVERS: &str = "subtree observers";

impl SubtreeShared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            below_nanos: AtomicU64::new(0),
            released: AtomicBool::new(false),
            state: Mutex::new(SubtreeState::default()),
            changed: tokio::sync::Notify::new(),
            observers: tokio::sync::Notify::new(),
            observed: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SubtreeState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wakes every wait whose `Notified` was created before this call: the
    /// library's own waits, then the observers', each list under a catch of
    /// its own. The observers' wakers belong to the observers' tasks, and a
    /// panic of one is kept out of the change that woke it. Tokio ends the wake
    /// of a list at the waker that panics, so the observers behind it stay
    /// asleep; the library's waits are on the other list and are never among
    /// them.
    fn wake_waits(&self, site: &'static str) {
        contain_waker_panic(site, None, || self.changed.notify_waiters());
        if self.observed.load(Ordering::SeqCst) {
            contain_waker_panic(OBSERVERS, None, || self.observers.notify_waiters());
        }
    }

    /// The `Notified` of a wait on this signal: the observers' list for a
    /// subtree observer's wait, the library's for any other.
    fn notified(&self, observer: bool) -> tokio::sync::futures::Notified<'_> {
        if observer {
            self.observed.store(true, Ordering::SeqCst);
            self.observers.notified()
        } else {
            self.changed.notified()
        }
    }

    /// Applies `change` to the signal, then wakes every wait whose `Notified`
    /// was created before this call.
    fn modify(&self, change: impl FnOnce(&mut SubtreeSignal)) {
        change(&mut self.lock().signal);
        self.wake_waits("subtree change");
    }

    fn signal(&self) -> SubtreeSignal {
        let read = self.lock().signal;
        #[cfg(test)]
        test_hook::run();
        read
    }

    fn update_aborts(&self, change: impl FnOnce(&mut SubtreeAborts)) {
        {
            let mut state = self.lock();
            change(&mut state.aborts);
            state.signal.below_gen = state.signal.below_gen.wrapping_add(1);
        }
        self.wake_waits("subtree change");
    }

    fn aborts(&self) -> SubtreeAborts {
        self.lock().aborts
    }
}

/// The record of the first abort issued against one incarnation's task, kept
/// beside its stop lane (every clone of the lane shares it) and outside the
/// lane's state, so it never adds a receiver to the lane.
///
/// Written once: the first abort wins, and a later abort or a re-raised
/// `Kill` never moves it. From the moment an abort is owed (a reaper entry
/// exists for the task, or a ladder raised its `Kill`) until the abort is
/// issued or the task ends, the record holds an owed mark, counted on the
/// incarnation's subtree token and on every ancestor's. The task's end
/// releases the mark when the actor ends, and when the join arm of a ladder,
/// an observer of a lost runner, the reaper's entry or the drop of the hold
/// sees the task end, whichever comes first. The record holds
/// its token weakly, so it never keeps a subtree alive, and the token's
/// shared state strongly (counters and a notification, which pin nothing), so
/// a reader can tell after the subtree ended that it did; the mark holds the
/// shared state of each token it counted on, so its release reaches them
/// whatever ended in between.
#[derive(Clone, Debug, Default)]
pub(crate) struct AbortRecord {
    shared: Arc<AbortShared>,
}

#[derive(Debug)]
struct AbortShared {
    linked: OnceLock<Linked>,
    owed: Mutex<Owed>,
    /// The incarnation's status plane (see `StatusCell`).
    status: crate::types::status_cell::StatusCell,
    /// Woken when the first abort is recorded.
    recorded: tokio::sync::Notify,
    /// The run loop's waker slot (see `WakeSlot`): woken by a raise of the
    /// stop lane.
    reader: crate::types::WakeSlot,
    /// The stop lane's cell: its state, version and receiver-gone flag.
    lane: crate::types::LaneCell,
    /// Woken, after the cell's lock is released, by every new version of the
    /// status plane and of the stop lane, and by the close of the plane's
    /// last sender. A waiter creates its notification before it reads, and
    /// re-checks what it waits for on every wake, so a wake meant for the
    /// other signal of the block is harmless.
    signals: tokio::sync::Notify,
    /// The incarnation's system plane. It stores no waker: a requester wakes
    /// the run loop through `reader` after it queued its request.
    system: Mutex<SystemSlot>,
    /// What the plane holds, as `SYSTEM_HELD` and `SYSTEM_OPEN`, written
    /// under `system`'s lock and read without it.
    system_state: std::sync::atomic::AtomicU8,
}

impl Default for AbortShared {
    fn default() -> Self {
        Self {
            linked: OnceLock::new(),
            owed: Mutex::new(Owed::default()),
            status: crate::types::status_cell::StatusCell::new(),
            signals: tokio::sync::Notify::new(),
            recorded: tokio::sync::Notify::new(),
            reader: crate::types::WakeSlot::default(),
            lane: crate::types::LaneCell::default(),
            system: Mutex::new(SystemSlot::default()),
            system_state: std::sync::atomic::AtomicU8::new(0),
        }
    }
}

/// What a record holds of its incarnation's subtree once the actor's hold
/// exists: the token weakly, so the record never keeps a subtree alive, and
/// the token's shared state strongly. The shared state is counters and a
/// notification and pins no actor, name, id or token, so a reader that comes
/// after the subtree ended still finds the signal that says so, and reads it
/// under the lock every writer of that signal takes.
#[derive(Debug)]
struct Linked {
    token: Weak<SubtreeToken>,
    shared: Arc<SubtreeShared>,
}

/// The plane holds a request outside its channel.
const SYSTEM_HELD: u8 = 1;
/// The plane's channel exists.
const SYSTEM_OPEN: u8 = 2;
/// The capacity of a plane's channel: in steady state a plane queues this
/// many requests, as many as the system channel of an actor without a held
/// slot does. The request held before the channel opened is taken by the run
/// loop with the channel's receiver, so a plane whose run loop has not yet
/// polled it holds that one request beside a full channel.
const SYSTEM_CAPACITY: usize = 64;

type StatusReply = tokio::sync::oneshot::Sender<crate::types::ActorStatusInfo>;

/// The incarnation's system plane. A query to an actor that has none queued
/// is held in the slot itself, so the first `get_status` allocates nothing
/// for the plane; a request that finds one held creates the channel, which
/// takes every later request. The held request is the oldest, and is
/// answered first.
#[derive(Debug, Default)]
enum SystemSlot {
    #[default]
    Idle,
    /// One request, held without a channel.
    One(crate::types::StatusRequest),
    /// The channel exists. `first` is the request held before it was created,
    /// until the run loop takes it.
    Open {
        first: Option<crate::types::StatusRequest>,
        tx: tokio::sync::mpsc::Sender<crate::types::SystemMessage>,
        rx: Option<tokio::sync::mpsc::Receiver<crate::types::SystemMessage>>,
    },
    Closed,
}

/// The run loop's end of the incarnation's system plane. The held request is
/// taken from the slot and the channel's receiver is taken here on the next
/// poll; dropping this closes the plane, so a request queued and not
/// answered fails, and a later `get_status` is refused.
///
/// The plane is read by its state bits and by emptiness and never registers
/// a waker of its own: a requester wakes the run loop through the
/// incarnation's waker slot after it queued its request
/// (`AbortRecord::submit_status`), and the run loop's stop branch arms that
/// slot earlier in the same select poll, so a request queued after this read
/// finds the slot armed, and one queued before it is seen by it.
///
/// The channel behind a full plane is tokio's bounded channel. A requester
/// that waits for room (past `SYSTEM_CAPACITY` queued requests) is woken by
/// tokio in two places. The close of the channel, at the plane's drop, is made
/// inside a catch (see [`contain_waker_panic`]): tokio ends its wake of the
/// parked requesters at a waker that panics, and the requesters behind it may
/// stay unwoken, so their futures finish at their next poll, which finds the
/// plane closed. The run loop's own receive is the other: it frees the slot a parked
/// requester takes, and it returns the request it took, which a catch around
/// it would drop, so a panic of that requester's waker there is outside the
/// catches the crate puts on the wakes it makes itself (see
/// [`AbortRecord::notify_signals`]). A request held outside the channel is
/// dropped inside a catch as well, and so is one queued in the channel when
/// the plane ends.
///
/// The priority lane is tokio's bounded channel too, and a `send_priority`
/// caller that waits for room on a full lane is woken by tokio in the same two
/// places: the close of the lane at the end of the message loop is made inside
/// a catch, with the same limit (the senders behind a waker that panics may
/// stay unwoken, and find the lane closed at their next poll), and the run loop's
/// receive from the lane frees the slot a parked sender takes, outside a
/// catch, for the same reason.
#[derive(Debug)]
pub(crate) struct SystemPlane {
    record: AbortRecord,
    rx: Option<tokio::sync::mpsc::Receiver<crate::types::SystemMessage>>,
    /// The actor the plane belongs to, named by the logs of its end.
    owner: crate::types::ActorId,
    /// The system whose helper slot takes a receiver that ends with a slot
    /// still out.
    system: std::sync::Arc<crate::system::ActorSystem>,
}

impl SystemPlane {
    pub(crate) fn new(
        record: AbortRecord,
        owner: crate::types::ActorId,
        system: std::sync::Arc<crate::system::ActorSystem>,
    ) -> Self {
        Self {
            record,
            rx: None,
            owner,
            system,
        }
    }

    fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<crate::types::SystemMessage>> {
        let shared = &self.record.shared;
        if shared.system_state.load(Ordering::Acquire) & SYSTEM_HELD != 0 {
            let mut slot = shared.system.lock().unwrap_or_else(|p| p.into_inner());
            let held = match &mut *slot {
                SystemSlot::One(_) => match std::mem::take(&mut *slot) {
                    SystemSlot::One(held) => Some(held),
                    _ => None,
                },
                SystemSlot::Open { first, .. } => first.take(),
                _ => None,
            };
            if held.is_some() {
                shared
                    .system_state
                    .fetch_and(!SYSTEM_HELD, Ordering::Release);
            }
            drop(slot);
            if let Some(request) = held {
                return std::task::Poll::Ready(Some(crate::types::SystemMessage::GetStatus(
                    request,
                )));
            }
        }
        #[cfg(test)]
        between_loads();
        if self.rx.is_none() {
            if shared.system_state.load(Ordering::Acquire) & SYSTEM_OPEN == 0 {
                return std::task::Poll::Pending;
            }
            let mut slot = shared.system.lock().unwrap_or_else(|p| p.into_inner());
            if let SystemSlot::Open { first, rx, .. } = &mut *slot {
                self.rx = rx.take();
                // A request held before the channel opened is older than
                // every request in it, and may have been queued after this
                // poll's read of the held bit: it is handed out first.
                if let Some(request) = first.take() {
                    shared
                        .system_state
                        .fetch_and(!SYSTEM_HELD, Ordering::Release);
                    drop(slot);
                    return std::task::Poll::Ready(Some(crate::types::SystemMessage::GetStatus(
                        request,
                    )));
                }
            }
        }
        match self.rx.as_mut() {
            // Empty and open: pend without registering on the channel.
            Some(rx) if rx.is_empty() && !rx.is_closed() => std::task::Poll::Pending,
            Some(rx) => rx.poll_recv(cx),
            // Open, and the receiver is not in the slot: only this
            // owner takes it, so this is not reached.
            None => std::task::Poll::Pending,
        }
    }

    /// Whether the run loop's poll of the plane would pend now: no request
    /// held, and no channel opened, or its receiver taken here, empty and
    /// open. A channel opened and not yet taken reads busy, so the poll takes
    /// its receiver.
    pub(crate) fn is_idle(&self) -> bool {
        let state = self.record.shared.system_state.load(Ordering::Acquire);
        if state & SYSTEM_HELD != 0 {
            return false;
        }
        match &self.rx {
            Some(rx) => rx.is_empty() && !rx.is_closed(),
            None => state & SYSTEM_OPEN == 0,
        }
    }

    /// The next request on the plane; pends while no request is queued.
    pub(crate) async fn recv(&mut self) -> Option<crate::types::SystemMessage> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Drops a request held outside the channel, inside a catch (see
    /// [`contain_waker_panic`]).
    fn drop_held(&self, request: Option<crate::types::StatusRequest>) {
        contain_waker_panic("status request", Some(&self.owner), move || drop(request));
    }
}

impl Drop for SystemPlane {
    fn drop(&mut self) {
        // The plane's contents are moved out under the lock and dropped after
        // it is released: the channel's close, the wakes of parked requesters
        // and the drops of queued requests (which wake their requesters) run
        // outside the lock.
        let open = {
            let mut slot = self
                .record
                .shared
                .system
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            std::mem::replace(&mut *slot, SystemSlot::Closed)
        };
        // A request held outside the channel is dropped, and so is the
        // slot's sender: the plane does not rely on a requester's sender
        // having gone out of scope. The drop of a request wakes its
        // requester, whose waker belongs to the requester's task, so it runs
        // inside a catch: a panic of that waker must not turn the actor's
        // reason into a failure, and the plane is dropped before the reason is
        // final. Each receiver, the one taken here and the one still in the
        // slot, is ended like any queue: closed and drained, and handed to a
        // task that drops what a sender pushes through a slot it reserved
        // before the close.
        let slot_rx = match open {
            SystemSlot::Open { first, tx, rx } => {
                self.drop_held(first);
                drop(tx);
                rx
            }
            SystemSlot::One(request) => {
                self.drop_held(Some(request));
                None
            }
            SystemSlot::Idle | SystemSlot::Closed => None,
        };
        for rx in self.rx.take().into_iter().chain(slot_rx) {
            if let Some(panic) =
                crate::actor::runtime::end_queue_late(rx, &self.system, &self.owner)
            {
                crate::actor::panic::log_waker_panic("status queue", &panic, Some(&self.owner));
            }
        }
    }
}

// Runs once, between the two reads of the plane's state in a poll, when a
// test installs it: it lets a test queue requests inside that window.
#[cfg(test)]
thread_local! {
    static BETWEEN_LOADS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn between_loads() {
    if let Some(hook) = BETWEEN_LOADS.with(|h| h.borrow_mut().take()) {
        hook();
    }
}

#[cfg(test)]
#[path = "subtree_system_tests.rs"]
mod system_tests;

#[cfg(test)]
#[path = "subtree_waker_tests.rs"]
mod waker_tests;

#[derive(Debug, Default)]
struct Owed {
    /// The instant of the first abort issued, written once.
    at: Option<Instant>,
    mark: Option<OwedMark>,
    /// The actor ended: nothing is owed to it any more.
    ended: bool,
}

/// One owed abort, counted on the shared state of the incarnation's token
/// and every ancestor's; its drop takes the count back.
#[derive(Debug)]
struct OwedMark {
    chain: Vec<Arc<SubtreeShared>>,
}

impl Drop for OwedMark {
    fn drop(&mut self) {
        for shared in &self.chain {
            shared.update_aborts(|a| a.owed = a.owed.saturating_sub(1));
        }
    }
}

impl AbortRecord {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The incarnation's status plane storage.
    pub(crate) fn status_cell(&self) -> &crate::types::status_cell::StatusCell {
        &self.shared.status
    }

    /// A future woken by the next [`notify_signals`](Self::notify_signals).
    /// Create it before reading what it waits for, so a change after the read
    /// wakes it.
    pub(crate) fn signalled(&self) -> tokio::sync::futures::Notified<'_> {
        self.shared.signals.notified()
    }

    /// Wakes every task waiting on the block's signals, after the lock that
    /// guarded the change is released: a stop lane's new version, a status
    /// plane's new version and the close of the plane. The waiters are other
    /// tasks' (`wait_stopped`, the waits on the status plane, a bounded
    /// stop's wait for the end), and a waker belongs to the task that polls
    /// with it. A panic of one waker's wake is contained here (see
    /// [`contain_waker_panic`]): the raise that is waking, the actor that is
    /// publishing its status and the drop that closes the plane go on, and a
    /// request or a status that was written stays written. Every raise and
    /// every plane write wakes through this call, and nothing else calls
    /// `notify_waiters` on the notification.
    pub(crate) fn notify_signals(&self) {
        contain_waker_panic("signals", None, || self.shared.signals.notify_waiters());
    }

    /// Whether both records are the same incarnation's block.
    pub(crate) fn same_block(&self, other: &AbortRecord) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// A sender of the plane's channel, when it exists.
    #[cfg(test)]
    pub(crate) fn system_tx_for_test(
        &self,
    ) -> Option<tokio::sync::mpsc::Sender<crate::types::SystemMessage>> {
        match &*self.shared.system.lock().unwrap_or_else(|p| p.into_inner()) {
            SystemSlot::Open { tx, .. } => Some(tx.clone()),
            _ => None,
        }
    }

    /// The run loop's waker slot.
    pub(crate) fn reader(&self) -> &crate::types::WakeSlot {
        &self.shared.reader
    }

    /// The stop lane's cell.
    pub(crate) fn lane(&self) -> &crate::types::LaneCell {
        &self.shared.lane
    }

    /// Queues a status request on the incarnation's system plane, waiting for
    /// room when the plane is full, then wakes the run loop with no await
    /// between the queueing and the wake. `Err` once the run loop closed the
    /// plane.
    ///
    /// No clone of the plane's sender lives across the requester's await on
    /// the reply: the one cloned here goes out of scope when this call
    /// returns, so once the run loop closes the plane and the last sender is
    /// gone, a request still queued is freed and its requester gets an
    /// answer instead of waiting on a value nobody will take. The plane's end
    /// does not depend on this: it also drains the queue and hands a
    /// receiver with slots still reserved to a task that drops late values.
    pub(crate) async fn submit_status(&self, reply: StatusReply) -> Result<(), ()> {
        // Each request carries its own requester kind, so the reply to it
        // reads that request's kind whatever else is queued.
        let request = crate::types::StatusRequest {
            reply,
            task: tokio::task::try_id().is_some(),
        };
        let tx = {
            let mut slot = self.shared.system.lock().unwrap_or_else(|p| p.into_inner());
            match std::mem::take(&mut *slot) {
                SystemSlot::Closed => {
                    *slot = SystemSlot::Closed;
                    return Err(());
                }
                SystemSlot::Idle => {
                    *slot = SystemSlot::One(request);
                    self.shared
                        .system_state
                        .fetch_or(SYSTEM_HELD, Ordering::Release);
                    drop(slot);
                    self.shared.reader.wake();
                    return Ok(());
                }
                SystemSlot::One(first) => {
                    let (tx, rx) = tokio::sync::mpsc::channel(SYSTEM_CAPACITY);
                    *slot = SystemSlot::Open {
                        first: Some(first),
                        tx: tx.clone(),
                        rx: Some(rx),
                    };
                    self.shared
                        .system_state
                        .fetch_or(SYSTEM_OPEN, Ordering::Release);
                    tx
                }
                SystemSlot::Open { first, tx, rx } => {
                    let sender = tx.clone();
                    *slot = SystemSlot::Open { first, tx, rx };
                    sender
                }
            }
        };
        tx.send(crate::types::SystemMessage::GetStatus(request))
            .await
            .map_err(|_| ())?;
        self.shared.reader.wake();
        Ok(())
    }

    /// The shared state of the linked token and of every ancestor's.
    fn chain(&self) -> Vec<Arc<SubtreeShared>> {
        let mut chain = Vec::new();
        let token = self
            .shared
            .linked
            .get()
            .and_then(|linked| linked.token.upgrade());
        #[cfg(test)]
        test_hook::run_in_chain();
        let mut at = token.as_deref();
        while let Some(current) = at {
            chain.push(current.shared.clone());
            at = current.parent.as_deref();
        }
        chain
    }

    /// An abort is now owed to the task: marks it unless an abort was
    /// already issued, a mark is already held, or the actor ended.
    pub(crate) fn owe(&self) {
        let mut owed = self.shared.owed.lock().unwrap_or_else(|p| p.into_inner());
        if owed.ended || owed.mark.is_some() || owed.at.is_some() {
            return;
        }
        let chain = self.chain();
        for shared in &chain {
            shared.update_aborts(|a| a.owed = a.owed.saturating_add(1));
        }
        owed.mark = Some(OwedMark { chain });
    }

    /// An abort is now owed to `task`: marks it as [`AbortRecord::owe`]
    /// does, unless the task already finished, in which case nothing is owed
    /// and the mark is released as [`AbortRecord::abort`] releases it. A
    /// hold that outlives the task (a forwarder still ending) keeps no mark
    /// alive over a task that ended before the owe.
    pub(crate) fn owe_for(&self, task: &AbortHandle) {
        if task.is_finished() {
            self.release();
        } else {
            self.owe();
        }
    }

    /// A party that watches the incarnation's task saw it end (a ladder's
    /// join, an observer of the status plane's end): releases the owed mark
    /// and takes none again, exactly as the actor's own end releases it, so
    /// a hold that outlives the task keeps no mark alive. Idempotent.
    pub(crate) fn task_ended(&self) {
        self.release();
    }

    /// Aborts `task`, recording the instant unless an abort was recorded
    /// already. Every abort the crate issues against an incarnation's task
    /// goes through here. A task that already finished gets no abort and no
    /// record: nothing is owed to a task that ended, so the owed mark is
    /// released instead, as the actor's end releases it, and a hold that
    /// outlives the task (a forwarder still ending) keeps no mark alive.
    pub(crate) fn abort(&self, task: &AbortHandle) {
        if task.is_finished() {
            self.release();
            return;
        }
        self.record(Instant::now());
        task.abort();
    }

    /// Records `now` as the first abort. A record already written stands.
    ///
    /// The chain of a record with no owed mark is read inside the critical
    /// section, and not at all once the actor has ended: the release of the
    /// actor's hold takes this lock before it lets go of the token, so no
    /// reader of the chain holds the token when the hold's release is
    /// written (see [`ReleaseSignal`]). An abort recorded after the actor
    /// ended reaches no ancestor: nothing is left of it to wait for.
    fn record(&self, now: Instant) {
        let (mark, chain) = {
            let mut owed = self.shared.owed.lock().unwrap_or_else(|p| p.into_inner());
            if owed.at.is_some() {
                return;
            }
            owed.at = Some(now);
            let mark = owed.mark.take();
            let chain = match &mark {
                Some(mark) => mark.chain.clone(),
                None if owed.ended => Vec::new(),
                None => self.chain(),
            };
            (mark, chain)
        };
        #[cfg(test)]
        test_hook::run_after_write();
        self.shared.recorded.notify_waiters();
        for shared in &chain {
            shared.update_aborts(|a| a.latest = Some(a.latest.map_or(now, |l| l.max(now))));
        }
        drop(mark);
    }

    /// The actor ended: releases the owed mark, and none is taken again.
    ///
    /// The end of an actor that was owed an abort counts as that abort for
    /// the waits on its subtree. Its end cancels the runners of its children,
    /// whose guards owe their aborts only once the runtime drops those
    /// runners, so the subtree needs the same settle time after the end that
    /// it gets after a recorded abort. The mark is taken and the end written
    /// in one critical section, the instant of the end is raised as the
    /// latest abort on every token the mark counted on, and only then does
    /// the mark drop: no reader sees the subtree with nothing owed and
    /// nothing recorded in between. With no mark held, nothing is raised.
    fn release(&self) {
        let mark = {
            let mut owed = self.shared.owed.lock().unwrap_or_else(|p| p.into_inner());
            owed.ended = true;
            owed.mark.take()
        };
        if let Some(mark) = &mark {
            let now = Instant::now();
            for shared in &mark.chain {
                shared.update_aborts(|a| a.latest = Some(a.latest.map_or(now, |l| l.max(now))));
            }
        }
        drop(mark);
    }

    /// The first abort's instant, and whether an abort is owed and not yet
    /// issued. The instant is written and the mark taken in one critical
    /// section, so an abort is never reported both recorded and owed.
    pub(crate) fn view(&self) -> (Option<Instant>, bool) {
        let read = {
            let owed = self.shared.owed.lock().unwrap_or_else(|p| p.into_inner());
            (owed.at, owed.mark.is_some())
        };
        #[cfg(test)]
        test_hook::run();
        read
    }

    /// A future woken when the first abort is recorded; create it before
    /// reading the record so no record is missed.
    pub(crate) fn recorded(&self) -> tokio::sync::futures::Notified<'_> {
        self.shared.recorded.notified()
    }

    /// Links the record to the incarnation's subtree token.
    fn link(&self, token: &Arc<SubtreeToken>) {
        let _ = self.shared.linked.set(Linked {
            token: Arc::downgrade(token),
            shared: token.shared.clone(),
        });
    }

    /// An observer of the incarnation's subtree, or `None` while the record
    /// is not linked, which is before the actor's hold is built (a spawn
    /// registers the actor first) and for a record that never got one.
    ///
    /// The link is set before the actor's task is spawned and a task cannot
    /// end before it is spawned, so a reader that has seen the actor end
    /// finds the link unless the actor never ran. The answer to "has the
    /// subtree ended" is read from the signal under its lock, never inferred
    /// from a failed upgrade of the token.
    pub(crate) fn subtree_watch(&self) -> Option<SubtreeWatch> {
        self.shared.linked.get().map(|linked| SubtreeWatch {
            shared: linked.shared.clone(),
            observer: false,
        })
    }
}

/// The token of one spawned actor. Held by the actor's [`SubtreeHold`] and
/// by the tokens of the actors it spawned.
#[derive(Debug)]
pub(crate) struct SubtreeToken {
    shared: Arc<SubtreeShared>,
    /// The time this actor can outlive a killed parent, in nanoseconds.
    own_nanos: u64,
    parent: Option<Arc<SubtreeToken>>,
}

impl SubtreeToken {
    /// Raises the budget below this token to at least `candidate`, and the
    /// parent's the same way.
    fn raise_below(&self, candidate: u64) {
        let previous = self
            .shared
            .below_nanos
            .fetch_max(candidate, Ordering::SeqCst);
        if candidate > previous {
            self.shared
                .modify(|s| s.below_gen = s.below_gen.wrapping_add(1));
            if let Some(parent) = &self.parent {
                parent.raise_below(self.own_nanos.saturating_add(candidate));
            }
        }
    }

    /// Raises or lowers by one the live-supervisor count of `token` and of
    /// every ancestor.
    fn shift_infinity(token: &Arc<SubtreeToken>, raise: bool) {
        let mut at = Some(token);
        while let Some(current) = at {
            current.shared.modify(|s| {
                s.inf_live = if raise {
                    s.inf_live.saturating_add(1)
                } else {
                    s.inf_live.saturating_sub(1)
                };
                s.inf_gen = s.inf_gen.wrapping_add(1);
            });
            at = current.parent.as_ref();
        }
    }
}

impl Drop for SubtreeToken {
    fn drop(&mut self) {
        self.shared.modify(|s| s.gone = true);
    }
}

/// How a child's token attaches to its parent's.
pub(crate) struct SubtreeLink {
    pub parent: Arc<SubtreeToken>,
    /// The time the child can outlive a killed parent. A value beyond what a
    /// `u64` of nanoseconds holds reads as unbounded.
    pub own: Duration,
    /// Whether the child is a supervisor whose orderly stop has no bound.
    pub infinity: bool,
}

/// Lowers the live-supervisor count of the ancestors of an unbounded
/// supervisor child when the child's actor ends, whether or not anything
/// below it has.
#[derive(Debug)]
struct InfinityMark {
    above: Arc<SubtreeToken>,
}

impl Drop for InfinityMark {
    fn drop(&mut self) {
        SubtreeToken::shift_infinity(&self.above, false);
    }
}

/// What an actor holds of its token for as long as it runs: its context and
/// its registry guard share one.
#[derive(Debug)]
pub(crate) struct SubtreeHold {
    token: Arc<SubtreeToken>,
    _mark: Option<InfinityMark>,
    /// The abort record of the actor's incarnation, released when the hold
    /// drops: the actor ended, so no abort is owed to it any more.
    record: AbortRecord,
    /// Declared last, so the hold's drop ends with it: it says the hold has
    /// been released once the hold's reference to the token is gone.
    _released: ReleaseSignal,
}

/// The last thing the drop of a [`SubtreeHold`] does: marks the hold released
/// and wakes the waits on the subtree. Fields drop in declaration order after
/// the hold's own `drop`, so this runs after the hold's reference to the
/// token dropped. A hold with nothing below it therefore reads released and
/// the subtree gone together, and a reader of the subtree that finds the hold
/// released and the subtree not gone knows that something else still holds
/// the token: an actor below, or a restart of one that is not dropped yet. No
/// reader of the abort chain is such a holder: the chain is read under the
/// lock the hold's release takes before it lets go of the token.
#[derive(Debug)]
struct ReleaseSignal(Arc<SubtreeShared>);

impl Drop for ReleaseSignal {
    fn drop(&mut self) {
        #[cfg(test)]
        test_hook::run_before_release();
        self.0.released.store(true, Ordering::SeqCst);
        // An observer's waker belongs to the observer's task: a panic from it
        // must not unwind into the drop that releases the hold.
        self.0.wake_waits("subtree release");
    }
}

impl Drop for SubtreeHold {
    fn drop(&mut self) {
        self.record.release();
        #[cfg(test)]
        test_hook::run_after_release();
    }
}

impl SubtreeHold {
    /// The hold of a new actor whose stop lane carries `record`: the record
    /// is linked to the actor's token, so its aborts reach the subtree's
    /// waits.
    pub(crate) fn with_record(link: Option<SubtreeLink>, record: AbortRecord) -> Arc<Self> {
        let hold = Self::build(link, record);
        hold.record.link(&hold.token);
        hold
    }

    /// The hold of a new actor: a root, or the child described by `link`.
    #[cfg(test)]
    pub(crate) fn new(link: Option<SubtreeLink>) -> Arc<Self> {
        Self::build(link, AbortRecord::new())
    }

    fn build(link: Option<SubtreeLink>, record: AbortRecord) -> Arc<Self> {
        let (token, mark) = match link {
            None => (
                Arc::new(SubtreeToken {
                    shared: SubtreeShared::new(),
                    own_nanos: 0,
                    parent: None,
                }),
                None,
            ),
            Some(link) => {
                let own_nanos = rules::saturating_nanos(link.own);
                let token = Arc::new(SubtreeToken {
                    shared: SubtreeShared::new(),
                    own_nanos,
                    parent: Some(link.parent.clone()),
                });
                link.parent.raise_below(own_nanos);
                let mark = link.infinity.then(|| {
                    SubtreeToken::shift_infinity(&link.parent, true);
                    InfinityMark {
                        above: link.parent.clone(),
                    }
                });
                (token, mark)
            }
        };
        let released = ReleaseSignal(token.shared.clone());
        Arc::new(Self {
            token,
            _mark: mark,
            record,
            _released: released,
        })
    }

    /// The token the actor's own children attach to.
    pub(crate) fn token(&self) -> Arc<SubtreeToken> {
        self.token.clone()
    }

    /// An observer of the actor's subtree that never keeps it alive.
    pub(crate) fn watch(&self) -> SubtreeWatch {
        SubtreeWatch {
            shared: self.token.shared.clone(),
            observer: false,
        }
    }
}

/// An observer of one actor's subtree. Cloning it is cheap, and holding it
/// never keeps the subtree alive.
///
/// Its waits park on the library's list of the token, or, once
/// [`for_observer`](Self::for_observer) made it a subtree observer's, on the
/// observers' list (see the module note).
#[derive(Clone, Debug)]
pub(crate) struct SubtreeWatch {
    shared: Arc<SubtreeShared>,
    /// Whether the watch's waits are a subtree observer's, whose task is a
    /// user's.
    observer: bool,
}

impl SubtreeWatch {
    /// This watch for a subtree observer's waits: they park on the observers'
    /// list, where a panic of a user's waker can strand only other observers.
    pub(crate) fn for_observer(mut self) -> Self {
        self.observer = true;
        self
    }

    /// Whether the actor and everything below it ended.
    pub(crate) fn is_gone(&self) -> bool {
        self.shared.signal().gone
    }

    /// Whether the actor's own hold has been released: the actor let go of
    /// what it held, and the actors below it may still run.
    #[cfg(test)]
    pub(crate) fn is_released(&self) -> bool {
        self.shared.released.load(Ordering::SeqCst)
    }

    /// Resolves once the actor's own hold has been released, at once when it
    /// was, whatever runs below the actor. It reads no clock and needs no
    /// timer, so a caller on any runtime, or none, can wait through it. When
    /// it resolves and the subtree is not gone, something else holds the
    /// token: an actor below the observed one, or a restart of one that is
    /// not dropped yet.
    pub(crate) async fn released(&self) {
        loop {
            // Created before the read: a release after the read wakes this
            // wait.
            let notified = self.shared.notified(self.observer);
            tokio::pin!(notified);
            notified.as_mut().enable();
            let released = self.shared.released.load(Ordering::SeqCst);
            #[cfg(test)]
            test_hook::run();
            if released {
                return;
            }
            notified.await;
        }
    }

    /// The live-supervisor count below the actor.
    fn infinity_live(&self) -> u32 {
        self.shared.signal().inf_live
    }

    /// The bound of a wait on the actor's subtree that starts now: `None`
    /// while a supervisor child with an unbounded orderly stop is alive below
    /// it (or when the budget saturated), else the budget plus the settle
    /// time, capped like every wait.
    pub(crate) fn release_bound(&self) -> Option<Duration> {
        rules::successor_bound(
            self.shared.below_nanos.load(Ordering::SeqCst),
            self.infinity_live(),
        )
        .map(bounded_wait)
    }

    /// The latest abort issued against an actor of the subtree, and whether
    /// a live actor of it is owed an abort not yet issued.
    pub(crate) fn abort_view(&self) -> (Option<Instant>, bool) {
        let aborts = self.shared.aborts();
        (aborts.latest, aborts.owed > 0)
    }
}

#[cfg(test)]
impl SubtreeWatch {
    /// Resolves once the actor and everything below it ended, at once when
    /// they did.
    pub(crate) async fn gone(&self) {
        self.wait_until(|s| s.gone).await;
    }

    /// Resolves once no supervisor child with an unbounded orderly stop is
    /// alive below the actor, at once when none is.
    pub(crate) async fn infinity_cleared(&self) {
        self.wait_until(|s| s.inf_live == 0).await;
    }

    async fn wait_until(&self, done: impl Fn(&SubtreeSignal) -> bool) {
        loop {
            let notified = self.shared.notified(self.observer);
            tokio::pin!(notified);
            notified.as_mut().enable();
            if done(&self.shared.signal()) {
                return;
            }
            notified.await;
        }
    }
}

/// A wait on a subtree, with the deadline it expires at.
///
/// The deadline is armed from the moment the wait has a finite bound, so a
/// subtree with an unbounded supervisor child below is waited for without
/// one, and the bound counts from the moment the last such child ended. A
/// new unbounded supervisor child disarms the deadline again, and a raise of
/// the budget extends it to the later of the armed deadline and the raise's
/// moment plus the new bound.
pub(crate) struct SubtreeWait {
    watch: SubtreeWatch,
    /// The signal as this wait last read it.
    seen: SubtreeSignal,
    deadline: Option<Instant>,
    /// The bound the deadline was last armed with: the cap of a wait for an
    /// owed abort counts it once more from the deadline.
    bound: Duration,
    /// The budget read when the deadline was last armed: a wake on the
    /// budget's signal that finds it unchanged came from the subtree's
    /// aborts and arms nothing.
    below_seen: u64,
}

/// What woke a [`SubtreeWait`].
enum WaitWake {
    Below,
    Deadline,
}

impl SubtreeWait {
    /// Starts a wait on `watch`, arming its deadline when it has a bound.
    pub(crate) fn new(watch: &SubtreeWatch) -> Self {
        let seen = watch.shared.signal();
        let mut wait = Self {
            watch: watch.clone(),
            seen,
            deadline: None,
            bound: Duration::ZERO,
            below_seen: 0,
        };
        wait.rearm();
        wait
    }

    /// Whether the subtree ended.
    pub(crate) fn gone(&self) -> bool {
        self.watch.is_gone()
    }

    /// Whether the wait may give its verdict now: the armed deadline passed,
    /// no live actor of the subtree is owed an abort not yet issued (up to
    /// the cap), and the latest abort issued against one of them is at least
    /// the settle time old ([`rules::verdict_floor`]).
    pub(crate) fn expired(&self) -> bool {
        self.verdict_at().is_some_and(|at| Instant::now() >= at)
    }

    /// The instant the verdict may be given, as the subtree reads now.
    fn verdict_at(&self) -> Option<Instant> {
        let deadline = self.deadline?;
        let (latest, owed) = self.watch.abort_view();
        Some(Instant::from_std(rules::verdict_floor(
            deadline.into_std(),
            self.bound,
            latest.map(Instant::into_std),
            owed,
        )))
    }

    #[cfg(test)]
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Re-reads the bound: unbounded disarms the deadline, a finite bound arms
    /// it from now or extends it, never shortens it.
    fn rearm(&mut self) {
        self.below_seen = self.watch.shared.below_nanos.load(Ordering::SeqCst);
        match self.watch.release_bound() {
            None => self.deadline = None,
            Some(bound) => {
                let at = saturating_deadline(Instant::now(), bound);
                self.deadline = Some(self.deadline.map_or(at, |armed| armed.max(at)));
                self.bound = self.bound.max(bound);
            }
        }
    }

    /// A wake on the budget's signal: arms again only when the budget rose,
    /// since the subtree's aborts share the signal.
    fn rearm_if_raised(&mut self) {
        if self.watch.shared.below_nanos.load(Ordering::SeqCst) != self.below_seen {
            self.rearm();
        }
    }

    /// Resolves at the next change that can decide the wait: the subtree
    /// ending, the count of unbounded supervisor children or the budget
    /// changing (after which the deadline is re-read), a change of the
    /// subtree's aborts, or the instant the verdict may be given.
    pub(crate) async fn changed(&mut self) {
        let shared = self.watch.shared.clone();
        // Created before the read: a change after the read wakes this wait.
        let notified = shared.notified(self.watch.observer);
        tokio::pin!(notified);
        notified.as_mut().enable();
        let current = shared.signal();
        if current.gone {
            return;
        }
        // A change since this wait last read the signal is handled at once.
        let wake = if current != self.seen {
            WaitWake::Below
        } else {
            let deadline = self.verdict_at();
            tokio::select! {
                biased;
                () = &mut notified => WaitWake::Below,
                _ = sleep_until_opt(deadline) => WaitWake::Deadline,
            }
        };
        if let WaitWake::Below = wake {
            let now = shared.signal();
            let was = std::mem::replace(&mut self.seen, now);
            if now.gone {
                return;
            }
            if now.inf_gen != was.inf_gen {
                self.rearm();
            } else if now.below_gen != was.below_gen {
                self.rearm_if_raised();
            }
        }
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod abort_record_tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A root and one child below it, the child's record linked to its
    /// token, the child's task held by the test.
    fn child_of(root: &Arc<SubtreeHold>) -> (Arc<SubtreeHold>, AbortRecord) {
        let record = AbortRecord::new();
        let link = SubtreeLink {
            parent: root.token(),
            own: ms(200),
            infinity: false,
        };
        (SubtreeHold::with_record(Some(link), record.clone()), record)
    }

    #[tokio::test(start_paused = true)]
    async fn an_owed_abort_is_counted_up_the_chain_and_released_by_its_record() {
        let root = SubtreeHold::new(None);
        let (child, record) = child_of(&root);
        let task = tokio::spawn(std::future::pending::<()>());
        record.owe();
        record.owe();
        assert_eq!(root.watch().abort_view(), (None, true));
        assert_eq!(child.watch().abort_view(), (None, true));
        tokio::time::advance(ms(30)).await;
        let at = Instant::now();
        record.abort(&task.abort_handle());
        assert_eq!(root.watch().abort_view(), (Some(at), false));
        assert_eq!(child.watch().abort_view(), (Some(at), false));
        record.owe();
        assert_eq!(
            record.view(),
            (Some(at), false),
            "nothing is owed after an abort"
        );
    }

    /// The end of an actor that was owed an abort releases the mark and
    /// counts as the subtree's latest abort, so a wait on the subtree gives
    /// the cascade of the end the settle time.
    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_actor_releases_its_owed_abort_and_none_is_taken_again() {
        let root = SubtreeHold::new(None);
        let (child, record) = child_of(&root);
        record.owe();
        tokio::time::advance(ms(30)).await;
        let end = Instant::now();
        drop(child);
        assert_eq!(root.watch().abort_view(), (Some(end), false));
        record.owe();
        assert_eq!(
            root.watch().abort_view(),
            (Some(end), false),
            "an ended actor owes nothing"
        );
        assert_eq!(record.view(), (None, false));
    }

    /// An abort recorded after the actor's hold was released reaches no
    /// ancestor, though an actor below it keeps the token alive: the chain is
    /// read under the lock the release takes first, and not at all once the
    /// actor has ended, so no reader of it holds the token when the release
    /// is written.
    #[tokio::test(start_paused = true)]
    async fn an_abort_recorded_after_the_hold_was_released_reaches_no_ancestor() {
        let root = SubtreeHold::new(None);
        let (middle, record) = child_of(&root);
        let below = SubtreeHold::new(Some(SubtreeLink {
            parent: middle.token(),
            own: ms(50),
            infinity: false,
        }));
        let task = tokio::spawn(std::future::pending::<()>());
        drop(middle);
        assert!(!record.subtree_watch().expect("linked").is_gone());
        record.abort(&task.abort_handle());
        assert_eq!(
            root.watch().abort_view(),
            (None, false),
            "the actor ended: the abort reaches no ancestor"
        );
        assert!(record.view().0.is_some(), "the instant is recorded");
        drop(below);
    }

    /// R3 proof: the chain is read under the lock the hold's release takes.
    /// The record's chain read holds a temporary strong reference to the
    /// token; a hold dropped on another thread meanwhile must not write its
    /// release while that reference lives.
    #[test]
    fn r3_the_chain_is_read_under_the_lock_the_release_takes() {
        use std::sync::mpsc;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let task = rt.spawn(std::future::pending::<()>());
        let record = AbortRecord::new();
        let hold = SubtreeHold::with_record(None, record.clone());
        let watch = hold.watch();
        let seen = Arc::new(Mutex::new(None));
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let joined = Arc::new(Mutex::new(None));
        test_hook::arm_in_chain({
            let (seen, joined) = (seen.clone(), joined.clone());
            move || {
                let dropper = std::thread::spawn(move || {
                    test_hook::arm_before_release({
                        let seen = seen.clone();
                        move || {
                            *seen.lock().unwrap() = Some((watch.is_released(), watch.is_gone()));
                            let _ = done_tx.send(());
                        }
                    });
                    drop(hold);
                });
                *joined.lock().unwrap() = Some(dropper);
                // Where the release is not held back by the record's lock it
                // is written within this wait.
                let _ = done_rx.recv_timeout(Duration::from_millis(500));
            }
        });
        record.abort(&task.abort_handle());
        joined
            .lock()
            .unwrap()
            .take()
            .expect("dropper")
            .join()
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            Some((false, true)),
            "the release was written while the record's chain read held the token"
        );
    }

    /// R3 proof: an abort recorded between the hold's release of the record
    /// and the drop of the hold's token reads no chain.
    #[test]
    fn r3_an_abort_between_the_record_release_and_the_token_drop_reads_no_chain() {
        use std::sync::mpsc;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let task = rt.spawn(std::future::pending::<()>());
        let abort = task.abort_handle();
        let record = AbortRecord::new();
        let hold = SubtreeHold::with_record(None, record.clone());
        let watch = hold.watch();
        let seen = Arc::new(Mutex::new(None));
        let (state_tx, state_rx) = mpsc::channel::<&'static str>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let joined = Arc::new(Mutex::new(None));
        let dropper = std::thread::spawn({
            let (seen, joined) = (seen.clone(), joined.clone());
            move || {
                test_hook::arm_after_release({
                    let (seen, joined) = (seen.clone(), joined.clone());
                    move || {
                        let racer = std::thread::spawn({
                            let state_tx = state_tx.clone();
                            move || {
                                test_hook::arm_in_chain({
                                    let state_tx = state_tx.clone();
                                    move || {
                                        let _ = state_tx.send("in_chain");
                                        let _ = go_rx.recv_timeout(Duration::from_secs(5));
                                    }
                                });
                                record.abort(&abort);
                                let _ = state_tx.send("done");
                            }
                        });
                        *joined.lock().unwrap() = Some(racer);
                        let _ = state_rx.recv_timeout(Duration::from_secs(5));
                        test_hook::arm_before_release({
                            let seen = seen.clone();
                            move || {
                                *seen.lock().unwrap() =
                                    Some((watch.is_released(), watch.is_gone()));
                                let _ = go_tx.send(());
                            }
                        });
                    }
                });
                drop(hold);
            }
        });
        dropper.join().unwrap();
        joined
            .lock()
            .unwrap()
            .take()
            .expect("racer")
            .join()
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            Some((false, true)),
            "an abort recorded after the record was released held the token while the hold dropped it"
        );
    }

    /// The end of an actor that was owed nothing raises nothing: a subtree
    /// with nothing owed and nothing recorded keeps its deadline exactly.
    #[tokio::test(start_paused = true)]
    async fn the_end_of_an_actor_owed_nothing_raises_nothing() {
        let root = SubtreeHold::new(None);
        let (child, record) = child_of(&root);
        assert_eq!(root.watch().abort_view(), (None, false));
        drop(child);
        assert_eq!(root.watch().abort_view(), (None, false));
        assert_eq!(record.view(), (None, false));
        record.owe();
        assert_eq!(
            root.watch().abort_view(),
            (None, false),
            "an ended actor owes nothing"
        );
    }

    /// A reader of the subtree never sees it with nothing owed and nothing
    /// recorded while an owed actor ends: the end is recorded before the
    /// mark drops. The reader runs on its own thread, so it reads at every
    /// point of the release.
    #[test]
    fn a_reader_never_sees_the_end_of_an_owed_actor_as_neither_owed_nor_recorded() {
        for _ in 0..2_000 {
            let root = SubtreeHold::new(None);
            let (child, record) = child_of(&root);
            record.owe();
            let watch = root.watch();
            let stop = Arc::new(AtomicU64::new(0));
            let started = Arc::new(AtomicU64::new(0));
            let reader = {
                let (stop, started) = (stop.clone(), started.clone());
                std::thread::spawn(move || {
                    let mut gap = false;
                    while stop.load(Ordering::SeqCst) == 0 {
                        let (latest, owed) = watch.abort_view();
                        started.store(1, Ordering::SeqCst);
                        gap |= latest.is_none() && !owed;
                    }
                    gap
                })
            };
            while started.load(Ordering::SeqCst) == 0 {
                std::hint::spin_loop();
            }
            drop(child);
            stop.store(1, Ordering::SeqCst);
            assert!(
                !reader.join().unwrap(),
                "the subtree read as neither owed nor recorded during the release"
            );
            assert!(!root.watch().abort_view().1, "the mark was released");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_abort_stays_recorded_on_the_record_and_up_the_chain() {
        let root = SubtreeHold::new(None);
        let (_child, record) = child_of(&root);
        let first = Instant::now();
        record.record(first);
        record.record(first + ms(100));
        assert_eq!(record.view(), (Some(first), false));
        assert_eq!(root.watch().abort_view(), (Some(first), false));
    }

    /// A party that watches the task end releases the owed mark once: a
    /// second release moves nothing, an owe after the end takes nothing, a
    /// later abort of a live task still writes its instant, and a release
    /// never moves a written instant.
    #[tokio::test(start_paused = true)]
    async fn task_ended_is_idempotent_and_never_moves_a_written_record() {
        let root = SubtreeHold::new(None);
        let (kept, record) = child_of(&root);
        let live = tokio::spawn(std::future::pending::<()>());
        record.owe();
        assert_eq!(root.watch().abort_view(), (None, true));
        record.task_ended();
        let first = root.watch().abort_view();
        assert!(first.0.is_some() && !first.1, "{first:?}");
        tokio::time::advance(ms(50)).await;
        record.task_ended();
        assert_eq!(
            root.watch().abort_view(),
            first,
            "a second release moves nothing"
        );
        record.owe();
        assert_eq!(
            record.view(),
            (None, false),
            "an ended task is owed nothing"
        );
        record.abort(&live.abort_handle());
        let (at, owed) = record.view();
        assert!(at.is_some() && !owed, "{at:?} {owed}");
        record.task_ended();
        assert_eq!(
            record.view(),
            (at, false),
            "a release never moves a written record"
        );
        live.abort();
        drop((kept, root));
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_task_records_nothing() {
        let record = AbortRecord::new();
        let task = tokio::spawn(async {});
        tokio::task::yield_now().await;
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        record.abort(&task.abort_handle());
        assert_eq!(record.view(), (None, false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::runtime::FAR_FUTURE;
    use crate::actor::supervision::ABORT_SETTLE;

    fn child(parent: &Arc<SubtreeHold>, own: Duration, infinity: bool) -> Arc<SubtreeHold> {
        SubtreeHold::new(Some(SubtreeLink {
            parent: parent.token(),
            own,
            infinity,
        }))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn gone_fires_only_after_every_descendant_dropped() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let kid = child(&root, ms(10), false);
        let grandkid = child(&kid, ms(10), false);
        drop(root);
        assert!(!watch.is_gone(), "descendants still hold the token");
        drop(kid);
        assert!(!watch.is_gone(), "the grandchild still holds it");
        drop(grandkid);
        assert!(watch.is_gone());
    }

    #[test]
    fn a_hold_reads_released_with_the_subtree_gone_when_nothing_is_below_it() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let other = root.clone();
        assert!(!watch.is_released());
        drop(root);
        assert!(!watch.is_released(), "a clone of the hold still lives");
        assert!(!watch.is_gone());
        drop(other);
        assert!(watch.is_released());
        assert!(watch.is_gone());
    }

    #[test]
    fn a_hold_with_an_actor_below_it_reads_released_while_the_subtree_is_not_gone() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let kid = child(&root, ms(10), false);
        drop(root);
        assert!(watch.is_released(), "the actor let go of its hold");
        assert!(!watch.is_gone(), "the actor below it still holds the token");
        drop(kid);
        assert!(watch.is_gone());
    }

    /// The release is written after the hold's reference to the token
    /// dropped, so a reader that sees the release and finds the subtree not
    /// gone knows something else holds the token.
    #[test]
    fn the_release_is_written_after_the_hold_let_go_of_the_token() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let seen = Arc::new(Mutex::new(None));
        test_hook::arm_before_release({
            let (watch, seen) = (watch.clone(), seen.clone());
            move || *seen.lock().unwrap() = Some((watch.is_released(), watch.is_gone()))
        });
        drop(root);
        assert_eq!(
            *seen.lock().unwrap(),
            Some((false, true)),
            "when the release is about to be written the token is gone"
        );
        assert!(watch.is_released());
    }

    #[tokio::test]
    async fn released_resolves_at_once_for_a_released_hold_and_wakes_a_waiter_otherwise() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let kid = child(&root, ms(10), false);
        let waiter = tokio::spawn({
            let watch = watch.clone();
            async move { watch.released().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "the hold is not released");
        drop(root);
        tokio::time::timeout(ms(1000), waiter)
            .await
            .expect("the release wakes the waiter")
            .expect("the waiter ends");
        assert!(!watch.is_gone(), "the actor below it still lives");
        tokio::time::timeout(ms(1000), watch.released())
            .await
            .expect("a released hold resolves at once");
        drop(kid);
    }

    #[test]
    fn below_is_the_max_of_children_budgets_and_never_decreases() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        assert_eq!(watch.release_bound(), Some(ABORT_SETTLE));
        let small = child(&root, ms(100), false);
        assert_eq!(watch.release_bound(), Some(ms(100) + ABORT_SETTLE));
        let large = child(&root, ms(500), false);
        assert_eq!(watch.release_bound(), Some(ms(500) + ABORT_SETTLE));
        let _smaller = child(&root, ms(200), false);
        assert_eq!(watch.release_bound(), Some(ms(500) + ABORT_SETTLE));
        // A grandchild's budget adds to its parent's own and reaches the root.
        let _deep = child(&small, ms(900), false);
        assert_eq!(watch.release_bound(), Some(ms(100 + 900) + ABORT_SETTLE));
        // A child that ends never lowers the bound.
        drop(large);
        assert_eq!(watch.release_bound(), Some(ms(100 + 900) + ABORT_SETTLE));
    }

    #[test]
    fn an_unbounded_child_makes_the_release_bound_unbounded() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let _kid = child(&root, Duration::MAX, false);
        assert_eq!(watch.release_bound(), None);
    }

    #[test]
    fn huge_budgets_saturate_to_unbounded_instead_of_wrapping() {
        // Two levels whose sum overflows a u64 of nanoseconds: unbounded,
        // never a small wrapped value.
        let half = Duration::from_nanos(u64::MAX / 2 + 1);
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let kid = child(&root, half, false);
        let _grandkid = child(&kid, half, false);
        assert_eq!(watch.release_bound(), None);

        // A duration just beyond a u64 of nanoseconds reads as unbounded, not
        // as the few nanoseconds a truncating cast would leave of it.
        let beyond = Duration::new(18_446_744_073, 709_551_621);
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let _kid = child(&root, beyond, false);
        assert_eq!(watch.release_bound(), None);

        // A budget of 30 years fits a u64 of nanoseconds: a finite bound, the
        // cap every wait carries.
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let _kid = child(&root, FAR_FUTURE, false);
        assert_eq!(watch.release_bound(), Some(FAR_FUTURE));
    }

    #[tokio::test]
    async fn gone_fires_from_an_aborted_task_s_drop_glue() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let held = root.clone();
        drop(root);
        let task = tokio::spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert!(!watch.is_gone());
        task.abort();
        let _ = task.await;
        tokio::time::timeout(ms(1000), watch.gone())
            .await
            .expect("the aborted task's drop glue fires gone");
    }

    #[tokio::test]
    async fn release_bound_is_unbounded_only_while_an_infinity_supervisor_child_lives_below() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let first = child(&root, ms(50), true);
        let second = child(&root, ms(50), true);
        assert_eq!(watch.release_bound(), None);
        drop(first);
        assert_eq!(watch.release_bound(), None, "one still lives");
        // An aborted child's drop glue lowers the count too.
        let held = second.clone();
        drop(second);
        let task = tokio::spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(watch.release_bound(), None);
        task.abort();
        let _ = task.await;
        tokio::time::timeout(ms(1000), watch.infinity_cleared())
            .await
            .expect("the count reaches zero");
        assert_eq!(watch.release_bound(), Some(ms(50) + ABORT_SETTLE));
    }

    #[tokio::test]
    async fn an_ended_infinity_supervisor_child_with_a_live_descendant_no_longer_counts() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let supervisor = child(&root, ms(50), true);
        let worker = child(&supervisor, ms(50), false);
        assert_eq!(watch.release_bound(), None);
        // The supervisor child's own actor ends while its worker still runs:
        // nothing unbounded is alive any more, though the subtree is not gone.
        drop(supervisor);
        assert_eq!(watch.release_bound(), Some(ms(100) + ABORT_SETTLE));
        assert!(!watch.is_gone());
        drop(worker);
        drop(root);
        assert!(watch.is_gone());
    }

    #[tokio::test(start_paused = true)]
    async fn release_bound_counts_from_the_end_of_the_last_infinity_supervisor_child() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let _worker = child(&root, ms(200), false);
        let first = child(&root, ms(50), true);
        let second = child(&root, ms(50), true);
        let mut wait = SubtreeWait::new(&watch);
        assert_eq!(wait.deadline(), None, "unbounded while they live");
        tokio::time::advance(ms(10_000)).await;
        assert!(!wait.expired());
        drop(first);
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the wait resolves");
        assert_eq!(wait.deadline(), None, "one still lives");
        let ended_at = Instant::now();
        drop(second);
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the wait resolves");
        assert_eq!(
            wait.deadline(),
            Some(ended_at + ms(200) + ABORT_SETTLE),
            "counted from the moment the count reached zero"
        );
        assert!(!wait.expired());
        tokio::time::advance(ms(200) + ABORT_SETTLE).await;
        assert!(wait.expired());
    }

    #[tokio::test(start_paused = true)]
    async fn release_bound_returns_to_unbounded_when_an_infinity_supervisor_child_appears_after_arming(
    ) {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let mut wait = SubtreeWait::new(&watch);
        assert!(
            wait.deadline().is_some(),
            "armed at once: nothing unbounded lives"
        );
        let kid = child(&root, ms(50), true);
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the wait resolves");
        assert_eq!(wait.deadline(), None, "disarmed again");
        tokio::time::advance(ms(10_000)).await;
        assert!(!wait.expired());
        drop(kid);
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the wait resolves");
        assert!(wait.deadline().is_some(), "armed again once it ended");
    }

    #[tokio::test(start_paused = true)]
    async fn a_below_raise_after_arming_extends_the_deadline() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let mut wait = SubtreeWait::new(&watch);
        let armed = wait.deadline().expect("armed");
        tokio::time::advance(ms(40)).await;
        let _kid = child(&root, ms(500), false);
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the wait resolves");
        let extended = wait.deadline().expect("armed");
        assert!(extended > armed);
        assert_eq!(extended, Instant::now() + ms(500) + ABORT_SETTLE);
        // A raise that stays below the armed deadline never shortens it.
        let before = wait.deadline();
        let _small = child(&root, ms(10), false);
        assert_eq!(wait.deadline(), before);
    }

    /// An actor whose value takes a while to drop and says when it is done.
    struct Slow {
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for Slow {
        fn drop(&mut self) {
            std::thread::sleep(ms(150));
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl crate::actor::Actor for Slow {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_subtree_reads_as_ended_only_after_the_actor_value_is_dropped() {
        // Read from this thread, which runs no task of the runtime: the
        // answer cannot wait behind the dropping worker. Both ways an actor
        // ends are covered: it returns, or its task is aborted.
        for abort in [false, true] {
            let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (handle, join, watch) = crate::actor::runtime::spawn_actor(
                crate::types::ActorId::from(format!("slow-drop-{abort}")),
                Slow {
                    dropped: dropped.clone(),
                },
                crate::actor::runtime::ActorConfig::default(),
                None,
                None,
                None,
                true,
                Duration::from_millis(120),
                None,
            )
            .expect("the actor spawns");
            if abort {
                join.abort();
            } else {
                handle
                    .stop(crate::types::StopReason::Graceful)
                    .await
                    .expect("the stop is accepted");
            }
            let started = std::time::Instant::now();
            while !watch.is_gone() {
                assert!(started.elapsed() < Duration::from_secs(5), "never ended");
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(
                dropped.load(Ordering::SeqCst),
                "abort {abort}: the subtree read as ended before the actor value was dropped"
            );
        }
    }

    /// A message whose value takes a while to drop.
    struct SlowMsg;

    impl Drop for SlowMsg {
        fn drop(&mut self) {
            std::thread::sleep(ms(150));
        }
    }

    /// An actor that holds a name and a mailbox.
    struct Named;

    impl crate::actor::Actor for Named {
        type Message = SlowMsg;
        type Response = ();

        async fn handle(
            &mut self,
            _msg: SlowMsg,
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    /// An actor that parks in `on_started` and drops slowly.
    struct Parked {
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for Parked {
        fn drop(&mut self) {
            std::thread::sleep(ms(150));
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl crate::actor::Actor for Parked {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }

        async fn on_started(
            &mut self,
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    /// Polls `watch` from a thread of its own until the subtree reads as
    /// ended, then returns `read()` taken at that moment. The thread runs no
    /// task of the runtime, so its answer cannot wait behind a dropping task.
    fn read_at_the_end<T: Send + 'static>(
        watch: SubtreeWatch,
        read: impl FnOnce() -> T + Send + 'static,
    ) -> std::thread::JoinHandle<T> {
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            while !watch.is_gone() {
                assert!(started.elapsed() < Duration::from_secs(5), "never ended");
                std::thread::sleep(Duration::from_millis(1));
            }
            read()
        })
    }

    /// A task dropped before its first poll drops its parameters in
    /// declaration order, so the hold of the registry guard is the one that
    /// keeps the subtree open past the mailbox: the name is free when the
    /// subtree reads as ended.
    #[tokio::test]
    async fn an_actor_aborted_before_its_first_poll_frees_its_name_before_its_subtree_ends() {
        let system =
            crate::system::ActorSystem::create(format!("unpolled-{}", uuid::Uuid::new_v4()))
                .unwrap();
        let name = format!("unpolled-{}", uuid::Uuid::new_v4());
        let (handle, join, watch) = crate::actor::runtime::spawn_actor(
            crate::types::ActorId::from(name.clone()),
            Named,
            crate::actor::runtime::ActorConfig::default(),
            Some(Arc::from(name.clone())),
            Some(system.clone()),
            None,
            true,
            Duration::from_millis(120),
            None,
        )
        .expect("the actor spawns");
        // The runtime has not polled the task: nothing awaited since the spawn.
        handle.try_notify(SlowMsg).expect("the mailbox has room");
        join.abort();
        let observer = read_at_the_end(watch, {
            let name = name.clone();
            move || system.get::<Named>(&name).is_none()
        });
        let _ = join.await;
        assert!(
            observer.join().unwrap(),
            "the name was still registered when the subtree read as ended"
        );
    }

    /// A task dropped while suspended drops its parameters last to first, so
    /// the hold declared first is released after the actor value.
    #[tokio::test]
    async fn an_actor_aborted_while_it_starts_is_dropped_before_its_subtree_ends() {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, join, watch) = crate::actor::runtime::spawn_actor(
            crate::types::ActorId::from(format!("parked-{}", uuid::Uuid::new_v4())),
            Parked {
                dropped: dropped.clone(),
            },
            crate::actor::runtime::ActorConfig::default(),
            None,
            None,
            None,
            true,
            Duration::from_millis(120),
            None,
        )
        .expect("the actor spawns");
        let status = handle.status_rx();
        // Let the task run to its parking point in `on_started`.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        join.abort();
        let observer = read_at_the_end(watch, {
            let dropped = dropped.clone();
            move || dropped.load(Ordering::SeqCst)
        });
        // The status plane reads as ended, its sender gone, only once the
        // actor value is dropped.
        let status_observer = {
            let dropped = dropped.clone();
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                while status.has_changed().is_ok() {
                    assert!(started.elapsed() < Duration::from_secs(5), "never ended");
                    std::thread::sleep(Duration::from_millis(1));
                }
                dropped.load(Ordering::SeqCst)
            })
        };
        let _ = join.await;
        assert!(
            observer.join().unwrap(),
            "the subtree read as ended before the actor value was dropped"
        );
        assert!(
            status_observer.join().unwrap(),
            "the status plane read as ended before the actor value was dropped"
        );
    }

    /// An actor whose value takes a while to drop, or panics when dropped.
    struct DropProbe {
        dropped: Arc<std::sync::atomic::AtomicBool>,
        panics: bool,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            std::thread::sleep(ms(150));
            self.dropped.store(true, Ordering::SeqCst);
            if self.panics {
                panic!("the actor value panicked when dropped");
            }
        }
    }

    impl crate::actor::Actor for DropProbe {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    fn spawn_drop_probe(
        panics: bool,
    ) -> (
        Arc<std::sync::atomic::AtomicBool>,
        crate::actor::handle::ActorHandle<DropProbe>,
        tokio::task::JoinHandle<crate::types::StopReason>,
    ) {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, join, _watch) = crate::actor::runtime::spawn_actor(
            crate::types::ActorId::from(format!("drop-probe-{}", uuid::Uuid::new_v4())),
            DropProbe {
                dropped: dropped.clone(),
                panics,
            },
            crate::actor::runtime::ActorConfig::default(),
            None,
            None,
            None,
            true,
            Duration::from_millis(120),
            None,
        )
        .expect("the actor spawns");
        (dropped, handle, join)
    }

    /// `Stopped` is published after the actor value, and the code its `Drop`
    /// runs, are gone.
    async fn stopped_means_the_actor_value_is_dropped() {
        let (dropped, handle, join) = spawn_drop_probe(false);
        handle
            .stop(crate::types::StopReason::Graceful)
            .await
            .expect("the stop is accepted");
        handle.wait_stopped().await;
        assert!(
            dropped.load(Ordering::SeqCst),
            "Stopped was observed before the actor value was dropped"
        );
        assert_eq!(
            handle.status(),
            crate::types::ActorStatus::Stopped,
            "the status reads Stopped"
        );
        let reason = join.await.expect("the task ends");
        assert!(
            matches!(reason, crate::types::StopReason::Graceful),
            "{reason:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopped_means_the_actor_value_is_dropped_multi_thread() {
        stopped_means_the_actor_value_is_dropped().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stopped_means_the_actor_value_is_dropped_current_thread() {
        stopped_means_the_actor_value_is_dropped().await;
    }

    /// A panic in the actor value's `Drop` comes after the reason is final:
    /// the actor ends in exactly one reason, the one it was stopped with, on
    /// its plane as in its task's return, `Stopped` is still published once
    /// the value is gone, and with the `tracing` feature the panic is logged
    /// once with the actor's id.
    async fn a_panicking_actor_drop_ends_in_one_reason() {
        #[cfg(feature = "tracing")]
        let logged = crate::actor::supervision::lost_runner::capture::install();
        for request in [
            crate::types::StopReason::Graceful,
            crate::types::StopReason::Failure(crate::error::ActorError::user("earlier")),
            crate::types::StopReason::Kill,
        ] {
            let (dropped, handle, join) = spawn_drop_probe(true);
            let status = handle.status_rx();
            handle
                .stop(request.clone())
                .await
                .expect("the stop is accepted");
            let reason = join.await.expect("the task returns its reason");
            assert!(dropped.load(Ordering::SeqCst));
            assert_eq!(reason.to_string(), request.to_string(), "the reason stands");
            assert_eq!(
                status.borrow().reason.as_ref().map(ToString::to_string),
                Some(request.to_string()),
                "the plane reads the same reason"
            );
            assert_eq!(
                status.borrow().status,
                crate::types::ActorStatus::Stopped,
                "the status reads Stopped"
            );
            #[cfg(feature = "tracing")]
            {
                let token = format!("actor_id={} ", handle.id());
                let lines: Vec<String> = logged
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|line| line.contains(&token) && line.contains("the reason stands"))
                    .cloned()
                    .collect();
                assert_eq!(lines.len(), 1, "one warning under {request:?}: {lines:?}");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_actor_drop_ends_in_one_reason_multi_thread() {
        a_panicking_actor_drop_ends_in_one_reason().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_panicking_actor_drop_ends_in_one_reason_current_thread() {
        a_panicking_actor_drop_ends_in_one_reason().await;
    }
}

/// A wait on a subtree gives its verdict no later than its deadline plus its
/// bound plus the settle time, whatever its members do near that cap: an
/// end or an abort recorded within a settle time of the last one raises the
/// floor no further than the ceiling.
#[cfg(test)]
mod ceiling_tests {
    use super::*;
    use crate::actor::supervision::ABORT_SETTLE;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A child below `parent` with `own` budget, its record linked to its
    /// token.
    fn below(parent: &Arc<SubtreeHold>, own: Duration) -> (Arc<SubtreeHold>, AbortRecord) {
        let record = AbortRecord::new();
        let hold = SubtreeHold::with_record(
            Some(SubtreeLink {
                parent: parent.token(),
                own,
                infinity: false,
            }),
            record.clone(),
        );
        (hold, record)
    }

    /// Runs the wait 1 ms at a time from now, applying `step(offset)` at
    /// each tick before reading it, and returns the offset of the first tick
    /// the wait reads expired.
    async fn first_verdict(wait: &SubtreeWait, mut step: impl FnMut(u64)) -> u64 {
        for offset in 0..5_000 {
            step(offset);
            if wait.expired() {
                return offset;
            }
            tokio::time::advance(ms(1)).await;
        }
        panic!("no verdict in 5 s");
    }

    /// A root P owed an abort, a wait on its subtree, and the clock moved to
    /// one millisecond before the wait's cap. Returns the wait, its
    /// deadline and its bound.
    async fn owed_root_at_the_cap(
        p: &Arc<SubtreeHold>,
        p_rec: &AbortRecord,
    ) -> (SubtreeWait, Instant, Duration) {
        let wait = SubtreeWait::new(&p.watch());
        let start = Instant::now();
        let deadline = wait.deadline().unwrap();
        let bound = deadline - start;
        p_rec.owe();
        tokio::time::advance(bound + bound - ms(1)).await;
        (wait, deadline, bound)
    }

    /// An owed supervisor P obeys its late Kill and ends 1 ms before its
    /// wait's cap. Its worker child Z is handed to its guard (owed, with a
    /// kill grace of 50 ms, below the settle time), never yields, and is
    /// aborted 50 ms after P's end; Z lives on. The verdict lands no later
    /// than the ceiling.
    #[tokio::test(start_paused = true)]
    async fn a_short_grace_abort_below_a_supervisor_that_ended_at_the_cap_keeps_the_ceiling() {
        let p_rec = AbortRecord::new();
        let p = SubtreeHold::with_record(None, p_rec.clone());
        let (z, z_rec) = below(&p, ms(300));
        let (wait, deadline, bound) = owed_root_at_the_cap(&p, &p_rec).await;
        let from = Instant::now();
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        let mut p = Some(p);
        let at = first_verdict(&wait, |t| {
            if t == 0 {
                drop(p.take());
                z_rec.owe();
            }
            if t == 50 {
                z_rec.abort(&handle);
            }
        })
        .await;
        let at = from + ms(at);
        let ceiling = deadline + bound + ABORT_SETTLE;
        drop(z);
        assert!(at <= ceiling, "verdict {:?} past the ceiling", at - ceiling);
        assert!(
            at >= deadline + bound,
            "verdict {:?} before the cap",
            deadline + bound - at
        );
    }

    /// With the default kill grace: P ends 1 ms before the cap; its
    /// supervisor child A (owed by its guard) obeys its Kill 90 ms later and
    /// ends; its worker child Z (owed, kill grace 120 ms) never yields and is
    /// aborted 120 ms after P's end. Each end and abort within a settle time
    /// of the last raises the floor, and the ceiling still holds.
    #[tokio::test(start_paused = true)]
    async fn owed_ends_below_a_supervisor_that_ended_at_the_cap_keep_the_ceiling() {
        let p_rec = AbortRecord::new();
        let p = SubtreeHold::with_record(None, p_rec.clone());
        let (a, a_rec) = below(&p, ms(300));
        let (z, z_rec) = below(&p, ms(300));
        let (wait, deadline, bound) = owed_root_at_the_cap(&p, &p_rec).await;
        let from = Instant::now();
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        let (mut p, mut a) = (Some(p), Some(a));
        let at = first_verdict(&wait, |t| {
            if t == 0 {
                drop(p.take());
                a_rec.owe();
                z_rec.owe();
            }
            if t == 90 {
                drop(a.take());
            }
            if t == 120 {
                z_rec.abort(&handle);
            }
        })
        .await;
        let at = from + ms(at);
        let ceiling = deadline + bound + ABORT_SETTLE;
        drop(z);
        assert!(at <= ceiling, "verdict {:?} past the ceiling", at - ceiling);
        assert!(
            at >= deadline + bound,
            "verdict {:?} before the cap",
            deadline + bound - at
        );
    }

    /// P is aborted (recorded) 1 ms before the cap and ends at the abort;
    /// its worker child Z, with a 50 ms kill grace, is aborted 50 ms later
    /// and lives on. The verdict lands no later than the ceiling.
    #[tokio::test(start_paused = true)]
    async fn a_short_grace_abort_below_a_supervisor_aborted_at_the_cap_keeps_the_ceiling() {
        let p_rec = AbortRecord::new();
        let p = SubtreeHold::with_record(None, p_rec.clone());
        let (z, z_rec) = below(&p, ms(300));
        let (wait, deadline, bound) = owed_root_at_the_cap(&p, &p_rec).await;
        let from = Instant::now();
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        let p_task = tokio::spawn(std::future::pending::<()>());
        let p_handle = p_task.abort_handle();
        let mut p = Some(p);
        let at = first_verdict(&wait, |t| {
            if t == 0 {
                p_rec.abort(&p_handle);
                drop(p.take());
                z_rec.owe();
            }
            if t == 50 {
                z_rec.abort(&handle);
            }
        })
        .await;
        let at = from + ms(at);
        let ceiling = deadline + bound + ABORT_SETTLE;
        drop(z);
        assert!(at <= ceiling, "verdict {:?} past the ceiling", at - ceiling);
        assert!(
            at >= deadline + bound,
            "verdict {:?} before the cap",
            deadline + bound - at
        );
    }
}

/// A one-shot hook run by the calling thread right after the next read of a
/// subtree signal or an abort record, so a test can land a change exactly
/// between a read and the wait that follows it.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::cell::RefCell;

    type Hook = RefCell<Option<Box<dyn FnOnce()>>>;

    thread_local! {
        static HOOK: Hook = const { RefCell::new(None) };
        static AFTER_WRITE: Hook = const { RefCell::new(None) };
        static BEFORE_RELEASE: Hook = const { RefCell::new(None) };
        static IN_CHAIN: Hook = const { RefCell::new(None) };
        static AFTER_RELEASE: Hook = const { RefCell::new(None) };
    }

    pub(crate) fn arm_in_chain(hook: impl FnOnce() + 'static) {
        IN_CHAIN.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn run_in_chain() {
        if let Some(hook) = IN_CHAIN.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    pub(crate) fn arm_after_release(hook: impl FnOnce() + 'static) {
        AFTER_RELEASE.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn run_after_release() {
        if let Some(hook) = AFTER_RELEASE.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    pub(crate) fn arm(hook: impl FnOnce() + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn run() {
        if let Some(hook) = HOOK.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    /// Armed like [`arm`], but run by the next first-abort record right after
    /// it wrote the abort's instant and before it wakes the waits.
    pub(crate) fn arm_after_write(hook: impl FnOnce() + 'static) {
        AFTER_WRITE.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn run_after_write() {
        if let Some(hook) = AFTER_WRITE.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    /// Armed like [`arm`], but run by the next hold's release right before it
    /// writes that the hold is released.
    pub(crate) fn arm_before_release(hook: impl FnOnce() + 'static) {
        BEFORE_RELEASE.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn run_before_release() {
        if let Some(hook) = BEFORE_RELEASE.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }
}

#[cfg(test)]
mod lost_wake_tests {
    use super::*;
    use crate::actor::supervision::ABORT_SETTLE;
    use crate::system::wait_end_or_floor;
    use crate::types::{ActorStatus, StatusTx, StatusValue};
    use std::cell::RefCell;
    use std::future::Future;
    use std::rc::Rc;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll, Wake, Waker};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn child(parent: &Arc<SubtreeHold>, own: Duration, infinity: bool) -> Arc<SubtreeHold> {
        SubtreeHold::new(Some(SubtreeLink {
            parent: parent.token(),
            own,
            infinity,
        }))
    }

    /// A release that lands right after the wait read that the hold was not
    /// released wakes the wait.
    #[tokio::test(start_paused = true)]
    async fn a_release_right_after_the_read_wakes_the_wait() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        test_hook::arm(move || drop(root));
        tokio::time::timeout(ms(1000), watch.released())
            .await
            .expect("the release after the read wakes the wait");
        assert!(watch.is_released());
    }

    /// A wait with no deadline (an unbounded supervisor child lives below)
    /// is woken by a change that lands right after the wait read the signal.
    #[tokio::test(start_paused = true)]
    async fn the_end_of_an_unbounded_child_right_after_the_read_wakes_the_wait() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let kid = child(&root, ms(50), true);
        let mut wait = SubtreeWait::new(&watch);
        assert_eq!(wait.deadline(), None);
        test_hook::arm(move || drop(kid));
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the change after the read wakes the wait");
        assert!(wait.deadline().is_some(), "armed again once it ended");
    }

    #[tokio::test(start_paused = true)]
    async fn a_budget_raise_right_after_the_read_wakes_the_wait() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let _inf = child(&root, ms(50), true);
        let mut wait = SubtreeWait::new(&watch);
        assert_eq!(wait.deadline(), None);
        let kept = Rc::new(RefCell::new(None));
        let kept2 = kept.clone();
        let root2 = root.clone();
        test_hook::arm(move || *kept2.borrow_mut() = Some(child(&root2, ms(500), false)));
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the raise after the read wakes the wait");
        assert!(kept.borrow().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn an_owed_abort_right_after_the_read_wakes_the_wait() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let _inf = child(&root, ms(50), true);
        let record = AbortRecord::new();
        let link = SubtreeLink {
            parent: root.token(),
            own: ms(200),
            infinity: false,
        };
        let _kid = SubtreeHold::with_record(Some(link), record.clone());
        let mut wait = SubtreeWait::new(&watch);
        test_hook::arm(move || record.owe());
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the owed abort after the read wakes the wait");
    }

    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_subtree_right_after_the_read_wakes_the_wait() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let kid = child(&root, ms(50), true);
        let mut wait = SubtreeWait::new(&watch);
        assert_eq!(wait.deadline(), None);
        test_hook::arm(move || {
            drop(kid);
            drop(root);
        });
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the end after the read wakes the wait");
        assert!(wait.gone());
    }

    /// An unbounded child that comes and goes between two polls of a wait
    /// (no raise of the budget, only the count moves 0 -> 1 -> 0) still
    /// counts the bound again from its end.
    #[tokio::test(start_paused = true)]
    async fn a_net_zero_unbounded_child_between_two_polls_rearms_the_deadline() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let mut wait = SubtreeWait::new(&watch);
        let armed = wait.deadline().expect("armed at once");
        tokio::time::advance(ms(40)).await;
        drop(child(&root, Duration::ZERO, true));
        assert_eq!(watch.release_bound(), Some(ABORT_SETTLE));
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the wait resolves");
        assert_eq!(wait.deadline(), Some(armed + ms(40)));
    }

    /// The same change while the wait is parked.
    #[tokio::test(start_paused = true)]
    async fn a_net_zero_unbounded_child_while_parked_rearms_the_deadline() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let mut wait = SubtreeWait::new(&watch);
        let armed = wait.deadline().expect("armed at once");
        let root2 = root.clone();
        let mover = tokio::spawn(async move {
            tokio::time::sleep(ms(40)).await;
            drop(child(&root2, Duration::ZERO, true));
        });
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the wait resolves");
        mover.await.unwrap();
        assert_eq!(wait.deadline(), Some(armed + ms(40)));
    }

    /// An unbounded child that appears while the wait is being built is
    /// seen by its first arming or by its first poll, never by neither.
    #[tokio::test(start_paused = true)]
    async fn an_unbounded_child_appearing_while_the_wait_is_built_disarms_it() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let kept = Rc::new(RefCell::new(None));
        let kept2 = kept.clone();
        let root2 = root.clone();
        test_hook::arm(move || *kept2.borrow_mut() = Some(child(&root2, ms(50), true)));
        let wait = SubtreeWait::new(&watch);
        assert!(
            kept.borrow().is_some(),
            "the child appeared during the build"
        );
        assert_eq!(wait.deadline(), None, "an unbounded child lives below");
    }

    /// The end of the subtree wakes a parked wait whose deadline is far off.
    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_subtree_wakes_a_parked_wait_long_before_its_deadline() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let kid = child(&root, ms(60_000), false);
        let mut wait = SubtreeWait::new(&watch);
        assert!(wait.deadline().is_some());
        let start = Instant::now();
        let ender = tokio::spawn(async move {
            tokio::time::sleep(ms(10)).await;
            drop(kid);
            drop(root);
        });
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the wait resolves");
        ender.await.unwrap();
        assert!(wait.gone());
        assert_eq!(
            Instant::now() - start,
            ms(10),
            "woken by the end, not by the deadline"
        );
    }

    /// A raise of the budget reaches a parked wait before its old deadline
    /// fires, and extends that deadline.
    #[tokio::test(start_paused = true)]
    async fn a_budget_raise_wakes_a_parked_wait_and_extends_its_deadline() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let mut wait = SubtreeWait::new(&watch);
        let armed = wait.deadline().expect("armed at once");
        let start = Instant::now();
        let root2 = root.clone();
        let raiser = tokio::spawn(async move {
            tokio::time::sleep(ms(40)).await;
            child(&root2, ms(500), false)
        });
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the wait resolves");
        let _kid = raiser.await.unwrap();
        assert_eq!(
            Instant::now() - start,
            ms(40),
            "woken by the raise, not by the deadline"
        );
        let extended = wait.deadline().expect("armed");
        assert!(extended > armed);
        assert_eq!(extended, start + ms(40) + ms(500) + ABORT_SETTLE);
        assert!(!wait.expired());
    }

    /// An abort recorded against a live actor of the subtree reaches a
    /// parked wait at once, and the verdict floor follows it.
    #[tokio::test(start_paused = true)]
    async fn an_owed_abort_wakes_a_parked_wait() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let record = AbortRecord::new();
        let link = SubtreeLink {
            parent: root.token(),
            own: ms(200),
            infinity: false,
        };
        let _kid = SubtreeHold::with_record(Some(link), record.clone());
        let mut wait = SubtreeWait::new(&watch);
        let start = Instant::now();
        let rec = record.clone();
        let mover = tokio::spawn(async move {
            tokio::time::sleep(ms(30)).await;
            rec.owe();
        });
        tokio::time::timeout(ms(1000), wait.changed())
            .await
            .expect("the wait resolves");
        mover.await.unwrap();
        assert_eq!(Instant::now() - start, ms(30), "woken by the owed abort");
        assert!(!wait.expired());
    }

    /// The first abort lands right after the wait read the record while an
    /// abort was owed: the owed floor (budget plus cap) is replaced by the
    /// recorded one (the later of the budget and the abort plus the settle
    /// time), and the wait must read the record again at once.
    #[tokio::test(start_paused = true)]
    async fn an_abort_recorded_right_after_the_read_moves_the_floor_at_once() {
        let (_status_tx, mut status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let record = AbortRecord::new();
        let task = tokio::spawn(std::future::pending::<()>());
        record.owe();
        let start = Instant::now();
        let rec = record.clone();
        let handle = task.abort_handle();
        test_hook::arm(move || rec.abort(&handle));
        let budget = ms(1000);
        let cap = ms(3000);
        let ended = wait_end_or_floor(&mut status_rx, Some(&record), budget, cap).await;
        assert!(!ended);
        assert_eq!(
            Instant::now() - start,
            budget.max(ABORT_SETTLE),
            "the verdict follows the recorded floor, not the owed one"
        );
    }

    /// The same, with the abort recorded while the wait sleeps.
    #[tokio::test(start_paused = true)]
    async fn an_abort_recorded_during_the_sleep_moves_the_floor_at_once() {
        let (_status_tx, mut status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let record = AbortRecord::new();
        let task = tokio::spawn(std::future::pending::<()>());
        record.owe();
        let start = Instant::now();
        let budget = ms(1000);
        let cap = ms(3000);
        let rec = record.clone();
        let waiter = tokio::spawn(async move {
            let ended = wait_end_or_floor(&mut status_rx, Some(&rec), budget, cap).await;
            (ended, Instant::now() - start)
        });
        tokio::time::sleep(ms(10)).await;
        record.abort(&task.abort_handle());
        let (ended, took) = waiter.await.unwrap();
        assert!(!ended);
        assert_eq!(took, budget.max(ms(10) + ABORT_SETTLE));
    }

    /// Two waits on one record, as a targeted kill racing a sweep: the first
    /// abort lands after the second wait read the record, and both waits
    /// move to the recorded floor, not only the one that parked first.
    #[tokio::test(start_paused = true)]
    async fn an_abort_recorded_after_a_second_wait_read_moves_both_floors() {
        let (status_tx, mut first_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let mut second_rx = status_tx.subscribe();
        let record = AbortRecord::new();
        let task = tokio::spawn(std::future::pending::<()>());
        record.owe();
        let start = Instant::now();
        let budget = ms(1000);
        let cap = ms(3000);
        let first = wait_end_or_floor(&mut first_rx, Some(&record), budget, cap);
        let second = wait_end_or_floor(&mut second_rx, Some(&record), budget, cap);
        tokio::pin!(first, second);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx)))
                .await
                .is_pending(),
            "the first wait parks on the owed floor"
        );
        let rec = record.clone();
        let handle = task.abort_handle();
        test_hook::arm(move || rec.abort(&handle));
        // The second wait's first poll reads the record, and the abort lands
        // right after that read.
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx)))
                .await
                .is_pending(),
            "the second wait parks on the recorded floor"
        );
        let (a, b) = tokio::time::timeout(ms(10_000), async {
            tokio::join!(
                async { (first.as_mut().await, Instant::now() - start) },
                async { (second.as_mut().await, Instant::now() - start) },
            )
        })
        .await
        .expect("both waits answer");
        assert_eq!(a, (false, budget.max(ABORT_SETTLE)));
        assert_eq!(b, (false, budget.max(ABORT_SETTLE)));
    }

    /// A woken wait must find the abort's instant already written: the wake
    /// is sent only after the record holds the first abort.
    #[tokio::test(start_paused = true)]
    async fn the_first_abort_is_written_before_the_waits_are_woken() {
        struct Flag(AtomicBool);
        impl Wake for Flag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let record = AbortRecord::new();
        let task = tokio::spawn(std::future::pending::<()>());
        record.owe();
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(flag.clone());
        let mut cx = Context::from_waker(&waker);
        let recorded = record.recorded();
        tokio::pin!(recorded);
        recorded.as_mut().enable();
        assert!(recorded.as_mut().poll(&mut cx).is_pending());
        let seen = Rc::new(RefCell::new(None));
        let (seen2, rec, flag2) = (seen.clone(), record.clone(), flag.clone());
        test_hook::arm_after_write(move || {
            *seen2.borrow_mut() = Some((rec.view().0.is_some(), flag2.0.load(Ordering::SeqCst)));
        });
        record.abort(&task.abort_handle());
        assert_eq!(
            *seen.borrow(),
            Some((true, false)),
            "written and not yet woken between the write and the wake"
        );
        assert!(flag.0.load(Ordering::SeqCst), "woken once it is written");
        assert!(recorded.as_mut().poll(&mut cx).is_ready());
    }
}
