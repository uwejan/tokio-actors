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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    pub(crate) fn run_in_chain() {
        if let Some(hook) = IN_CHAIN.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    pub(crate) fn run_after_release() {
        if let Some(hook) = AFTER_RELEASE.with(|h| h.borrow_mut().take()) {
            hook();
        }
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

    pub(crate) fn run_before_release() {
        if let Some(hook) = BEFORE_RELEASE.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }
}
