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
use crate::types::StopReason;

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
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default))]
pub(crate) struct AbortRecord {
    shared: Arc<AbortShared>,
}

#[derive(Debug)]
struct AbortShared {
    linked: OnceLock<Linked>,
    owed: Mutex<Owed>,
    /// The incarnation's status plane (see `StatusCell`).
    status: crate::types::status_cell::StatusCell,
    /// The public name of this run, drawn from the process-wide counter when
    /// the block is made. Every spawn path makes a block per run, so a
    /// supervised restart gets a new one.
    incarnation: crate::types::Incarnation,
    /// How many monitors other actors hold on this run that have not fired
    /// or been revoked.
    monitored_by: std::sync::atomic::AtomicU32,
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
    /// under `system`'s lock and read without it, and the flags of the
    /// signal machinery (`SYSTEM_SIGNALS` and the bits after it): they live
    /// in this one word so that the order of two operations on them is the
    /// order of the word's updates, with no lock taken by an incarnation
    /// that uses none of them.
    system_state: std::sync::atomic::AtomicU8,
    /// Everything that only an incarnation using links, exit trapping, a
    /// process group or a signal map keeps (see `SignalInner`), made by the
    /// first use: an incarnation that uses none holds the empty cell.
    signal_cell: OnceLock<Box<Mutex<SignalInner>>>,
    // The `sys` bundle's bits, the supervised-children count and the
    // guard-ended flag of the incarnation live in the tail padding of `lane`
    // (see `LaneCell`), so the small atomics of this block fit one word.
    /// The telemetry state of the system the incarnation was spawned in, set
    /// once before the incarnation is registered. It holds the system weakly,
    /// so a handle never keeps a system alive; it is the incarnation's way to
    /// its system and, past two relaxed loads, to its events.
    mode: OnceLock<Arc<crate::telemetry::Mode>>,
}

/// What an incarnation that uses the signal machinery keeps, under the lock
/// of its cell.
#[derive(Debug, Default)]
struct SignalInner {
    /// The signal inbox and the pair table of the links this incarnation
    /// owns.
    block: crate::actor::link::SignalBlock,
    /// How many process-group memberships this run holds: each join not yet
    /// left counts one. Read by the registry guard at the run's end to learn
    /// whether any scope holds the member.
    pg_joined: usize,
    /// The first panic of a signal map function (a monitor's, a group
    /// monitor's) that failed the actor, kept where the actor's stop can read
    /// it however late the panic landed. See `keep_map_panic`.
    map_panic: Option<String>,
    /// The lengths of the signal tables the actor itself holds, indexed by
    /// [`Coupling`]. The actor writes a length whenever its table changes, so
    /// a snapshot shows them without asking the actor.
    coupling: [u32; 3],
    /// The exit signals sent with `exit` that this run has queued and whose
    /// delivery has not yet taken a slot of its mailbox: counted by the push,
    /// given back by the [`ExitSlot`](crate::actor::link::ExitSlot) each one
    /// carries from the drain on.
    exits: usize,
    /// The bound of `exits`: the mailbox capacity the actor recorded when it
    /// called `trap_exit`; 0 until then, read as [`DEFAULT_EXIT_CAP`].
    exit_cap: usize,
}

/// The bound of the exit signals waiting for an actor that traps exits
/// without having called `trap_exit` (a supervisor traps from its spawn):
/// the default mailbox capacity.
const DEFAULT_EXIT_CAP: usize = 64;

/// One of the signal tables an actor holds itself, whose length a snapshot
/// shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Coupling {
    /// The monitors the actor holds.
    Monitor = 0,
    /// The links the actor holds.
    Link = 1,
    /// The group and scope monitors the actor holds.
    GroupMonitor = 2,
}

impl Drop for AbortShared {
    /// The last holder of the record lets go of the incarnation's identity,
    /// if the system recorded one: the identity lives exactly as long as the
    /// record that names it, so a request made through a handle after the
    /// actor's end still finds the incarnation it is made on.
    fn drop(&mut self) {
        if let Some(mode) = self.mode.get() {
            mode.release(std::ptr::from_ref::<Self>(self) as usize);
        }
    }
}

impl AbortShared {
    fn with_mode(
        mode: OnceLock<Arc<crate::telemetry::Mode>>,
        incarnation: crate::types::Incarnation,
    ) -> Self {
        Self {
            linked: OnceLock::new(),
            owed: Mutex::new(Owed::default()),
            status: crate::types::status_cell::StatusCell::new(),
            incarnation,
            monitored_by: std::sync::atomic::AtomicU32::new(0),
            signals: tokio::sync::Notify::new(),
            reader: crate::types::WakeSlot::default(),
            lane: crate::types::LaneCell::default(),
            system: Mutex::new(SystemSlot::default()),
            system_state: std::sync::atomic::AtomicU8::new(0),
            signal_cell: OnceLock::new(),
            mode,
        }
    }
}

/// A block a unit test builds by hand, with a number of its own. No
/// production path builds one: a spawn draws the number first and passes it
/// to [`AbortRecord::in_system`].
#[cfg(test)]
impl Default for AbortShared {
    fn default() -> Self {
        Self::with_mode(OnceLock::new(), crate::types::Incarnation::mint())
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
    /// What trapping exit signals adds for this actor; `None` for an actor
    /// that adds nothing by it (any actor but a worker below a supervisor
    /// whose `Shutdown` has a window or is `Infinity`).
    trap: Option<Box<TrapSpec>>,
}

/// The plane holds a request outside its channel.
const SYSTEM_HELD: u8 = 1;
/// The plane's channel exists.
const SYSTEM_OPEN: u8 = 2;
/// The signal inbox holds items the run loop has not taken.
const SYSTEM_SIGNALS: u8 = 4;
/// The signal inbox is sealed: its owner took the last items it will take,
/// so a later push is refused.
const SYSTEM_SEALED: u8 = 8;
/// The actor traps exits. Written by `set_trap` and read by the senders of
/// exit signals.
const SYSTEM_TRAP: u8 = 16;
/// A push or a pair was begun: the signal cell may hold an inbox or a pair
/// table. Set, under the cell's lock, by every push and pair before it reads
/// `SYSTEM_SEALED`, so a seal that reads this bit clear in the word it
/// updated knows that every later push finds the seal.
const SYSTEM_BLOCK: u8 = 32;
/// A join of a process group was begun: the cell may hold a membership.
const SYSTEM_PG_USED: u8 = 64;
/// The run ended: no join is accepted from here on.
const SYSTEM_PG_ENDED: u8 = 128;
/// What the end of a run sets: the inbox sealed and the groups closed. The
/// first end site sets both in one update and takes what the inbox holds, so
/// a word that shows both bits shows that an earlier update did.
const SYSTEM_ENDED: u8 = SYSTEM_SEALED | SYSTEM_PG_ENDED;
/// The capacity of a plane's channel: in steady state a plane queues this
/// many requests, as many as the system channel of an actor without a held
/// slot does. The request held before the channel opened is taken by the run
/// loop with the channel's receiver, so a plane whose run loop has not yet
/// polled it holds that one request beside a full channel.
const SYSTEM_CAPACITY: usize = 64;

type StatusReply = tokio::sync::oneshot::Sender<Box<crate::types::ActorStatusInfo>>;

/// The actor is suspended: its loop takes no message.
#[allow(dead_code)]
pub(crate) const SYS_SUSPENDED: u8 = 1;
/// The actor was spawned with state access.
pub(crate) const SYS_ALLOW_STATE: u8 = 2;
/// The actor is sensitive.
pub(crate) const SYS_SENSITIVE: u8 = 4;
/// The handler that runs stopped its own actor with `ActorContext::stop`, so
/// the reply it returns is held to the seal of the stop lane. Written and read
/// only by the actor's own task; it shares the word with the `sys` bits so
/// the flag costs no byte of the context or the block.
const SELF_STOP: u8 = 8;

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
        let seen = shared.system_state.load(Ordering::Acquire);
        if seen & SYSTEM_SIGNALS != 0 {
            return std::task::Poll::Ready(Some(crate::types::SystemMessage::Signals));
        }
        if seen & SYSTEM_HELD != 0 {
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
        if state & (SYSTEM_HELD | SYSTEM_SIGNALS) != 0 {
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
        contain_waker_panic("status request", Some(self.owner.as_who()), move || {
            drop(request)
        });
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
            if let Some(panic) = crate::actor::runtime::end_queue_late(
                rx,
                &self.system,
                &crate::actor::runtime::Owner::of(&self.owner, &self.record),
                Some(&self.record),
            ) {
                crate::actor::panic::log_waker_panic(
                    "status queue",
                    &panic,
                    Some(self.owner.as_who()),
                );
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
    /// Boxed, so a record that is owed nothing (nearly every one) holds one
    /// pointer for it.
    mark: Option<Box<OwedMark>>,
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
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The public name of the run this block belongs to.
    pub(crate) fn incarnation(&self) -> crate::types::Incarnation {
        self.shared.incarnation
    }

    /// How many monitors other actors hold on this run.
    pub(crate) fn monitored_by(&self) -> usize {
        self.shared.monitored_by.load(Ordering::Acquire) as usize
    }

    /// Counts one more monitor held on this run.
    pub(crate) fn add_monitored_by(&self) {
        self.shared.monitored_by.fetch_add(1, Ordering::AcqRel);
    }

    /// Counts one monitor fewer, stopping at zero.
    pub(crate) fn sub_monitored_by(&self) {
        let count = &self.shared.monitored_by;
        let mut seen = count.load(Ordering::Acquire);
        while seen != 0 {
            match count.compare_exchange_weak(seen, seen - 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => seen = now,
            }
        }
    }

    /// The cell of the signal machinery, made by the first use.
    fn cell(&self) -> &Mutex<SignalInner> {
        self.shared.signal_cell.get_or_init(Box::default)
    }

    fn lock_cell(&self) -> std::sync::MutexGuard<'_, SignalInner> {
        self.cell().lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The cell, when some use made it.
    fn existing_cell(&self) -> Option<&Mutex<SignalInner>> {
        self.shared.signal_cell.get().map(|cell| &**cell)
    }

    fn state_bits(&self) -> u8 {
        self.shared.system_state.load(Ordering::SeqCst)
    }

    /// Keeps the panic of a signal map function that fails the actor, the
    /// first one only. Every site that raises such a `Failure` on the stop
    /// lane calls this first, because the raise reaches no reader once the
    /// actor's message loop has ended; the stop reads the panic once, after
    /// the last moment a map function can run (`map_panic`).
    pub(crate) fn keep_map_panic(&self, panic: String) {
        let mut inner = self.lock_cell();
        if inner.map_panic.is_none() {
            inner.map_panic = Some(panic);
        }
    }

    /// Records the length of one of the actor's own signal tables. Called on
    /// the actor's own task when the table changes; the first call with a
    /// length above zero makes the signal cell.
    pub(crate) fn set_coupling(&self, kind: Coupling, len: u32) {
        if len == 0 && self.existing_cell().is_none() {
            // Nothing was recorded, so zero already reads; no cell is made.
            return;
        }
        self.lock_cell().coupling[kind as usize] = len;
    }

    /// The lengths last recorded, in the order monitors, links, group
    /// monitors; zeros for an incarnation that never used a signal. Makes no
    /// cell.
    pub(crate) fn coupling(&self) -> [u32; 3] {
        self.existing_cell().map_or([0; 3], |cell| {
            cell.lock().unwrap_or_else(|p| p.into_inner()).coupling
        })
    }

    /// Records the bound of the exit signals that may wait for this actor:
    /// its mailbox capacity. Called by `trap_exit` on the actor's own task;
    /// the first call makes the signal cell.
    pub(crate) fn set_exit_cap(&self, cap: usize) {
        self.lock_cell().exit_cap = cap;
    }

    /// Gives back one exit signal counted by the push, once its delivery took
    /// a mailbox slot, ended without one, or was dropped. Takes the cell's
    /// lock only; wakes nothing and runs no user code.
    pub(crate) fn release_exit(&self) {
        if let Some(cell) = self.existing_cell() {
            let mut inner = cell.lock().unwrap_or_else(|p| p.into_inner());
            inner.exits = inner.exits.saturating_sub(1);
        }
    }

    /// The first panic kept by `keep_map_panic`, if any. An incarnation that
    /// never kept one has no cell to look in.
    pub(crate) fn map_panic(&self) -> Option<String> {
        let cell = self.existing_cell()?;
        cell.lock()
            .unwrap_or_else(|p| p.into_inner())
            .map_panic
            .clone()
    }

    /// Whether the actor traps exits.
    pub(crate) fn trap(&self) -> bool {
        self.state_bits() & SYSTEM_TRAP != 0
    }

    /// Sets or clears the trap flag, ordered against the senders of exit
    /// signals: a sender reads the flag under the cell's lock, after it
    /// marked the cell in use, and this write then waits out a sender that
    /// is inside the lock. A sender therefore sees the flag either before or
    /// after this write, and none pushes once this call has returned.
    /// Allocates nothing.
    ///
    /// Turning the flag on first raises the subtree budget above the actor by
    /// its trapping window, or, for a worker under `Shutdown::Infinity`, counts
    /// it among the unbounded stops until the actor ends, so no wait that
    /// reads the flag can have a bound that does not cover the orderly stop
    /// the flag asks for.
    pub(crate) fn set_trap(&self, on: bool) {
        if on {
            if let Some(linked) = self.shared.linked.get() {
                if let (Some(spec), Some(token)) = (linked.trap.as_deref(), linked.token.upgrade())
                {
                    token.raise_for_trap(spec);
                }
            }
        }
        let before = if on {
            self.shared
                .system_state
                .fetch_or(SYSTEM_TRAP, Ordering::SeqCst)
        } else {
            self.shared
                .system_state
                .fetch_and(!SYSTEM_TRAP, Ordering::SeqCst)
        };
        if before & SYSTEM_BLOCK != 0 {
            if let Some(cell) = self.existing_cell() {
                drop(cell.lock().unwrap_or_else(|p| p.into_inner()));
            }
        }
    }

    /// How many pairs the table of this block holds.
    #[cfg(test)]
    pub(crate) fn pairs_for_test(&self) -> usize {
        self.existing_cell().map_or(0, |cell| {
            cell.lock()
                .unwrap_or_else(|p| p.into_inner())
                .block
                .pairs
                .len()
        })
    }

    /// Whether items wait in the signal inbox.
    pub(crate) fn signals_pending(&self) -> bool {
        self.shared.system_state.load(Ordering::Acquire) & SYSTEM_SIGNALS != 0
    }

    /// The state of the link between this incarnation and `peer`, found in
    /// the block of the lower incarnation of the two, where both ends look.
    /// True when this call made it. A sealed block cannot keep a table, so
    /// it hands out a state of its own; its owner is ending and holds no
    /// entry for the pair.
    pub(crate) fn pair_state(
        &self,
        own: crate::types::Incarnation,
        peer: &AbortRecord,
    ) -> (Arc<crate::actor::link::LinkState>, bool) {
        let peer_incarnation = peer.incarnation();
        if own < peer_incarnation {
            self.table_state(peer_incarnation)
        } else {
            peer.table_state(own)
        }
    }

    fn table_state(
        &self,
        higher: crate::types::Incarnation,
    ) -> (Arc<crate::actor::link::LinkState>, bool) {
        let mut inner = self.lock_cell();
        let before = self
            .shared
            .system_state
            .fetch_or(SYSTEM_BLOCK, Ordering::SeqCst);
        if before & SYSTEM_SEALED != 0 {
            return (Arc::new(crate::actor::link::LinkState::new()), true);
        }
        match inner.block.pairs.entry(higher) {
            std::collections::btree_map::Entry::Occupied(found) => (found.get().clone(), false),
            std::collections::btree_map::Entry::Vacant(room) => {
                let state = Arc::new(crate::actor::link::LinkState::new());
                room.insert(state.clone());
                (state, true)
            }
        }
    }

    /// Forgets the pair between `own` and `peer` when the table still holds
    /// `state` for it; called on the record of the lower incarnation. A state
    /// of an earlier link between the same two runs is never matched by the
    /// pair's key alone.
    pub(crate) fn remove_pair(
        &self,
        own: crate::types::Incarnation,
        peer: crate::types::Incarnation,
        state: &Arc<crate::actor::link::LinkState>,
    ) {
        let Some(cell) = self.existing_cell() else {
            return;
        };
        let removed = {
            let mut inner = cell.lock().unwrap_or_else(|p| p.into_inner());
            let key = own.max(peer);
            match inner.block.pairs.get(&key) {
                Some(held) if Arc::ptr_eq(held, state) => inner.block.pairs.remove(&key),
                _ => None,
            }
        };
        drop(removed);
    }

    /// Queues `item` in the inbox and wakes the run loop, once the lock is
    /// released. The run loop's waker is the actor's own task's, and the
    /// slot contains a panic of its wake.
    fn push(&self, item: crate::actor::link::InboxItem) -> crate::actor::link::PushResult {
        {
            let mut inner = self.lock_cell();
            let before = self
                .shared
                .system_state
                .fetch_or(SYSTEM_BLOCK, Ordering::SeqCst);
            if before & SYSTEM_SEALED != 0 {
                drop(inner);
                drop(item);
                return crate::actor::link::PushResult::Sealed;
            }
            inner.block.inbox.push_back(item);
            self.shared
                .system_state
                .fetch_or(SYSTEM_SIGNALS, Ordering::Release);
        }
        self.shared.reader.wake();
        crate::actor::link::PushResult::Pushed
    }

    /// Queues a link for this incarnation to install. `Sealed`: its owner is
    /// ending, and the caller's entry reads this incarnation's end itself.
    pub(crate) fn push_link(
        &self,
        item: crate::actor::link::InboxItem,
    ) -> crate::actor::link::PushResult {
        self.push(item)
    }

    /// Queues the cancel of a link.
    pub(crate) fn push_unlink(
        &self,
        from: crate::types::Incarnation,
        state: Arc<crate::actor::link::LinkState>,
    ) -> crate::actor::link::PushResult {
        self.push(crate::actor::link::InboxItem::Unlink { from, state })
    }

    /// Queues an exit signal when the actor traps exits, reading the flag
    /// under the cell's lock; pushes nothing otherwise. A read of the word
    /// that already shows a seal or no trap answers without the lock, as a
    /// push that came before the write or the seal would have. A trapping
    /// actor for which as many exit signals wait as its bound allows (see
    /// `exit_cap`) accepts none beyond them: `Full`, and nothing is queued.
    pub(crate) fn push_exit_or_trap(
        &self,
        from: crate::types::PeerRef,
        reason: crate::types::StopReason,
    ) -> crate::actor::link::ExitPush {
        self.push_exit_or_trap_with(from, reason, false)
    }

    /// As [`push_exit_or_trap`](Self::push_exit_or_trap); `priority` asks that
    /// a trapping receiver with a priority lane be given the exit through it.
    pub(crate) fn push_exit_or_trap_with(
        &self,
        from: crate::types::PeerRef,
        reason: crate::types::StopReason,
        priority: bool,
    ) -> crate::actor::link::ExitPush {
        let seen = self.state_bits();
        if seen & SYSTEM_SEALED != 0 {
            return crate::actor::link::ExitPush::Sealed;
        }
        if seen & SYSTEM_TRAP == 0 {
            return crate::actor::link::ExitPush::NotTrapping;
        }
        {
            let mut inner = self.lock_cell();
            let before = self
                .shared
                .system_state
                .fetch_or(SYSTEM_BLOCK, Ordering::SeqCst);
            if before & SYSTEM_SEALED != 0 {
                return crate::actor::link::ExitPush::Sealed;
            }
            if before & SYSTEM_TRAP == 0 {
                return crate::actor::link::ExitPush::NotTrapping;
            }
            let cap = match inner.exit_cap {
                0 => DEFAULT_EXIT_CAP,
                cap => cap,
            };
            if inner.exits >= cap {
                return crate::actor::link::ExitPush::Full;
            }
            inner.exits += 1;
            inner
                .block
                .inbox
                .push_back(crate::actor::link::InboxItem::Exit {
                    from,
                    reason,
                    priority,
                });
            self.shared
                .system_state
                .fetch_or(SYSTEM_SIGNALS, Ordering::Release);
        }
        self.shared.reader.wake();
        crate::actor::link::ExitPush::Pushed
    }

    /// Takes everything queued in the inbox, in order, and clears the
    /// pending mark.
    pub(crate) fn take_signals(&self) -> Vec<crate::actor::link::InboxItem> {
        if self.state_bits() & SYSTEM_BLOCK == 0 {
            return Vec::new();
        }
        let mut inner = self.lock_cell();
        self.shared
            .system_state
            .fetch_and(!SYSTEM_SIGNALS, Ordering::Release);
        std::mem::take(&mut inner.block.inbox).into()
    }

    /// Marks the word with `bits` (which include `SYSTEM_SEALED`) in one
    /// update, then takes what the cell holds when the word showed a push or
    /// a pair: later pushes and pairs are refused, the pair table is dropped
    /// outside the lock, and the inbox is returned. A word that showed none
    /// needs no lock: every later push marks the word before it reads the
    /// seal.
    fn seal_with(&self, bits: u8) -> Vec<crate::actor::link::InboxItem> {
        let before = self.shared.system_state.fetch_or(bits, Ordering::SeqCst);
        if before & SYSTEM_BLOCK == 0 {
            return Vec::new();
        }
        self.take_after_seal()
    }

    /// Marks the run ended for the inbox and the groups in one update and
    /// returns the word the update read. What a push or a pair left is taken
    /// and dropped, outside the lock; a word that showed none builds nothing.
    fn mark_ended(&self) -> u8 {
        let before = self
            .shared
            .system_state
            .fetch_or(SYSTEM_ENDED, Ordering::SeqCst);
        if before & SYSTEM_BLOCK != 0 {
            drop(self.take_after_seal());
        }
        before
    }

    /// What the cell holds after a seal that found the word showing a push or
    /// a pair: the pair table is dropped outside the lock and the inbox is
    /// returned.
    #[cold]
    #[inline(never)]
    fn take_after_seal(&self) -> Vec<crate::actor::link::InboxItem> {
        let Some(cell) = self.existing_cell() else {
            return Vec::new();
        };
        let (inbox, pairs) = {
            let mut inner = cell.lock().unwrap_or_else(|p| p.into_inner());
            self.shared
                .system_state
                .fetch_and(!SYSTEM_SIGNALS, Ordering::Release);
            (
                std::mem::take(&mut inner.block.inbox),
                std::mem::take(&mut inner.block.pairs),
            )
        };
        drop(pairs);
        inbox.into()
    }

    /// As [`take_signals`](Self::take_signals), and seals the block: later
    /// pushes are refused and the pair table is dropped.
    pub(crate) fn take_and_seal_signals(&self) -> Vec<crate::actor::link::InboxItem> {
        self.seal_with(SYSTEM_SEALED)
    }

    /// Seals the block and drops what it holds, each item's drop outside the
    /// lock. Idempotent. The actor's context calls it as it drops, so no join
    /// of a process group is accepted from here on either. A word that shows
    /// both end bits was updated by an earlier call (the registry guard's),
    /// which took what the inbox held, so this one only loads. A word with no
    /// cell proves nothing: a push can make the cell after a read of the
    /// word, and only an update of the word orders it against the seal.
    pub(crate) fn seal_signals(&self) {
        if self.state_bits() & SYSTEM_ENDED == SYSTEM_ENDED {
            return;
        }
        self.mark_ended();
    }

    /// Whether some use made the signal cell: a push, a pair, a kept map
    /// panic, a recorded coupling count or an exit bound.
    #[inline]
    pub(crate) fn has_signal_cell(&self) -> bool {
        self.existing_cell().is_some()
    }

    /// Whether a push or a pair was ever begun on this block: the signal cell
    /// may hold an inbox or a pair table.
    #[inline]
    pub(crate) fn has_signal_state(&self) -> bool {
        self.state_bits() & SYSTEM_BLOCK != 0
    }

    /// The reason a pending `Kill` on the stop lane was raised on behalf of,
    /// if an exit signal raised it: the reason the actor ends with in place
    /// of `Kill`. Read by exactly three sites: the seal of the run loop, the
    /// abort guard's write and the status receiver's read of a plane that
    /// closed without a reason.
    pub(crate) fn kill_cause(&self) -> Option<StopReason> {
        self.shared.lane.kill_cause()
    }

    /// How many process-group memberships this run holds.
    pub(crate) fn pg_joined(&self) -> usize {
        if self.state_bits() & SYSTEM_PG_USED == 0 {
            return 0;
        }
        self.lock_cell().pg_joined
    }

    /// A join begins: counts one membership, unless the run has ended, in
    /// which case it counts nothing and returns `false`. A join and the end
    /// both update the one word, so of the two, at least one sees the other.
    pub(crate) fn pg_join_begin(&self) -> bool {
        let mut inner = self.lock_cell();
        let before = self
            .shared
            .system_state
            .fetch_or(SYSTEM_PG_USED, Ordering::SeqCst);
        if before & SYSTEM_PG_ENDED != 0 {
            return false;
        }
        inner.pg_joined += 1;
        true
    }

    /// One membership fewer, stopping at zero: a leave, or a join that did
    /// not complete.
    pub(crate) fn pg_join_end(&self) {
        if self.state_bits() & SYSTEM_PG_USED == 0 {
            return;
        }
        let mut inner = self.lock_cell();
        inner.pg_joined = inner.pg_joined.saturating_sub(1);
    }

    /// The run ends: no join is accepted from here on, and the inbox is
    /// sealed in the same update (what it holds is dropped outside the lock,
    /// and holds no user code). Returns how many memberships were counted,
    /// read after the mark. A run whose context sealed the block already
    /// takes no further update of the word; a run that never joined pays this
    /// one update, which the context's seal then finds done.
    pub(crate) fn pg_mark_ended(&self) -> usize {
        let seen = self.state_bits();
        let seen = if seen & SYSTEM_ENDED == SYSTEM_ENDED {
            seen
        } else {
            self.mark_ended()
        };
        if seen & SYSTEM_PG_USED == 0 {
            return 0;
        }
        self.lock_cell().pg_joined
    }

    /// A record of the incarnation `incarnation` (a number the spawn drew),
    /// spawned in the system whose telemetry state is `mode`. The pointer is
    /// in place before the record is shared, so recording it costs no atomic
    /// step.
    pub(crate) fn in_system(
        mode: Arc<crate::telemetry::Mode>,
        incarnation: crate::types::Incarnation,
    ) -> Self {
        Self {
            shared: Arc::new(AbortShared::with_mode(OnceLock::from(mode), incarnation)),
        }
    }

    /// Records the system a hand-built record belongs to. Set once: a second
    /// call changes nothing.
    #[cfg(test)]
    pub(crate) fn set_system(&self, system: &Arc<crate::system::ActorSystem>) {
        let _ = self.shared.mode.set(system.telemetry_mode().clone());
    }

    /// The telemetry state of the system the incarnation was spawned in, or
    /// `None` for a record no spawn made.
    #[inline]
    pub(crate) fn mode(&self) -> Option<&Arc<crate::telemetry::Mode>> {
        self.shared.mode.get()
    }

    /// The system the incarnation was spawned in, if it is still alive.
    pub(crate) fn system(&self) -> Option<Arc<crate::system::ActorSystem>> {
        self.shared.mode.get().and_then(|mode| mode.system())
    }

    /// The identity of the system the incarnation was spawned in: the address
    /// of its telemetry state, which the record keeps from being reused, so
    /// it is unique to one system for the life of the process; `0` for a
    /// record no spawn made.
    pub(crate) fn system_identity(&self) -> u64 {
        self.shared
            .mode
            .get()
            .map_or(0, |mode| Arc::as_ptr(mode) as usize as u64)
    }

    /// Whether the incarnation was spawned in the system whose telemetry
    /// state `system` points to. The weak reference keeps the state's
    /// allocation, so an address it holds is never reused by another system;
    /// a record no spawn made is in no system.
    pub(crate) fn spawned_in(&self, system: &Weak<crate::telemetry::Mode>) -> bool {
        self.shared
            .mode
            .get()
            .is_some_and(|mode| std::ptr::eq(Arc::as_ptr(mode), system.as_ptr()))
    }

    /// The `sys` bundle's bits of the incarnation.
    #[allow(dead_code)]
    pub(crate) fn sys_bits(&self) -> u8 {
        self.shared
            .lane
            .sys_bits
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Sets the bits in `bits`, leaving the others.
    pub(crate) fn set_sys_bits(&self, bits: u8) {
        self.shared
            .lane
            .sys_bits
            .fetch_or(bits, std::sync::atomic::Ordering::Relaxed);
    }

    /// Marks that the running handler stopped its own actor (see
    /// [`SELF_STOP`]).
    pub(crate) fn mark_self_stop(&self) {
        self.shared
            .lane
            .sys_bits
            .fetch_or(SELF_STOP, Ordering::Relaxed);
    }

    /// Whether the handler that just returned stopped its own actor; leaves
    /// the mark.
    pub(crate) fn self_stop_set(&self) -> bool {
        self.shared.lane.sys_bits.load(Ordering::Relaxed) & SELF_STOP != 0
    }

    /// Whether the handler that just returned stopped its own actor; reads
    /// the mark once and clears it. An unmarked word is only read.
    pub(crate) fn take_self_stop(&self) -> bool {
        if !self.self_stop_set() {
            return false;
        }
        self.shared
            .lane
            .sys_bits
            .fetch_and(!SELF_STOP, Ordering::Relaxed);
        true
    }

    /// Forgets the mark, before a request is handled.
    pub(crate) fn clear_self_stop(&self) {
        let _ = self.take_self_stop();
    }

    /// Sets or clears one bit.
    #[allow(dead_code)]
    pub(crate) fn set_sys_bit(&self, bit: u8, on: bool) {
        use std::sync::atomic::Ordering;
        if on {
            self.shared.lane.sys_bits.fetch_or(bit, Ordering::Relaxed);
        } else {
            self.shared.lane.sys_bits.fetch_and(!bit, Ordering::Relaxed);
        }
    }

    /// Records how many children the incarnation supervises. Written only by
    /// the incarnation's own supervision state.
    pub(crate) fn set_children(&self, count: usize) {
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        self.shared
            .lane
            .children
            .store(count, std::sync::atomic::Ordering::Relaxed);
    }

    /// How many children the incarnation supervises, as it last recorded.
    pub(crate) fn children(&self) -> usize {
        self.shared
            .lane
            .children
            .load(std::sync::atomic::Ordering::Relaxed) as usize
    }

    /// The address of the shared record, which names the incarnation in the
    /// system's telemetry table for as long as the record lives.
    #[inline]
    pub(crate) fn key(&self) -> usize {
        Arc::as_ptr(&self.shared) as usize
    }

    /// The address of the incarnation's subtree token, once the actor's hold
    /// has linked it. An address only: it is compared, never read through.
    pub(crate) fn token_addr(&self) -> Option<usize> {
        self.shared
            .linked
            .get()
            .map(|linked| linked.token.as_ptr() as usize)
    }

    /// The address of the token of the supervisor above the incarnation, when
    /// the incarnation is linked below one and its token is alive. An
    /// address only, which the supervisor's own record answers to through
    /// [`token_addr`](Self::token_addr).
    pub(crate) fn parent_token_addr(&self) -> Option<usize> {
        let token = self.shared.linked.get()?.token.upgrade()?;
        token
            .parent
            .as_ref()
            .map(|parent| Arc::as_ptr(parent) as usize)
    }

    /// Records that the incarnation has ended (see `ended`), ahead of the
    /// supervisor's book of owed deaths.
    pub(crate) fn mark_guard_ended(&self) {
        self.shared
            .lane
            .ended
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether [`mark_guard_ended`](Self::mark_guard_ended) ran.
    pub(crate) fn has_ended(&self) -> bool {
        self.shared
            .lane
            .ended
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether the incarnation's status plane reads `Stopped`.
    pub(crate) fn has_stopped(&self) -> bool {
        self.shared.status.has_stopped()
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

    /// Whether the calling task is the one that is polling the incarnation,
    /// so a request it sends to it could never be answered. A thread outside
    /// any task, another task, an ended incarnation and a later incarnation
    /// are never it (see `task_key`).
    #[inline]
    pub(crate) fn is_self_call(&self) -> bool {
        crate::actor::task_key::is_self_call(self.shared.incarnation)
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
        let permit = crate::actor::panic::reserve_held("status queue", None, &tx, |_| {})
            .await
            .ok_or(())?;
        permit
            .into_inner()
            .send(crate::types::SystemMessage::GetStatus(request));
        self.shared.reader.wake();
        Ok(())
    }

    /// Queues a request of a `SupervisorRef` on the incarnation's system
    /// plane, waiting for room when the plane is full, then wakes the run
    /// loop with no await between the queueing and the wake. `Err` once the
    /// run loop closed the plane.
    ///
    /// The request always goes through the plane's channel, which is made
    /// here when no status request made it yet, so requests of one sender
    /// reach the run loop in the order they were queued, and a request held
    /// for the first status query never reorders them. As for a status
    /// request, no clone of the sender lives across the caller's await on its
    /// answer.
    pub(crate) async fn submit_supervise(
        &self,
        request: crate::types::SuperviseRequest,
    ) -> Result<(), ()> {
        let tx = {
            let mut slot = self.shared.system.lock().unwrap_or_else(|p| p.into_inner());
            match std::mem::take(&mut *slot) {
                SystemSlot::Closed => {
                    *slot = SystemSlot::Closed;
                    return Err(());
                }
                SystemSlot::Idle => {
                    let (tx, rx) = tokio::sync::mpsc::channel(SYSTEM_CAPACITY);
                    *slot = SystemSlot::Open {
                        first: None,
                        tx: tx.clone(),
                        rx: Some(rx),
                    };
                    self.shared
                        .system_state
                        .fetch_or(SYSTEM_OPEN, Ordering::Release);
                    tx
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
        let permit = crate::actor::panic::reserve_held("supervise queue", None, &tx, |_| {})
            .await
            .ok_or(())?;
        permit
            .into_inner()
            .send(crate::types::SystemMessage::Supervise(Box::new(request)));
        self.shared.reader.wake();
        Ok(())
    }

    /// Queues a `sys` command on the incarnation's system plane, waiting for
    /// room when the plane is full, then wakes the run loop with no await
    /// between the queueing and the wake. `Err` once the run loop closed the
    /// plane.
    ///
    /// The held slot takes status requests only, so a command always goes
    /// through the channel: a plane with nothing held opens it, a plane that
    /// holds a status request opens it and keeps that request as the oldest,
    /// answered first, exactly as [`submit_status`](Self::submit_status)
    /// does. As there, no clone of the plane's sender outlives this call, so
    /// the caller's wait for the reply never keeps the channel open.
    pub(crate) async fn submit_sys(&self, command: crate::types::SysCommand) -> Result<(), ()> {
        let message = crate::types::SystemMessage::Sys(Box::new(command));
        let tx = {
            let mut slot = self.shared.system.lock().unwrap_or_else(|p| p.into_inner());
            match std::mem::take(&mut *slot) {
                SystemSlot::Closed => {
                    *slot = SystemSlot::Closed;
                    return Err(());
                }
                SystemSlot::Idle => {
                    let (tx, rx) = tokio::sync::mpsc::channel(SYSTEM_CAPACITY);
                    *slot = SystemSlot::Open {
                        first: None,
                        tx: tx.clone(),
                        rx: Some(rx),
                    };
                    self.shared
                        .system_state
                        .fetch_or(SYSTEM_OPEN, Ordering::Release);
                    tx
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
        // The slot is held as every slot of a bounded channel is (see
        // `reserve_held`): a caller that waits on a full plane is woken by the
        // run loop's receive, and its waker's panic stays out of that loop.
        let permit = crate::actor::panic::reserve_held("sys queue", None, &tx, |_| {})
            .await
            .ok_or(())?;
        permit.into_inner().send(message);
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
        owed.mark = Some(Box::new(OwedMark { chain }));
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
    ///
    /// The abort that writes the first instant of an unfinished task reports
    /// the `AbortFired` step, and no other does: the instant is written once
    /// per incarnation under the record's lock, so two aborts that race (the
    /// reaper's and a caller's own) report one step. The event is built
    /// before the lock is taken and queued inside it, right after the instant
    /// is written, so an abort that lost the race passes the lock only once
    /// the step is queued and cannot end the task ahead of it: the step
    /// precedes the events of the actor's end whichever abort ends the task.
    pub(crate) fn abort(&self, task: &AbortHandle) {
        if task.is_finished() {
            self.release();
            return;
        }
        let staged = crate::system::stage_abort_fired(self);
        self.record(Instant::now(), staged);
        task.abort();
    }

    /// Records `now` as the first abort and returns true, or returns false
    /// when an abort was recorded already, which stands. The first abort
    /// queues `staged` while it holds the lock: the push is a bounded queue
    /// push that never waits and runs no user code.
    ///
    /// The chain of a record with no owed mark is read inside the critical
    /// section, and not at all once the actor has ended: the release of the
    /// actor's hold takes this lock before it lets go of the token, so no
    /// reader of the chain holds the token when the hold's release is
    /// written (see [`ReleaseSignal`]). An abort recorded after the actor
    /// ended reaches no ancestor: nothing is left of it to wait for.
    fn record(&self, now: Instant, staged: Option<crate::telemetry::Staged>) -> bool {
        let (mark, chain) = {
            let mut owed = self.shared.owed.lock().unwrap_or_else(|p| p.into_inner());
            if owed.at.is_some() {
                return false;
            }
            owed.at = Some(now);
            #[cfg(test)]
            test_hook::run_before_push();
            if let Some(staged) = staged {
                staged.push();
            }
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
        self.notify_signals();
        for shared in &chain {
            shared.update_aborts(|a| a.latest = Some(a.latest.map_or(now, |l| l.max(now))));
        }
        drop(mark);
        true
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

    /// A future woken when the first abort is recorded (and by every other
    /// change the block's signals announce); create it before reading the
    /// record so no record is missed, and read the record again after each
    /// wake.
    pub(crate) fn recorded(&self) -> tokio::sync::futures::Notified<'_> {
        self.shared.signals.notified()
    }

    /// Links the record to the incarnation's subtree token.
    fn link(&self, token: &Arc<SubtreeToken>, trap: Option<Box<TrapSpec>>) {
        let _ = self.shared.linked.set(Linked {
            token: Arc::downgrade(token),
            shared: token.shared.clone(),
            trap,
        });
    }

    /// The `Shutdown` of a worker below a supervisor, as its trap spec stands
    /// for it: the window of a `Timeout`, `Infinity`, else `Kill`. What a
    /// link guard needs to know of a child that traps exit signals when the
    /// guard fires.
    pub(crate) fn worker_shutdown(&self) -> crate::types::Shutdown {
        match self.shared.linked.get().and_then(|l| l.trap.as_deref()) {
            None => crate::types::Shutdown::Kill,
            Some(spec) if spec.infinity => crate::types::Shutdown::Infinity,
            Some(spec) => {
                crate::types::Shutdown::Timeout(std::time::Duration::from_nanos(spec.extra_nanos))
            }
        }
    }

    /// Gives back the count the trap of a worker under `Shutdown::Infinity`
    /// raised, when the actor's hold drops.
    fn close_trap_mark(&self) {
        if let Some(spec) = self.shared.linked.get().and_then(|l| l.trap.as_deref()) {
            SubtreeToken::close_trap_mark(spec);
        }
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
    /// The time this actor can outlive a killed parent, in nanoseconds. It
    /// rises once, when a worker starts to trap exit signals (see
    /// `TrapSpec`), and never falls.
    own_nanos: AtomicU64,
    parent: Option<Arc<SubtreeToken>>,
}

/// What a worker that traps exit signals adds to the budget above it. Kept
/// beside the actor's record, which only a worker below a supervisor whose
/// `Shutdown` has a window or is `Infinity` makes one for.
#[derive(Debug)]
struct TrapSpec {
    /// What the worker adds to `own_nanos`: its own `Shutdown` window, since
    /// a killed supervisor then stops it in order within that window instead
    /// of killing it. Zero for a worker whose stop has no window to add.
    extra_nanos: u64,
    /// Whether the actor is a worker under `Shutdown::Infinity`: once it
    /// traps exit signals its orderly stop has no bound, so it counts among
    /// the live unbounded stops above it, as a supervisor child under
    /// `Infinity` does from its start.
    infinity: bool,
    /// The count `infinity` raised, held until the actor's hold drops.
    mark: Mutex<TrapMark>,
    /// Set by the first trap: the budget rises once per incarnation, so a
    /// later `trap_exit`, or one after `untrap_exit`, raises nothing more.
    raised: AtomicBool,
}

/// The state of the unbounded-stop count a trapping worker adds.
#[derive(Debug, Default)]
enum TrapMark {
    /// The actor has not trapped.
    #[default]
    Unset,
    /// The actor trapped: the count is raised until the mark drops.
    Held(#[allow(dead_code)] InfinityMark),
    /// The actor's hold dropped: the count was given back, and a later trap
    /// raises nothing.
    Closed,
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
                parent.raise_below(
                    self.own_nanos
                        .load(Ordering::SeqCst)
                        .saturating_add(candidate),
                );
            }
        }
    }

    /// The actor started to trap exit signals: a killed parent stops it in
    /// order within its own window, so the budget of every wait above it
    /// rises by that window. Once; a later `untrap_exit` lowers nothing, as
    /// the budget never falls.
    fn raise_for_trap(&self, spec: &TrapSpec) {
        if spec.raised.swap(true, Ordering::SeqCst) {
            return;
        }
        if spec.infinity {
            self.mark_unbounded_stop(spec);
        }
        if spec.extra_nanos == 0 {
            return;
        }
        let base = self.own_nanos.load(Ordering::SeqCst);
        let raised = base.saturating_add(spec.extra_nanos);
        let previous = self.own_nanos.fetch_max(raised, Ordering::SeqCst);
        if raised > previous {
            if let Some(parent) = &self.parent {
                let below = self.shared.below_nanos.load(Ordering::SeqCst);
                parent.raise_below(raised.saturating_add(below));
            }
        }
    }

    /// Counts this worker among the live unbounded stops above it, once, for
    /// as long as its hold lives. A later `untrap_exit` lowers nothing, as
    /// the budget never falls.
    fn mark_unbounded_stop(&self, spec: &TrapSpec) {
        let Some(parent) = &self.parent else {
            return;
        };
        let mut mark = spec.mark.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(*mark, TrapMark::Unset) {
            SubtreeToken::shift_infinity(parent, true);
            *mark = TrapMark::Held(InfinityMark {
                above: parent.clone(),
            });
        }
    }

    /// Gives the count back when the actor's hold drops.
    fn close_trap_mark(spec: &TrapSpec) {
        let released = {
            let mut mark = spec.mark.lock().unwrap_or_else(|p| p.into_inner());
            std::mem::replace(&mut *mark, TrapMark::Closed)
        };
        drop(released);
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
    /// What the child adds to `own` once its actor traps exit signals: its
    /// own `Shutdown` window for a worker, zero for a supervisor child.
    pub trap_extra: Duration,
    /// Whether the child is a worker under `Shutdown::Infinity`, whose
    /// orderly stop has no bound once its actor traps exit signals.
    pub trap_infinity: bool,
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
        self.record.close_trap_mark();
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
        let trap = link.as_ref().and_then(|link| {
            let extra_nanos = rules::saturating_nanos(link.trap_extra);
            (extra_nanos != 0 || link.trap_infinity).then(|| {
                Box::new(TrapSpec {
                    extra_nanos,
                    infinity: link.trap_infinity,
                    mark: Mutex::new(TrapMark::Unset),
                    raised: AtomicBool::new(false),
                })
            })
        });
        let hold = Self::build(link, record);
        hold.record.link(&hold.token, trap);
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
                    own_nanos: AtomicU64::new(0),
                    parent: None,
                }),
                None,
            ),
            Some(link) => {
                let own_nanos = rules::saturating_nanos(link.own);
                let token = Arc::new(SubtreeToken {
                    shared: SubtreeShared::new(),
                    own_nanos: AtomicU64::new(own_nanos),
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
            trap_extra: std::time::Duration::ZERO,
            trap_infinity: false,
        };
        (SubtreeHold::with_record(Some(link), record.clone()), record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(parent: &Arc<SubtreeHold>, own: Duration, infinity: bool) -> Arc<SubtreeHold> {
        SubtreeHold::new(Some(SubtreeLink {
            parent: parent.token(),
            own,
            infinity,
            trap_extra: std::time::Duration::ZERO,
            trap_infinity: false,
        }))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
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
}

/// A wait on a subtree gives its verdict no later than its deadline plus its
/// bound plus the settle time, whatever its members do near that cap: an
/// end or an abort recorded within a settle time of the last one raises the
/// floor no further than the ceiling.
#[cfg(test)]
mod ceiling_tests {
    use super::*;

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
                trap_extra: std::time::Duration::ZERO,
                trap_infinity: false,
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
        static BEFORE_PUSH: Hook = const { RefCell::new(None) };
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

    /// Armed like [`arm`], but run by the next first-abort record while it
    /// holds the owed lock, after it wrote the instant and before it queues
    /// the abort's event.
    pub(crate) fn arm_before_push(hook: impl FnOnce() + 'static) {
        BEFORE_PUSH.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn run_before_push() {
        if let Some(hook) = BEFORE_PUSH.with(|h| h.borrow_mut().take()) {
            hook();
        }
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

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn child(parent: &Arc<SubtreeHold>, own: Duration, infinity: bool) -> Arc<SubtreeHold> {
        SubtreeHold::new(Some(SubtreeLink {
            parent: parent.token(),
            own,
            infinity,
            trap_extra: std::time::Duration::ZERO,
            trap_infinity: false,
        }))
    }
}

/// A worker under `Shutdown::Infinity` that traps exit signals is waited for
/// without a bound, as a supervisor child under `Infinity` is, for as long as
/// its actor lives.
#[cfg(test)]
mod trap_unbounded_tests {
    use super::*;
    use crate::types::Shutdown;

    fn worker_below(
        parent: &Arc<SubtreeHold>,
        trap_infinity: bool,
    ) -> (Arc<SubtreeHold>, AbortRecord) {
        let record = AbortRecord::new();
        let hold = SubtreeHold::with_record(
            Some(SubtreeLink {
                parent: parent.token(),
                own: Duration::from_millis(50),
                infinity: false,
                trap_extra: Duration::ZERO,
                trap_infinity,
            }),
            record.clone(),
        );
        (hold, record)
    }

    #[test]
    fn a_trapping_worker_under_infinity_holds_the_wait_until_its_actor_ends() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let (worker, record) = worker_below(&root, true);
        assert_eq!(watch.infinity_live(), 0, "not trapping: bounded");
        assert!(watch.release_bound().is_some());
        record.set_trap(true);
        assert_eq!(watch.infinity_live(), 1, "trapping: counted once");
        assert!(watch.release_bound().is_none(), "and without a bound");
        record.set_trap(true);
        record.set_trap(false);
        assert_eq!(
            watch.infinity_live(),
            1,
            "a toggle counts nothing more and takes nothing back"
        );
        // A child below the worker keeps the worker's token alive after the
        // worker's actor ended; the count is the actor's, not the token's.
        let (kid, _kid_record) = worker_below(&worker, false);
        drop(worker);
        assert_eq!(watch.infinity_live(), 0, "the actor's end gives it back");
        assert!(watch.release_bound().is_some());
        record.set_trap(true);
        assert_eq!(
            watch.infinity_live(),
            0,
            "a trap after the end counts nothing"
        );
        drop(kid);
    }

    /// A link built the way a spawn builds it, from the child's kind and
    /// `Shutdown`.
    fn link_below(
        parent: &Arc<SubtreeHold>,
        is_supervisor: bool,
        shutdown: Shutdown,
        record: &AbortRecord,
    ) -> Arc<SubtreeHold> {
        SubtreeHold::with_record(
            Some(SubtreeLink {
                parent: parent.token(),
                own: Duration::from_millis(50),
                infinity: is_supervisor && shutdown == Shutdown::Infinity,
                trap_extra: rules::trap_extra(is_supervisor, shutdown),
                trap_infinity: rules::trap_infinity(is_supervisor, shutdown),
            }),
            record.clone(),
        )
    }

    /// A supervisor child under `Infinity` is counted from its start, and
    /// trapping adds nothing: it is one unbounded stop, not two.
    #[test]
    fn a_trapping_supervisor_child_under_infinity_is_counted_once() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let record = AbortRecord::new();
        let _child = link_below(&root, true, Shutdown::Infinity, &record);
        assert_eq!(watch.infinity_live(), 1, "counted from its start");
        record.set_trap(true);
        assert_eq!(watch.infinity_live(), 1, "trapping counts nothing more");
    }

    /// A worker is counted by trapping only under `Infinity`: under a window
    /// or `Kill` it never enters the count.
    #[test]
    fn a_worker_is_counted_by_trapping_only_under_infinity() {
        for (shutdown, counted) in [
            (Shutdown::Kill, false),
            (Shutdown::Timeout(Duration::from_secs(3)), false),
            (Shutdown::Infinity, true),
        ] {
            let root = SubtreeHold::new(None);
            let watch = root.watch();
            let record = AbortRecord::new();
            let _worker = link_below(&root, false, shutdown, &record);
            assert_eq!(watch.infinity_live(), 0, "{shutdown:?}: not trapping");
            record.set_trap(true);
            assert_eq!(
                watch.infinity_live(),
                u32::from(counted),
                "{shutdown:?}: trapping"
            );
        }
    }

    /// A worker with a window adds the window to the budget above it once it
    /// traps, and not before, and only once per incarnation.
    #[test]
    fn a_trapping_worker_with_a_window_raises_the_budget_above_it() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let record = AbortRecord::new();
        let window = Duration::from_secs(3);
        let _worker = link_below(&root, false, Shutdown::Timeout(window), &record);
        let before = watch.release_bound().expect("a bound");
        record.set_trap(true);
        let after = watch.release_bound().expect("a bound");
        assert!(
            after >= before + window,
            "the window is added once the worker traps: {before:?} then {after:?}"
        );
        record.set_trap(true);
        record.set_trap(false);
        record.set_trap(true);
        assert_eq!(
            watch.release_bound().expect("a bound"),
            after,
            "a second trap, and one after untrapping, raise nothing more"
        );
    }
}

/// The size of the per-incarnation record, pinned: it holds the system's
/// telemetry state by one pointer and nothing else for telemetry. The record
/// is no larger than a mirror of its fields as they were before the pointer,
/// built from the same field types in the same build, so the claim holds on
/// every target and toolchain; the exact sizes are pinned only where they were
/// measured.
#[cfg(test)]
mod footprint_tests {
    use super::*;

    /// The owed state with the mark held inline.
    #[allow(dead_code)]
    struct InlineOwed {
        at: Option<Instant>,
        mark: Option<InlineMark>,
        ended: bool,
    }

    #[allow(dead_code)]
    struct InlineMark {
        chain: Vec<Arc<SubtreeShared>>,
    }

    /// The record's fields without the pointer to the telemetry state, with
    /// the owed state held inline.
    #[allow(dead_code)]
    struct WithoutPointer {
        linked: OnceLock<Linked>,
        owed: Mutex<InlineOwed>,
        status: crate::types::status_cell::StatusCell,
        recorded: tokio::sync::Notify,
        reader: crate::types::WakeSlot,
        lane: crate::types::LaneCell,
        signals: tokio::sync::Notify,
        system: Mutex<SystemSlot>,
        system_state: std::sync::atomic::AtomicU8,
    }

    /// The sizes as measured on x86_64 Linux, where the standard library's
    /// locks and the layout of the fields are the ones measured.
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    #[test]
    fn the_record_has_its_measured_size() {
        // The block's own pieces (the incarnation number and the signal
        // cell) are what it holds beyond v0.12.0's 408 bytes; the telemetry
        // pointer and the three telemetry atomics (the latter in the tail of
        // the lane's cell) add nothing.
        assert_eq!(std::mem::size_of::<AbortShared>(), 416, "AbortShared");
        assert_eq!(
            std::mem::size_of::<crate::types::LaneCell>(),
            96,
            "the lane's cell, with the record's three atomics in its tail"
        );
        assert_eq!(std::mem::size_of::<Owed>(), 32, "Owed");
        assert_eq!(
            std::mem::size_of::<OnceLock<Arc<crate::telemetry::Mode>>>(),
            16,
            "the record's pointer to the system's telemetry state"
        );
    }
}
