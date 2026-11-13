//! The stop lane: a cell in the incarnation's abort block, used to deliver
//! stop and kill requests.
//!
//! Every actor owns one lane. Raising it is synchronous - it never awaits
//! mailbox capacity, unlike a channel send - and the run loop
//! observes it with the highest priority of anything in its `select!`, ahead
//! of the system channel and the user mailbox. It is the single transport for
//! every stop or kill signal: spawning-side, supervision-side, and
//! system-shutdown-side alike.
//!
//! # The cell
//!
//! The lane's state, its version and the receiver-gone flag live in a
//! [`LaneCell`] inside the incarnation's abort block, beside the block's
//! `signals` notification. The state is guarded by one mutex that is a leaf:
//! nothing is acquired while it is held, no wake runs under it, and no user
//! code runs under it. The version is a copy of the state's generation,
//! written inside the same critical section as the state and readable
//! without the lock.
//!
//! The orderings every operation keeps:
//!
//! | operation | reads | writes | order |
//! |---|---|---|---|
//! | `try_raise` | receiver-gone flag, state | state, generation, version | decision, state and version in one critical section, the version written last; the wakes after the unlock, each contained |
//! | `close_to_non_kill`, `seal` | state | state flags | one critical section, no version change, no wake |
//! | `reason` | state | none | under the lock |
//! | `LaneRx::borrow` | state | none | never marks a version seen |
//! | `LaneRx::borrow_and_update` | state and version | seen | both read under the one lock; seen is set to that version |
//! | `LaneRx::has_changed` | version | none | no lock |
//! | `LaneRx::changed` | version | seen | the notification is created first, then the version is read under the lock, then it waits |
//!
//! A guard handed out by `borrow` is not reentrant: while one is held, no
//! other operation on the same lane may run on that thread. The guards are
//! `!Send` and must not be held across an await.

use std::convert::Infallible;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Waker;

use crate::actor::panic::contain_waker_panic;
use crate::actor::subtree::AbortRecord;
use crate::actor::supervision::rules::lane_accept;
use crate::types::StopReason;

#[cfg(test)]
thread_local! {
    static UNWIND_AFTER_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arms the next [`StopLane::try_raise`] on this thread that records a
/// request to unwind right after the write, where its wakes would run.
#[cfg(test)]
pub(crate) fn unwind_after_the_next_write() {
    UNWIND_AFTER_WRITE.with(|armed| armed.set(true));
}

#[cfg(test)]
fn unwind_after_the_write_if_armed() {
    if UNWIND_AFTER_WRITE.with(|armed| armed.replace(false)) {
        panic!("unwind after the lane write");
    }
}

/// Escalation tier of a stop request, derived from its [`StopReason`]. Used
/// only to keep the lane monotone: comparing two requests' tiers is how
/// `raise` decides whether an incoming request may overwrite the pending one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StopSeverity {
    /// Vetoable via `pre_stop`: [`StopReason::Graceful`],
    /// [`StopReason::ParentRequest`], [`StopReason::Shutdown`].
    Requested,
    /// Non-vetoable, but `on_stopped` still runs: [`StopReason::Failure`],
    /// [`StopReason::ParentKilled`], [`StopReason::Cancelled`].
    Forced,
    /// Not vetoable, and no lifecycle hook begins once it is read: [`StopReason::Kill`].
    Kill,
}

impl StopSeverity {
    pub(crate) fn of(reason: &StopReason) -> Self {
        match reason {
            StopReason::Graceful | StopReason::ParentRequest | StopReason::Shutdown(_) => {
                StopSeverity::Requested
            }
            StopReason::Failure(_) | StopReason::ParentKilled | StopReason::Cancelled => {
                StopSeverity::Forced
            }
            StopReason::Kill => StopSeverity::Kill,
        }
    }

    /// Whether a request with this reason is a `Requested` one, which the
    /// actor's `pre_stop` may veto. The one reader of the tier for the
    /// `pre_stop` gate and for a supervisor's veto check.
    pub(crate) fn is_vetoable(reason: &StopReason) -> bool {
        StopSeverity::of(reason) == StopSeverity::Requested
    }

    /// The order of two requests of the same severity: a parent's request
    /// outranks a shutdown the actor asked for itself, which outranks a plain
    /// graceful one, and a failure outranks an inherited stop, which outranks
    /// a cancellation. Ranks are compared only inside one severity.
    pub(crate) fn rank(reason: &StopReason) -> u8 {
        match reason {
            StopReason::Graceful | StopReason::Cancelled | StopReason::Kill => 0,
            StopReason::Shutdown(_) | StopReason::ParentKilled => 1,
            StopReason::ParentRequest | StopReason::Failure(_) => 2,
        }
    }
}

/// Value carried by the stop lane: the highest-severity request raised so
/// far, and a generation counter bumped on every accepted raise - including a
/// re-raise at the same severity as the one already pending, which changes
/// nothing about the tier but still guarantees the run loop re-observes it
/// (for example, re-firing `pre_stop` after an earlier veto).
///
/// `kill_only` is set at the closing point of the message loop: from there
/// the lane accepts a `Kill` and nothing else. `sealed` is set at the end of
/// the actor's teardown, just before its queued messages and its value are
/// dropped: from there it accepts nothing.
///
/// `kill_cause` is the reason a `Kill` was raised on behalf of, when the
/// raiser was an exit signal: the reason the actor ends with in place of
/// `Kill`. It is written once, by the raise that puts the `Kill` on the lane
/// (a lane that holds a `Kill` already keeps what it has), a plain `Kill`
/// clears it, and it is `Some` only while the stored reason is `Kill`.
#[derive(Debug, Clone, Default)]
pub(crate) struct LaneState {
    pub(crate) reason: Option<StopReason>,
    pub(crate) generation: u64,
    pub(crate) kill_only: bool,
    pub(crate) sealed: bool,
    /// Boxed: a cause is rare, and the lane's cell is shared by every actor.
    pub(crate) kill_cause: Option<Box<StopReason>>,
}

/// How the lane answered a raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RaiseResult {
    /// The request was accepted: the generation moved, and the stored reason
    /// is the new one unless an equal-severity request of higher rank is
    /// pending.
    Recorded,
    /// A strictly higher severity is pending: nothing was written.
    Superseded,
    /// The lane takes no such request any more (sealed, open to `Kill` only,
    /// or its actor's task is gone): nothing was written.
    Refused,
}

/// The lane's state, version and receiver-gone flag. Lives in the
/// incarnation's abort block; see the module documentation for the
/// orderings.
#[derive(Debug, Default)]
pub(crate) struct LaneCell {
    state: Mutex<LaneState>,
    /// A copy of the state's generation, written last inside the critical
    /// section that moves the generation. Read without the lock by
    /// [`LaneRx::has_changed`]; read under the lock wherever it must belong
    /// to a value read in the same section.
    version: AtomicU64,
    /// Set once, by the receiver's drop: the actor's task is gone.
    rx_gone: AtomicBool,
    // The three fields below belong to the incarnation's record, not to the
    // lane: they sit in this cell's tail padding, so the record's own small
    // atomics fit one word and the record grows by nothing for them. Each is
    // an independent atomic with the ordering its accessor on `AbortRecord`
    // documents; none is read or written under the lane's lock.
    /// The `sys` bundle's bits (`SYS_SUSPENDED`, `SYS_ALLOW_STATE`,
    /// `SYS_SENSITIVE`) and the self-stop mark (`SELF_STOP`); see
    /// `AbortRecord::sys_bits` and `AbortRecord::mark_self_stop`.
    pub(crate) sys_bits: std::sync::atomic::AtomicU8,
    /// The children the incarnation supervises now; see
    /// `AbortRecord::children`.
    pub(crate) children: std::sync::atomic::AtomicU32,
    /// Set when the incarnation's registry guard starts to drop, while the
    /// system tracks turns; see `AbortRecord::has_ended`.
    pub(crate) ended: AtomicBool,
}

/// A held critical section of the cell: the state under the lock. Not
/// `Send`, so it can never be held across an await in a `Send` future.
struct LaneSection<'a> {
    guard: MutexGuard<'a, LaneState>,
    #[cfg(test)]
    _mark: held::Mark,
}

impl Deref for LaneSection<'_> {
    type Target = LaneState;
    fn deref(&self) -> &LaneState {
        &self.guard
    }
}

impl DerefMut for LaneSection<'_> {
    fn deref_mut(&mut self) -> &mut LaneState {
        &mut self.guard
    }
}

/// A read of the lane's state under its lock. Not `Send`, and not
/// reentrant: while it is held, no other operation on the same lane may run
/// on the thread (the lock is not reentrant), so read what is needed and drop
/// it before the next call. A release build does not detect a violation, and
/// the lock may then hang or panic; a test build detects it and panics. It
/// never marks a version seen.
#[must_use = "a lane guard holds the lane's lock until dropped"]
pub(crate) struct LaneRef<'a>(LaneSection<'a>);

impl Deref for LaneRef<'_> {
    type Target = LaneState;
    fn deref(&self) -> &LaneState {
        &self.0
    }
}

/// What one raise did, handed back so the caller wakes and drops outside
/// the lock.
struct Raised {
    result: RaiseResult,
    /// The generation moved: receivers have a new version to observe.
    bumped: bool,
    /// The reason the raise replaced, dropped after the unlock.
    replaced: Option<StopReason>,
}

impl LaneCell {
    fn section(&self) -> LaneSection<'_> {
        #[cfg(test)]
        let mark = held::Mark::enter(self);
        let guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        LaneSection {
            guard,
            #[cfg(test)]
            _mark: mark,
        }
    }

    /// The version, without the lock.
    fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Whether a raise of `reason` would be refused now, deciding as
    /// [`raise`](Self::raise) does and writing nothing.
    fn refuses(&self, reason: &StopReason) -> bool {
        let section = self.section();
        let receiver_gone = self.rx_gone.load(Ordering::Acquire);
        lane_accept(&section, reason, receiver_gone).0 == RaiseResult::Refused
    }

    /// The cause stored with a pending `Kill`, if any.
    pub(crate) fn kill_cause(&self) -> Option<StopReason> {
        self.section().kill_cause.as_deref().cloned()
    }

    /// Decides and writes one raise of a reason that is not `Kill` in a single
    /// critical section. The receiver-gone flag is read inside it. A raise
    /// of any other reason touches no stored cause.
    fn raise(&self, reason: StopReason) -> Raised {
        let mut section = self.section();
        let receiver_gone = self.rx_gone.load(Ordering::Acquire);
        let (result, store, bump) = lane_accept(&section, &reason, receiver_gone);
        let mut replaced = None;
        if bump {
            section.generation = section.generation.wrapping_add(1);
            if store {
                replaced = section.reason.replace(reason);
            }
            // The version is the section's last write: a reader that
            // consumed it without the lock reads a state at least as new.
            self.version.store(section.generation, Ordering::Release);
        }
        drop(section);
        Raised {
            result,
            bumped: bump,
            replaced,
        }
    }

    /// As [`raise`](Self::raise), for a `Kill`. A `Kill` raised with a cause
    /// stores the cause only when the lane held no `Kill` before the raise
    /// and the cause is not `Kill` itself; a `Kill` raised without one
    /// clears the stored cause. The cause the raise cleared or did not store
    /// comes back with the result, to be dropped after the unlock.
    fn raise_kill(&self, cause: Option<StopReason>) -> (Raised, Option<StopReason>) {
        let mut section = self.section();
        let receiver_gone = self.rx_gone.load(Ordering::Acquire);
        let (result, store, bump) = lane_accept(&section, &StopReason::Kill, receiver_gone);
        let mut replaced = None;
        let mut spare_cause = None;
        if bump {
            section.generation = section.generation.wrapping_add(1);
            let held_a_kill = matches!(section.reason, Some(StopReason::Kill));
            match cause {
                None => spare_cause = section.kill_cause.take().map(|boxed| *boxed),
                Some(cause) if !held_a_kill && !matches!(cause, StopReason::Kill) => {
                    section.kill_cause = Some(Box::new(cause));
                }
                Some(cause) => spare_cause = Some(cause),
            }
            if store {
                replaced = section.reason.replace(StopReason::Kill);
            }
            self.version.store(section.generation, Ordering::Release);
        }
        drop(section);
        (
            Raised {
                result,
                bumped: bump,
                replaced,
            },
            spare_cause,
        )
    }
}

/// Per-actor stop/kill signal.
///
/// `raise` never blocks and never fails: it is a plain write to the lane's
/// cell, not a channel send, so there is no mailbox capacity to await and no
/// `Result` to hand back to the caller. [`try_raise`](Self::try_raise) is the
/// same write that also says how the lane answered. Severity only ever
/// increases - a `Kill` already raised can never be displaced by a later
/// `Graceful`/`ParentRequest` call - while a request at the same severity as
/// the one already pending still lands and bumps the generation, so it is
/// never silently swallowed; it replaces the stored reason only when it ranks
/// at least as high. Multiple raises that land between two run-loop
/// observations coalesce into one observation of the strongest request: the
/// loop sees that one request and not one per `raise` call, and a request
/// that a stronger one overtakes before the loop reads is never observed.
///
/// The lane lives in the incarnation's abort block, which every clone of the
/// lane shares.
#[derive(Debug, Clone)]
pub(crate) struct StopLane {
    /// The incarnation's abort block: the lane's cell, beside the first abort
    /// issued against the incarnation's task.
    aborted: AbortRecord,
}

/// The run loop's receiving half of the lane. It observes versions; it is
/// not cloneable, and its drop is how the lane learns the task is gone.
#[derive(Debug)]
pub(crate) struct LaneRx {
    record: AbortRecord,
    /// The last version this receiver marked seen.
    seen: u64,
}

impl LaneRx {
    /// The lane's state, under its lock. Never marks a version seen.
    pub(crate) fn borrow(&self) -> LaneRef<'_> {
        LaneRef(self.record.lane().section())
    }

    /// The lane's state, and marks the version read in the same critical
    /// section as seen.
    pub(crate) fn borrow_and_update(&mut self) -> LaneRef<'_> {
        let cell = self.record.lane();
        let section = cell.section();
        self.seen = cell.version.load(Ordering::Acquire);
        #[cfg(test)]
        assert_eq!(self.seen, section.generation, "version follows generation");
        LaneRef(section)
    }

    /// Whether a version this receiver has not marked seen exists. Reads the
    /// version without the lock. The lane has no closed state, so this is
    /// never an error.
    pub(crate) fn has_changed(&self) -> Result<bool, Infallible> {
        Ok(self.record.lane().version() != self.seen)
    }

    /// Waits for a version this receiver has not marked seen, and marks it
    /// seen. The lane has no closed state, so this never returns an error.
    /// Cancel safe.
    ///
    /// The wait creates its notification before it reads the version, and
    /// every raise notifies after its critical section, so a raise that
    /// lands after the read finds the notification.
    pub(crate) async fn changed(&mut self) -> Result<(), Infallible> {
        tokio::task::coop::cooperative(async {
            loop {
                let notified = self.record.signalled();
                let version = {
                    let cell = self.record.lane();
                    let _section = cell.section();
                    cell.version.load(Ordering::Acquire)
                };
                if version != self.seen {
                    self.seen = version;
                    return Ok(());
                }
                #[cfg(test)]
                hook::run();
                notified.await;
            }
        })
        .await
    }
}

impl Drop for LaneRx {
    fn drop(&mut self) {
        self.record.lane().rx_gone.store(true, Ordering::Release);
    }
}

/// The run loop's own waker: one reader, so one slot instead of a waiter
/// list. Kept in the incarnation's abort record. Armed only while the loop
/// waits in its select (armed by the stop branch's poll, disarmed when that
/// branch's future drops), so a raise wakes the loop exactly when a new
/// version appears: never during a callback.
#[derive(Debug, Default)]
pub(crate) struct WakeSlot {
    waker: Mutex<Option<Waker>>,
    armed: AtomicBool,
}

impl WakeSlot {
    /// Stores the loop's waker and arms the slot, under the lock a waking
    /// raise takes: a raise ordered before it is seen by the loop's read
    /// after it, one ordered after it finds the slot armed.
    pub(crate) fn register(&self, waker: &Waker) {
        let mut slot = self.waker.lock().unwrap_or_else(|p| p.into_inner());
        match slot.as_ref() {
            Some(held) if held.will_wake(waker) => {}
            _ => *slot = Some(waker.clone()),
        }
        self.armed.store(true, Ordering::Relaxed);
    }

    /// The loop is not waiting on the slot.
    pub(crate) fn disarm(&self) {
        self.armed.store(false, Ordering::Relaxed);
    }

    /// Wakes the loop if it is waiting on the slot. The waker is cloned
    /// under the lock and woken outside it. The waker belongs to the loop's
    /// task, not to the caller, so a panic of its wake is contained (see
    /// [`contain_waker_panic`]): the raise or the status request that wakes
    /// the loop goes on.
    pub(crate) fn wake(&self) {
        let waker = {
            let slot = self.waker.lock().unwrap_or_else(|p| p.into_inner());
            if self.armed.swap(false, Ordering::Relaxed) {
                slot.clone()
            } else {
                None
            }
        };
        if let Some(waker) = waker {
            contain_waker_panic("run loop", None, move || waker.wake());
        }
    }

    /// Drops the stored waker, so a task that does not read the slot is
    /// not kept alive by it. The loop's own task calls this, and the waker is
    /// its own: a panic of its drop is that task's.
    pub(crate) fn clear(&self) {
        let waker = self.waker.lock().unwrap_or_else(|p| p.into_inner()).take();
        self.armed.store(false, Ordering::Relaxed);
        drop(waker);
    }
}

impl StopLane {
    /// Creates a fresh, unraised lane and the run loop's receiving half, for
    /// a unit test that names no system.
    #[cfg(test)]
    pub(crate) fn new() -> (StopLane, LaneRx) {
        let aborted = AbortRecord::new();
        let rx = LaneRx {
            record: aborted.clone(),
            seen: 0,
        };
        (StopLane { aborted }, rx)
    }

    /// As [`new`](Self::new), for the incarnation `incarnation` (a number the
    /// spawn drew) of the system whose telemetry state is `mode`: the record
    /// holds both from the start.
    pub(crate) fn new_in(
        mode: Arc<crate::telemetry::Mode>,
        incarnation: crate::types::Incarnation,
    ) -> (StopLane, LaneRx) {
        let aborted = AbortRecord::in_system(mode, incarnation);
        let rx = LaneRx {
            record: aborted.clone(),
            seen: 0,
        };
        (StopLane { aborted }, rx)
    }

    /// The incarnation's abort record.
    pub(crate) fn abort_record(&self) -> &AbortRecord {
        &self.aborted
    }

    /// Raises a stop request at `reason`'s severity and reports how the lane
    /// answered. The decision and the write are one step.
    ///
    /// Refused once the lane is sealed, when it is open to `Kill` only and
    /// `reason` is not `Kill`, and when the actor's task has dropped the
    /// receiving half (an aborted task never reaches the tail that seals the
    /// lane). Superseded when a strictly higher severity is pending.
    /// Otherwise recorded, and the generation counter is bumped even when the
    /// specific reason is unchanged from the one already pending.
    pub(crate) fn try_raise(&self, reason: StopReason) -> RaiseResult {
        if matches!(reason, StopReason::Kill) {
            let (raised, spare_cause) = self.aborted.lane().raise_kill(None);
            let result = self.after_raise(raised);
            drop(spare_cause);
            return result;
        }
        let raised = self.aborted.lane().raise(reason);
        self.after_raise(raised)
    }

    /// Raises a `Kill` on behalf of `cause`, the reason an exit signal
    /// carried: the lane stores the cause when it holds no `Kill` yet, and the
    /// actor ends with it in place of `Kill`. Answers as
    /// [`try_raise`](Self::try_raise) does. Only
    /// [`raise_kill_declared`](crate::system::raise_kill_declared) calls it,
    /// so a caused `Kill` is always declared and backstopped.
    pub(crate) fn try_raise_kill_caused(&self, cause: StopReason) -> RaiseResult {
        let (raised, spare_cause) = self.aborted.lane().raise_kill(Some(cause));
        let result = self.after_raise(raised);
        drop(spare_cause);
        result
    }

    /// What follows every raise, outside the critical section.
    #[inline]
    fn after_raise(&self, raised: Raised) -> RaiseResult {
        // The wakes run after the critical section, exactly when a new
        // version exists. Each one is contained (the run loop's and the
        // waiters' wakers belong to other tasks), so a raise returns whatever
        // a waker does, and a caller that goes on after it (the start bound's
        // cut raises, then aborts) is never cut short by one.
        if raised.bumped {
            #[cfg(test)]
            held::assert_none();
            #[cfg(test)]
            unwind_after_the_write_if_armed();
            self.aborted.reader().wake();
            self.aborted.notify_signals();
        }
        drop(raised.replaced);
        raised.result
    }

    /// Whether a raise of `reason` is refused now, without raising it. A
    /// refusal is final: a lane that refuses a request never takes it again,
    /// so a caller that must answer such an actor without a timer reads this
    /// first, and `try_raise` stays the only writer.
    pub(crate) fn refuses(&self, reason: &StopReason) -> bool {
        self.aborted.lane().refuses(reason)
    }

    /// Raises a stop request exactly like [`try_raise`](Self::try_raise),
    /// for the callers that have no one to tell: a refused request is
    /// dropped silently.
    #[cfg(test)]
    pub(crate) fn raise(&self, reason: StopReason) {
        let _ = self.try_raise(reason);
    }

    /// The request currently pending on the lane, if any: the
    /// highest-severity reason raised so far. Read under the lane's lock.
    pub(crate) fn reason(&self) -> Option<StopReason> {
        self.aborted.lane().section().reason.clone()
    }

    /// Opens the lane to `Kill` only, without waking the actor. Called at
    /// the closing point of the message loop; the receiver stays alive.
    pub(crate) fn close_to_non_kill(&self) {
        self.aborted.lane().section().kill_only = true;
    }

    /// Seals the lane and returns the request pending on it, in one step.
    /// From here every raise is refused. Idempotent, and it works when the
    /// receiving half is gone.
    #[cfg(test)]
    pub(crate) fn seal(&self) -> Option<StopReason> {
        self.seal_caused().0
    }

    /// [`seal`](Self::seal), also returning the cause stored with a pending
    /// `Kill`, read in the same step: the request and its cause cannot
    /// differ.
    pub(crate) fn seal_caused(&self) -> (Option<StopReason>, Option<StopReason>) {
        let mut section = self.aborted.lane().section();
        section.sealed = true;
        let reason = section.reason.clone();
        // A cause is stored only while the stored reason is `Kill`.
        let cause = if matches!(reason, Some(StopReason::Kill)) {
            section.kill_cause.as_deref().cloned()
        } else {
            None
        };
        (reason, cause)
    }

    /// True once the lane takes no request but `Kill` any more: the actor
    /// has finished its message loop, or its task is gone. The callers that
    /// must answer a caller read the verdict of `try_raise` instead, which
    /// decides and writes in one step.
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        let cell = self.aborted.lane();
        let section = cell.section();
        section.sealed || section.kill_only || cell.rx_gone.load(Ordering::Acquire)
    }

    /// A lane whose cell starts at `state`, with its version matching.
    #[cfg(test)]
    pub(crate) fn with_state(state: LaneState) -> (StopLane, LaneRx) {
        let (lane, mut rx) = StopLane::new();
        let cell = lane.aborted.lane();
        cell.version.store(state.generation, Ordering::Release);
        *cell.section() = state;
        rx.seen = rx.record.lane().version();
        (lane, rx)
    }
}

/// Test instruments: a per-thread record of the lane locks held, so a
/// re-entrant lock or a wake under the lock panics instead of hanging or
/// passing unseen, and a one-shot hook inside `LaneRx::changed`.
#[cfg(test)]
pub(crate) mod held {
    use std::cell::{Cell, RefCell};

    use super::LaneCell;

    thread_local! {
        static HELD: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
        static SECTIONS: Cell<u64> = const { Cell::new(0) };
    }

    /// A lane lock held by this thread. Created before the lock is taken,
    /// so a re-entrant call panics instead of deadlocking.
    pub(super) struct Mark(usize);

    impl Mark {
        pub(super) fn enter(cell: &LaneCell) -> Mark {
            let at = cell as *const LaneCell as usize;
            HELD.with(|held| {
                let mut held = held.borrow_mut();
                assert!(
                    !held.contains(&at),
                    "re-entrant lock of one lane's cell on a thread that holds a guard of it"
                );
                held.push(at);
            });
            SECTIONS.with(|n| n.set(n.get() + 1));
            Mark(at)
        }
    }

    impl Drop for Mark {
        fn drop(&mut self) {
            let _ = HELD.try_with(|held| {
                let mut held = held.borrow_mut();
                if let Some(at) = held.iter().rposition(|held| *held == self.0) {
                    held.remove(at);
                }
            });
        }
    }

    /// Panics when this thread holds any lane lock: called where a wake or
    /// a notification runs.
    pub(crate) fn assert_none() {
        HELD.with(|held| {
            assert!(
                held.borrow().is_empty(),
                "a wake ran while a lane lock was held"
            );
        });
    }

    /// The number of critical sections this thread has entered.
    pub(crate) fn sections() -> u64 {
        SECTIONS.with(Cell::get)
    }
}

/// A one-shot hook that runs inside `LaneRx::changed`, after the version read
/// found nothing unseen and before the wait begins: it lets a test raise in
/// exactly that window.
#[cfg(test)]
pub(crate) mod hook {
    use std::cell::RefCell;

    thread_local! {
        static AFTER_READ: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    pub(crate) fn arm(hook: impl FnOnce() + 'static) {
        AFTER_READ.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn run() {
        if let Some(hook) = AFTER_READ.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ActorError;

    fn severity_of(state: &LaneState) -> Option<StopSeverity> {
        state.reason.as_ref().map(StopSeverity::of)
    }

    /// A `Shutdown` request is a vetoable one that ranks above a `Graceful`
    /// one and below a parent's request: it takes the place of a `Graceful`
    /// one, an outside `Graceful` one never takes its place, and a parent's
    /// request or any forced one stays.
    #[test]
    fn a_shutdown_request_is_vetoable_and_ranks_above_graceful() {
        let shutdown = || StopReason::Shutdown("x".into());
        assert!(StopSeverity::is_vetoable(&shutdown()));
        assert_eq!(StopSeverity::of(&shutdown()), StopSeverity::Requested);
        assert!(StopSeverity::rank(&shutdown()) > StopSeverity::rank(&StopReason::Graceful));
        assert!(StopSeverity::rank(&shutdown()) < StopSeverity::rank(&StopReason::ParentRequest));
        // (pending, incoming, the reason the lane keeps)
        let rows: Vec<(Option<StopReason>, StopReason, StopReason)> = vec![
            (None, shutdown(), shutdown()),
            (Some(StopReason::Graceful), shutdown(), shutdown()),
            (Some(shutdown()), StopReason::Graceful, shutdown()),
            (
                Some(StopReason::ParentRequest),
                shutdown(),
                StopReason::ParentRequest,
            ),
            (
                Some(shutdown()),
                StopReason::ParentRequest,
                StopReason::ParentRequest,
            ),
            (
                Some(StopReason::ParentKilled),
                shutdown(),
                StopReason::ParentKilled,
            ),
            (
                Some(StopReason::Cancelled),
                shutdown(),
                StopReason::Cancelled,
            ),
            (
                Some(StopReason::Failure(ActorError::user("f"))),
                shutdown(),
                StopReason::Failure(ActorError::user("f")),
            ),
        ];
        for (pending, incoming, kept) in rows {
            let (lane, rx) = StopLane::new();
            if let Some(pending) = pending.clone() {
                lane.raise(pending);
            }
            let answer = lane.try_raise(incoming.clone());
            let expected = if StopSeverity::of(&incoming)
                < pending
                    .as_ref()
                    .map_or(StopSeverity::Requested, StopSeverity::of)
            {
                RaiseResult::Superseded
            } else {
                RaiseResult::Recorded
            };
            assert_eq!(answer, expected, "{pending:?} then {incoming:?}");
            assert_eq!(
                rx.borrow().reason.as_ref(),
                Some(&kept),
                "{pending:?} then {incoming:?}"
            );
        }
    }

    // - Exhaustive raise table ------------------------------------------------

    /// One representative [`StopReason`] per severity, used to build the
    /// literal raise table below without reaching for `StopSeverity` itself.
    #[derive(Debug, Clone, Copy)]
    enum RaiseChoice {
        Requested,
        Forced,
        Kill,
    }

    impl RaiseChoice {
        fn reason(self) -> StopReason {
            match self {
                RaiseChoice::Requested => StopReason::Graceful,
                RaiseChoice::Forced => StopReason::Cancelled,
                RaiseChoice::Kill => StopReason::Kill,
            }
        }

        /// True if `stored` holds exactly the reason this choice raises.
        fn matches_stored(self, stored: &Option<StopReason>) -> bool {
            matches!(
                (self, stored),
                (RaiseChoice::Requested, Some(StopReason::Graceful))
                    | (RaiseChoice::Forced, Some(StopReason::Cancelled))
                    | (RaiseChoice::Kill, Some(StopReason::Kill))
            )
        }
    }

    // Twelve rows: every combination of a prior lane state (none, or one of
    // the three severities already raised) and a newly raised severity. This
    // is an independent statement of the contract - not derived from
    // `StopSeverity`'s ordering or from `StopLane::raise`'s own comparison.
    const RAISE_TABLE: [(Option<RaiseChoice>, RaiseChoice, bool); 12] = [
        // no prior raise: everything is accepted
        (None, RaiseChoice::Requested, true),
        (None, RaiseChoice::Forced, true),
        (None, RaiseChoice::Kill, true),
        // prior: Requested (Graceful/ParentRequest)
        (Some(RaiseChoice::Requested), RaiseChoice::Requested, true),
        (Some(RaiseChoice::Requested), RaiseChoice::Forced, true),
        (Some(RaiseChoice::Requested), RaiseChoice::Kill, true),
        // prior: Forced (Failure/Cancelled)
        (Some(RaiseChoice::Forced), RaiseChoice::Requested, false),
        (Some(RaiseChoice::Forced), RaiseChoice::Forced, true),
        (Some(RaiseChoice::Forced), RaiseChoice::Kill, true),
        // prior: Kill
        (Some(RaiseChoice::Kill), RaiseChoice::Requested, false),
        (Some(RaiseChoice::Kill), RaiseChoice::Forced, false),
        (Some(RaiseChoice::Kill), RaiseChoice::Kill, true),
    ];

    // - Rank, kill-only and sealed rows ---------------------------------------

    /// How a row's lane is prepared before the request under test.
    #[derive(Debug, Clone, Copy)]
    enum Mode {
        Open,
        KillOnly,
        Sealed,
    }

    fn failure(text: &str) -> StopReason {
        StopReason::Failure(ActorError::user(text))
    }

    /// Prepares a lane holding `stored` in `mode`, raises `new`, and returns
    /// the answer, the generation change and the stored reason.
    fn row(
        stored: Option<StopReason>,
        mode: Mode,
        new: StopReason,
    ) -> (RaiseResult, u64, Option<StopReason>) {
        let (lane, rx) = StopLane::new();
        if let Some(stored) = stored {
            lane.raise(stored);
        }
        match mode {
            Mode::Open => {}
            Mode::KillOnly => lane.close_to_non_kill(),
            Mode::Sealed => {
                lane.close_to_non_kill();
                lane.seal();
            }
        }
        let before = rx.borrow().generation;
        let answer = lane.try_raise(new);
        let state = rx.borrow();
        (
            answer,
            state.generation.wrapping_sub(before),
            state.reason.clone(),
        )
    }

    #[test]
    fn rank_inside_a_severity_keeps_the_higher_request_and_always_bumps() {
        // A parent's request above a graceful one, in both orders.
        let (answer, bump, kept) = row(
            Some(StopReason::ParentRequest),
            Mode::Open,
            StopReason::Graceful,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::ParentRequest)), "{kept:?}");
        let (answer, bump, kept) = row(
            Some(StopReason::Graceful),
            Mode::Open,
            StopReason::ParentRequest,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::ParentRequest)), "{kept:?}");
        // A failure above a cancellation, in both orders.
        let (answer, bump, kept) = row(Some(failure("f")), Mode::Open, StopReason::Cancelled);
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Failure(_))), "{kept:?}");
        let (answer, bump, kept) = row(Some(StopReason::Cancelled), Mode::Open, failure("f"));
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Failure(_))), "{kept:?}");
        // Between two failures the newer is stored.
        let (_, bump, kept) = row(Some(failure("old")), Mode::Open, failure("new"));
        assert_eq!(bump, 1);
        assert!(
            matches!(&kept, Some(StopReason::Failure(err)) if err.to_string().contains("new")),
            "{kept:?}"
        );
        // A strictly lower severity is superseded without a bump.
        let (answer, bump, kept) = row(Some(failure("f")), Mode::Open, StopReason::ParentRequest);
        assert_eq!((answer, bump), (RaiseResult::Superseded, 0));
        assert!(matches!(kept, Some(StopReason::Failure(_))), "{kept:?}");
    }
}

#[cfg(test)]
mod cell_tests {
    use std::future::Future;

    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    use std::time::Duration;

    use super::*;
    use crate::actor::Actor;
    use crate::error::{ActorError, ActorResult};

    use crate::types::Shutdown;
    use crate::ActorContext;

    /// A waker that counts its wakes.
    #[derive(Default)]
    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Counter {
        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn poll_once<F: Future>(fut: &mut Pin<Box<F>>, waker: &Waker) -> Poll<F::Output> {
        fut.as_mut().poll(&mut Context::from_waker(waker))
    }

    /// A waker that probes the lane's lock: it records whether the lock was
    /// free when the wake ran.
    struct LockProbe {
        record: AbortRecord,
        free: AtomicUsize,
        held: AtomicUsize,
    }

    impl LockProbe {
        fn probe(&self) {
            match self.record.lane().state.try_lock() {
                Ok(_) => self.free.fetch_add(1, Ordering::SeqCst),
                Err(_) => self.held.fetch_add(1, Ordering::SeqCst),
            };
        }
    }

    impl Wake for LockProbe {
        fn wake(self: Arc<Self>) {
            self.probe();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.probe();
        }
    }

    /// A waker that counts its wakes and panics in them while it is armed.
    #[derive(Default)]
    struct LoudWake {
        armed: AtomicBool,
        woken: AtomicUsize,
    }

    impl LoudWake {
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

    // - A supervisor killed in its teardown ---------------------------------

    /// A child that vetoes every graceful stop.
    struct Vetoer {
        pre_stops: Arc<AtomicUsize>,
    }

    impl Actor for Vetoer {
        type Message = ();
        type Response = ();

        async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }

        fn pre_stop(
            &mut self,
            _reason: &StopReason,
            _ctx: &mut ActorContext<Self>,
        ) -> impl Future<Output = bool> + Send {
            self.pre_stops.fetch_add(1, Ordering::SeqCst);
            async { false }
        }
    }

    /// A supervisor of one [`Vetoer`] with a two-hour shutdown window.
    struct Boss {
        pre_stops: Arc<AtomicUsize>,
    }

    impl Actor for Boss {
        type Message = ();
        type Response = ();

        async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            let pre_stops = self.pre_stops.clone();
            ctx.spawn_child(move || Vetoer {
                pre_stops: pre_stops.clone(),
            })
            .named("vetoer")
            .shutdown(Shutdown::Timeout(Duration::from_secs(2 * 3600)))
            .kill_grace(Duration::from_secs(3600))
            .await
            .map_err(|err| ActorError::user(err.to_string()))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod cause_rule_tests {
    use super::*;
    use crate::error::ActorError;

    fn failure(text: &str) -> StopReason {
        StopReason::Failure(ActorError::user(text))
    }

    fn held() -> StopReason {
        StopReason::Shutdown("held".into())
    }

    fn incoming() -> StopReason {
        StopReason::ParentRequest
    }

    #[derive(Clone, Copy, Debug)]
    enum Pending {
        Nothing,
        Failure,
        Kill,
        KillWithCause,
    }

    #[derive(Clone, Copy, Debug)]
    enum Incoming {
        CausedKill,
        PlainKill,
        Failure,
        Graceful,
    }

    fn lane_with(pending: Pending) -> StopLane {
        let state = match pending {
            Pending::Nothing => LaneState::default(),
            Pending::Failure => LaneState {
                reason: Some(failure("pending")),
                ..Default::default()
            },
            Pending::Kill => LaneState {
                reason: Some(StopReason::Kill),
                ..Default::default()
            },
            Pending::KillWithCause => LaneState {
                reason: Some(StopReason::Kill),
                kill_cause: Some(Box::new(held())),
                ..Default::default()
            },
        };
        let (lane, rx) = StopLane::with_state(state);
        // The receiver stays alive for the lane's life in these rows.
        std::mem::forget(rx);
        lane
    }
}
