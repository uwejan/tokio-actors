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
use std::sync::{Mutex, MutexGuard};
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
    /// Vetoable via `pre_stop`: [`StopReason::Graceful`], [`StopReason::ParentRequest`].
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
            StopReason::Graceful | StopReason::ParentRequest => StopSeverity::Requested,
            StopReason::Failure(_) | StopReason::ParentKilled | StopReason::Cancelled => {
                StopSeverity::Forced
            }
            StopReason::Kill => StopSeverity::Kill,
        }
    }

    /// The order of two requests of the same severity: a parent's request
    /// outranks a plain graceful one, and a failure outranks an inherited
    /// stop, which outranks a cancellation.
    pub(crate) fn rank(reason: &StopReason) -> u8 {
        match reason {
            StopReason::Graceful | StopReason::Cancelled | StopReason::Kill => 0,
            StopReason::ParentRequest | StopReason::ParentKilled => 1,
            StopReason::Failure(_) => 2,
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
#[derive(Debug, Clone, Default)]
pub(crate) struct LaneState {
    pub(crate) reason: Option<StopReason>,
    pub(crate) generation: u64,
    pub(crate) kill_only: bool,
    pub(crate) sealed: bool,
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

    /// Decides and writes one raise in a single critical section. The
    /// receiver-gone flag is read inside it.
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
    /// Creates a fresh, unraised lane and the run loop's receiving half.
    pub(crate) fn new() -> (StopLane, LaneRx) {
        let aborted = AbortRecord::new();
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
        let raised = self.aborted.lane().raise(reason);
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
    pub(crate) fn seal(&self) -> Option<StopReason> {
        let mut section = self.aborted.lane().section();
        section.sealed = true;
        section.reason.clone()
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

    #[test]
    fn first_raise_is_always_accepted() {
        let (lane, rx) = StopLane::new();
        lane.raise(StopReason::Graceful);
        let state = rx.borrow();
        assert_eq!(severity_of(&state), Some(StopSeverity::Requested));
        assert_eq!(state.generation, 1);
    }

    #[test]
    fn higher_severity_overwrites_lower() {
        let (lane, rx) = StopLane::new();
        lane.raise(StopReason::Graceful);
        lane.raise(StopReason::Kill);
        let state = rx.borrow();
        assert_eq!(severity_of(&state), Some(StopSeverity::Kill));
        assert_eq!(state.generation, 2);
    }

    #[test]
    fn lower_severity_never_displaces_a_higher_one() {
        let (lane, rx) = StopLane::new();
        lane.raise(StopReason::Kill);
        lane.raise(StopReason::Graceful);
        lane.raise(StopReason::Failure(ActorError::user("x")));
        let state = rx.borrow();
        assert_eq!(
            severity_of(&state),
            Some(StopSeverity::Kill),
            "Kill must never be downgraded by a later, weaker raise"
        );
        assert_eq!(
            state.generation, 1,
            "a rejected (lower-severity) raise must not bump the generation"
        );
    }

    #[test]
    fn equal_severity_reraise_bumps_generation_without_changing_tier() {
        let (lane, rx) = StopLane::new();
        lane.raise(StopReason::Graceful);
        lane.raise(StopReason::ParentRequest);
        let state = rx.borrow();
        assert_eq!(severity_of(&state), Some(StopSeverity::Requested));
        assert_eq!(
            state.generation, 2,
            "same-tier re-raise must still bump the generation for re-observation"
        );
        assert!(matches!(state.reason, Some(StopReason::ParentRequest)));
    }

    #[tokio::test]
    async fn raise_after_receiver_dropped_is_a_harmless_no_op() {
        let (lane, rx) = StopLane::new();
        drop(rx);
        assert!(lane.is_closed());
        lane.raise(StopReason::Kill); // must not panic
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

    #[test]
    fn raise_table_is_monotone() {
        for &(prior, raised, accepted) in RAISE_TABLE.iter() {
            let (lane, mut rx) = StopLane::new();
            if let Some(prior) = prior {
                lane.raise(prior.reason());
            }
            let generation_before = rx.borrow_and_update().generation;

            lane.raise(raised.reason());

            // An accepted raise is a new version to observe, a rejected one
            // is not, and the version follows the generation.
            assert_eq!(
                rx.has_changed(),
                Ok(accepted),
                "prior {prior:?} raised {raised:?}"
            );
            assert_eq!(rx.record.lane().version(), rx.borrow().generation);
            let state = rx.borrow();
            if accepted {
                assert_eq!(
                    state.generation,
                    generation_before.wrapping_add(1),
                    "prior {:?} raised {:?} should be accepted",
                    prior,
                    raised
                );
                assert!(
                    raised.matches_stored(&state.reason),
                    "prior {:?} raised {:?}: stored reason should become the raised one, got {:?}",
                    prior,
                    raised,
                    state.reason
                );
            } else {
                let prior = prior.expect("a rejected raise always has a prior");
                assert_eq!(
                    state.generation, generation_before,
                    "prior {:?} raised {:?} should be rejected",
                    prior, raised
                );
                assert!(
                    prior.matches_stored(&state.reason),
                    "prior {:?} raised {:?}: stored reason should stay unchanged, got {:?}",
                    prior,
                    raised,
                    state.reason
                );
            }
        }
    }

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

    #[test]
    fn an_inherited_stop_ranks_between_a_failure_and_a_cancellation() {
        // A failure keeps its place over a later inherited stop, and an
        // inherited stop is replaced by a later failure: every accepted
        // raise bumps the generation.
        let (answer, bump, kept) = row(Some(failure("f")), Mode::Open, StopReason::ParentKilled);
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Failure(_))), "{kept:?}");
        let (answer, bump, kept) = row(Some(StopReason::ParentKilled), Mode::Open, failure("f"));
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Failure(_))), "{kept:?}");
        // An inherited stop keeps its place over a later cancellation, and
        // replaces an earlier one.
        let (answer, bump, kept) = row(
            Some(StopReason::ParentKilled),
            Mode::Open,
            StopReason::Cancelled,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::ParentKilled)), "{kept:?}");
        let (answer, bump, kept) = row(
            Some(StopReason::Cancelled),
            Mode::Open,
            StopReason::ParentKilled,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::ParentKilled)), "{kept:?}");
        // Two inherited stops: the newer is stored and the generation moves.
        let (answer, bump, kept) = row(
            Some(StopReason::ParentKilled),
            Mode::Open,
            StopReason::ParentKilled,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::ParentKilled)), "{kept:?}");
        // It replaces a vetoable request, and a vetoable request after it is
        // superseded without a bump.
        for requested in [StopReason::Graceful, StopReason::ParentRequest] {
            let (answer, bump, kept) = row(
                Some(requested.clone()),
                Mode::Open,
                StopReason::ParentKilled,
            );
            assert_eq!((answer, bump), (RaiseResult::Recorded, 1), "{requested:?}");
            assert!(matches!(kept, Some(StopReason::ParentKilled)), "{kept:?}");
            let (answer, bump, kept) = row(
                Some(StopReason::ParentKilled),
                Mode::Open,
                requested.clone(),
            );
            assert_eq!(
                (answer, bump),
                (RaiseResult::Superseded, 0),
                "{requested:?}"
            );
            assert!(matches!(kept, Some(StopReason::ParentKilled)), "{kept:?}");
        }
        // Kill-only and sealed lanes refuse it without a bump, and a Kill is
        // accepted over a stored one while kill-only.
        let (answer, bump, kept) = row(None, Mode::KillOnly, StopReason::ParentKilled);
        assert_eq!((answer, bump), (RaiseResult::Refused, 0));
        assert!(kept.is_none(), "{kept:?}");
        let (answer, bump, kept) = row(None, Mode::Sealed, StopReason::ParentKilled);
        assert_eq!((answer, bump), (RaiseResult::Refused, 0));
        assert!(kept.is_none(), "{kept:?}");
        let (answer, bump, kept) = row(
            Some(StopReason::ParentKilled),
            Mode::KillOnly,
            StopReason::Kill,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Kill)), "{kept:?}");
    }

    #[test]
    fn kill_only_lane_takes_a_kill_and_nothing_else() {
        for new in [
            StopReason::Graceful,
            StopReason::ParentRequest,
            StopReason::ParentKilled,
            StopReason::Cancelled,
            failure("f"),
        ] {
            let (answer, bump, kept) = row(None, Mode::KillOnly, new.clone());
            assert_eq!((answer, bump), (RaiseResult::Refused, 0), "{new:?}");
            assert!(kept.is_none(), "{new:?} stored {kept:?}");
        }
        let (answer, bump, kept) =
            row(Some(StopReason::Graceful), Mode::KillOnly, StopReason::Kill);
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Kill)), "{kept:?}");
        // A pending request does not change what the lane refuses.
        let (answer, bump, _) = row(
            Some(StopReason::Graceful),
            Mode::KillOnly,
            StopReason::ParentRequest,
        );
        assert_eq!((answer, bump), (RaiseResult::Refused, 0));
    }

    #[test]
    fn refuses_answers_as_a_raise_would_and_writes_nothing() {
        let (lane, rx) = StopLane::new();
        assert!(!lane.refuses(&StopReason::Graceful));
        assert!(!lane.refuses(&StopReason::Kill));
        assert!(lane.reason().is_none(), "the check wrote nothing");
        assert_eq!(rx.borrow().generation, 0, "the check moved no version");
        lane.close_to_non_kill();
        assert!(lane.refuses(&StopReason::Graceful));
        assert!(!lane.refuses(&StopReason::Kill));
        let _ = lane.seal();
        assert!(lane.refuses(&StopReason::Kill));
        let (gone, rx) = StopLane::new();
        drop(rx);
        assert!(gone.refuses(&StopReason::Kill));
        assert!(gone.refuses(&StopReason::Graceful));
    }

    #[test]
    fn sealed_lane_refuses_everything() {
        for new in [
            StopReason::Kill,
            StopReason::Graceful,
            StopReason::ParentKilled,
            StopReason::Cancelled,
            failure("f"),
        ] {
            let (answer, bump, kept) = row(None, Mode::Sealed, new.clone());
            assert_eq!((answer, bump), (RaiseResult::Refused, 0), "{new:?}");
            assert!(kept.is_none(), "{new:?} stored {kept:?}");
        }
    }

    #[test]
    fn lane_without_a_receiver_refuses_everything() {
        for new in [
            StopReason::Kill,
            StopReason::Graceful,
            StopReason::ParentRequest,
            StopReason::ParentKilled,
            StopReason::Cancelled,
            failure("f"),
        ] {
            let (lane, rx) = StopLane::new();
            drop(rx);
            assert_eq!(lane.try_raise(new.clone()), RaiseResult::Refused, "{new:?}");
            assert!(lane.reason().is_none(), "{new:?} was stored");
        }
    }

    #[test]
    fn seal_is_idempotent_and_works_with_no_receiver() {
        let (lane, rx) = StopLane::new();
        lane.raise(StopReason::Graceful);
        assert!(matches!(lane.seal(), Some(StopReason::Graceful)));
        assert!(matches!(lane.seal(), Some(StopReason::Graceful)));
        drop(rx);
        assert!(matches!(lane.seal(), Some(StopReason::Graceful)));
        let (lane, rx) = StopLane::new();
        drop(rx);
        assert!(lane.seal().is_none());
        assert!(lane.is_closed());
    }

    #[test]
    fn try_raise_decides_and_writes_in_one_step() {
        // Whenever a raise of `Kill` is recorded, the seal that races it reads
        // that `Kill`: the decision and the write cannot be separated by it.
        for _ in 0..2_000 {
            let (lane, _rx) = StopLane::new();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let raiser = {
                let (lane, barrier) = (lane.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    lane.try_raise(StopReason::Kill)
                })
            };
            barrier.wait();
            let pending = lane.seal();
            let answer = raiser.join().unwrap();
            match answer {
                RaiseResult::Recorded => assert!(
                    matches!(pending, Some(StopReason::Kill)),
                    "a recorded Kill must be read by the seal, got {pending:?}"
                ),
                RaiseResult::Refused => assert!(pending.is_none(), "{pending:?}"),
                RaiseResult::Superseded => panic!("nothing is pending to supersede a Kill"),
            }
        }
    }

    #[test]
    fn generation_wraps_instead_of_overflowing() {
        let (lane, rx) = StopLane::with_state(LaneState {
            reason: None,
            generation: u64::MAX,
            ..Default::default()
        });

        lane.raise(StopReason::Graceful);

        let state = rx.borrow();
        assert_eq!(state.generation, 0);
        assert!(matches!(state.reason, Some(StopReason::Graceful)));
    }
}

#[cfg(test)]
mod cell_tests {
    use std::future::Future;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    use std::time::Duration;

    use super::*;
    use crate::actor::{Actor, ActorExt};
    use crate::error::{ActorError, ActorResult};
    use crate::system::ActorSystem;
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

    #[test]
    fn a_raise_wakes_a_parked_lane_changed() {
        let (lane, mut rx) = StopLane::new();
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        {
            let mut parked = Box::pin(rx.changed());
            assert!(poll_once(&mut parked, &waker).is_pending());
            assert_eq!(counter.count(), 0);
            lane.raise(StopReason::Graceful);
            assert_eq!(counter.count(), 1, "the raise woke the parked wait");
            assert_eq!(poll_once(&mut parked, &waker), Poll::Ready(Ok(())));
        }
        assert_eq!(
            rx.has_changed(),
            Ok(false),
            "the wait marked the version seen"
        );
    }

    #[test]
    fn a_raise_wakes_the_run_loops_slot_and_a_rejected_one_wakes_nothing() {
        let (lane, _rx) = StopLane::new();
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        lane.abort_record().reader().register(&waker);
        assert_eq!(lane.try_raise(StopReason::Kill), RaiseResult::Recorded);
        assert_eq!(counter.count(), 1);
        // Superseded, refused, closing and sealing do not wake.
        lane.abort_record().reader().register(&waker);
        assert_eq!(
            lane.try_raise(StopReason::Graceful),
            RaiseResult::Superseded
        );
        lane.close_to_non_kill();
        assert_eq!(lane.try_raise(StopReason::Cancelled), RaiseResult::Refused);
        assert!(matches!(lane.seal(), Some(StopReason::Kill)));
        assert_eq!(lane.try_raise(StopReason::Kill), RaiseResult::Refused);
        assert_eq!(counter.count(), 1, "only the accepted raise woke the slot");
    }

    #[test]
    fn a_borrow_never_marks_a_version_seen() {
        let (lane, mut rx) = StopLane::new();
        lane.raise(StopReason::Graceful);
        drop(rx.borrow());
        assert_eq!(
            rx.has_changed(),
            Ok(true),
            "borrow leaves the version unseen"
        );
        drop(rx.borrow_and_update());
        assert_eq!(rx.has_changed(), Ok(false));
        // A same-severity re-raise after the read is a new version.
        lane.raise(StopReason::Graceful);
        assert_eq!(rx.has_changed(), Ok(true));
        let waker = Waker::from(Arc::new(Counter::default()));
        let mut changed = Box::pin(rx.changed());
        assert_eq!(poll_once(&mut changed, &waker), Poll::Ready(Ok(())));
        drop(changed);
        assert_eq!(rx.has_changed(), Ok(false));
    }

    #[test]
    fn the_lane_reports_a_gone_receiver_inside_the_raise_and_not_before() {
        let (lane, rx) = StopLane::new();
        assert!(!lane.is_closed());
        drop(rx);
        assert!(lane.is_closed());
        assert_eq!(lane.try_raise(StopReason::Kill), RaiseResult::Refused);
        assert_eq!(
            lane.abort_record().lane().version(),
            0,
            "a refusal moves no version"
        );
    }

    #[test]
    fn the_operations_take_the_lock_exactly_as_documented() {
        let (lane, mut rx) = StopLane::new();
        let sections = |f: &mut dyn FnMut()| {
            let before = held::sections();
            f();
            held::sections() - before
        };
        assert_eq!(
            sections(&mut || {
                let _ = lane.try_raise(StopReason::Graceful);
            }),
            1
        );
        assert_eq!(
            sections(&mut || drop(lane.reason())),
            1,
            "reason reads under the lock"
        );
        lane.raise(StopReason::Kill);
        assert_eq!(
            sections(&mut || drop(lane.reason())),
            1,
            "a pending Kill is read under the lock too"
        );
        assert_eq!(sections(&mut || drop(rx.borrow())), 1);
        assert_eq!(sections(&mut || drop(rx.borrow_and_update())), 1);
        assert_eq!(sections(&mut || drop(lane.seal())), 1);
        assert_eq!(sections(&mut || lane.close_to_non_kill()), 1);
        assert_eq!(
            sections(&mut || {
                let _ = rx.has_changed();
            }),
            0,
            "has_changed takes no lock"
        );
        assert_eq!(
            sections(&mut || {
                let _ = lane.is_closed();
            }),
            1
        );
    }

    #[test]
    #[should_panic(expected = "re-entrant lock")]
    fn a_held_guard_makes_a_second_call_on_the_lane_panic_instead_of_hanging() {
        let (lane, rx) = StopLane::new();
        let _held = rx.borrow();
        let _ = lane.reason();
    }

    #[test]
    #[should_panic(expected = "a wake ran while a lane lock was held")]
    fn the_wake_assertion_fires_under_a_held_lock() {
        let (lane_a, rx_a) = StopLane::new();
        let (lane_b, _rx_b) = StopLane::new();
        let _held = rx_a.borrow();
        let _ = &lane_a;
        lane_b.raise(StopReason::Graceful);
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

    #[test]
    fn no_wake_runs_while_the_lanes_lock_is_held() {
        let (lane, mut rx) = StopLane::new();
        let probe = Arc::new(LockProbe {
            record: lane.abort_record().clone(),
            free: AtomicUsize::new(0),
            held: AtomicUsize::new(0),
        });
        let waker = Waker::from(probe.clone());
        // Both wakers of a raise: the run loop's slot and a parked waiter.
        lane.abort_record().reader().register(&waker);
        let mut parked = Box::pin(rx.changed());
        assert!(poll_once(&mut parked, &waker).is_pending());
        lane.raise(StopReason::Graceful);
        assert_eq!(
            probe.held.load(Ordering::SeqCst),
            0,
            "a wake ran under the lock"
        );
        assert_eq!(probe.free.load(Ordering::SeqCst), 2, "both wakers ran");
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

    /// The run loop's waker belongs to the loop's task, not to the raiser. A
    /// wake of it that panics stays out of the raise: the request is recorded,
    /// the raise returns what the lane answered, and the waiters of the lane's
    /// signals are woken all the same.
    #[test]
    fn a_raise_whose_run_loop_waker_panics_still_records_and_wakes_the_signals() {
        let (lane, mut rx) = StopLane::new();
        let loud = Arc::new(LoudWake::default());
        lane.abort_record()
            .reader()
            .register(&Waker::from(loud.clone()));
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut parked = Box::pin(rx.changed());
        assert!(poll_once(&mut parked, &waker).is_pending());
        loud.arm(true);
        let raised = catch_unwind(AssertUnwindSafe(|| lane.try_raise(StopReason::Graceful)));
        loud.arm(false);
        assert_eq!(
            raised.ok(),
            Some(RaiseResult::Recorded),
            "the loop's waker unwound out of the raise"
        );
        assert_eq!(loud.woken(), 1, "the loop's waker was woken once");
        assert_eq!(counter.count(), 1, "the signals' waiter is woken after it");
        assert!(matches!(lane.reason(), Some(StopReason::Graceful)));
        assert_eq!(poll_once(&mut parked, &waker), Poll::Ready(Ok(())));
    }

    /// A waiter's waker belongs to the waiter's task. A wake of it that panics
    /// stays out of the raise: the request is recorded and the raise returns.
    #[test]
    fn a_raise_whose_signals_waiter_waker_panics_still_records_and_returns() {
        let (lane, mut rx) = StopLane::new();
        let counter = Arc::new(Counter::default());
        lane.abort_record()
            .reader()
            .register(&Waker::from(counter.clone()));
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut parked = Box::pin(rx.changed());
        assert!(poll_once(&mut parked, &waker).is_pending());
        loud.arm(true);
        let raised = catch_unwind(AssertUnwindSafe(|| lane.try_raise(StopReason::Kill)));
        loud.arm(false);
        assert_eq!(
            raised.ok(),
            Some(RaiseResult::Recorded),
            "the waiter's waker unwound out of the raise"
        );
        assert_eq!(loud.woken(), 1, "the waiter's waker was woken once");
        assert_eq!(counter.count(), 1, "the run loop was woken");
        assert!(matches!(lane.reason(), Some(StopReason::Kill)));
        assert_eq!(poll_once(&mut parked, &waker), Poll::Ready(Ok(())));
    }

    #[tokio::test(start_paused = true)]
    async fn a_raise_between_the_read_and_the_wait_is_not_lost() {
        let (lane, mut rx) = StopLane::new();
        let raiser = lane.clone();
        hook::arm(move || raiser.raise(StopReason::Graceful));
        // The hook raises after the wait read nothing unseen and before it
        // parks: the wait must still return.
        tokio::time::timeout(Duration::from_secs(1), rx.changed())
            .await
            .expect("the wait saw the raise made in its window")
            .unwrap();
        assert!(matches!(rx.borrow().reason, Some(StopReason::Graceful)));
    }

    #[test]
    fn a_reader_never_marks_a_version_seen_whose_value_it_did_not_read() {
        // The raiser keeps raising at one severity, so every raise is a new
        // version; whatever the reader reads, the version it marked seen is
        // the generation of the value it read.
        const RAISES: u64 = 60_000;
        let (lane, mut rx) = StopLane::new();
        let done = Arc::new(AtomicBool::new(false));
        let raiser = {
            let (lane, done) = (lane.clone(), done.clone());
            std::thread::spawn(move || {
                for _ in 0..RAISES {
                    lane.raise(StopReason::ParentRequest);
                }
                done.store(true, Ordering::Release);
            })
        };
        let mut reads = 0u64;
        loop {
            let read = rx.borrow_and_update().generation;
            assert_eq!(rx.seen, read, "seen is the version of the value read");
            // Read under the lock, so no raise lands between the two reads.
            let guard = rx.borrow();
            if rx.has_changed() == Ok(false) {
                assert_eq!(
                    guard.generation, read,
                    "no change is reported only when the value is the one read"
                );
            }
            drop(guard);
            reads += 1;
            // Ends once the raiser is done and the last version was read.
            if done.load(Ordering::Acquire) && rx.has_changed() == Ok(false) {
                break;
            }
            std::thread::yield_now();
        }
        raiser.join().unwrap();
        assert!(reads > 0);
        assert_eq!(rx.record.lane().version(), RAISES);
        assert_eq!(rx.seen, RAISES);
    }

    #[test]
    fn a_reader_that_consumed_a_version_reads_a_state_not_older_than_it() {
        // Every raise stores a reason numbered like its version, so the
        // reason read after a version was consumed without the lock must
        // carry at least that number.
        const RAISES: u64 = 60_000;
        fn number(state: &LaneState) -> u64 {
            match &state.reason {
                Some(StopReason::Failure(err)) => err
                    .to_string()
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .expect("a numbered reason"),
                other => panic!("a version was consumed but the state holds {other:?}"),
            }
        }
        let (lane, mut rx) = StopLane::new();
        let done = Arc::new(AtomicBool::new(false));
        let raiser = {
            let (lane, done) = (lane.clone(), done.clone());
            std::thread::spawn(move || {
                for n in 1..=RAISES {
                    lane.raise(StopReason::Failure(ActorError::user(n.to_string())));
                }
                done.store(true, Ordering::Release);
            })
        };
        loop {
            let finished = done.load(Ordering::Acquire);
            let consumed = rx.record.lane().version();
            if consumed == rx.seen {
                // Nothing unseen once the raiser is done means every version
                // was consumed.
                if finished {
                    break;
                }
                std::thread::yield_now();
                continue;
            }
            {
                let state = rx.borrow();
                assert!(
                    state.generation >= consumed,
                    "the generation is behind the version"
                );
                assert!(
                    number(&state) >= consumed,
                    "the reason is older than the version"
                );
            }
            drop(rx.borrow_and_update());
        }
        raiser.join().unwrap();
        assert_eq!(rx.seen, RAISES, "every version was consumed");
    }

    #[test]
    fn the_lane_types_keep_their_thread_bounds() {
        trait AmbiguousIfSend<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
        fn is_send<T: Send>() {}
        fn is_send_val<T: Send>(_: &T) {}
        fn is_sync<T: Sync>() {}
        is_send::<LaneRx>();
        is_sync::<StopLane>();
        is_send::<StopLane>();
        // A guard of the lane's lock is not `Send`: the line below does not
        // compile if `LaneRef` becomes `Send`.
        let _ = <LaneRef<'static> as AmbiguousIfSend<_>>::check;
        let (_lane, mut rx) = StopLane::new();
        is_send_val(&rx.changed());
    }

    #[test]
    fn a_poisoned_lock_is_ignored() {
        let (lane, mut rx) = StopLane::new();
        let poisoner = {
            let lane = lane.clone();
            std::thread::spawn(move || {
                let _section = lane.abort_record().lane().section();
                panic!("poison the lane's lock");
            })
        };
        assert!(poisoner.join().is_err());
        assert!(lane.abort_record().lane().state.is_poisoned());
        assert_eq!(lane.try_raise(StopReason::Graceful), RaiseResult::Recorded);
        assert!(matches!(lane.reason(), Some(StopReason::Graceful)));
        assert!(matches!(
            rx.borrow_and_update().reason,
            Some(StopReason::Graceful)
        ));
        assert!(matches!(lane.seal(), Some(StopReason::Graceful)));
    }

    #[test]
    fn a_handle_keeps_its_thread_and_unwind_bounds() {
        fn bounds<T: Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}
        bounds::<crate::actor::handle::ActorHandle<Boss>>();
        bounds::<StopLane>();
    }

    #[test]
    fn the_stop_lane_is_one_pointer() {
        assert_eq!(std::mem::size_of::<StopLane>(), 8);
        assert_eq!(std::mem::size_of::<LaneRx>(), 16);
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

    /// A `Kill` raised on the lane of a supervisor that is waiting for a
    /// child to stop ends the wait through the lane's own wake: no backstop
    /// is armed (the raise goes to the lane, not through a kill entry point),
    /// no abort is issued, and the child's two-hour window is not waited out.
    #[tokio::test(start_paused = true)]
    async fn a_kill_ends_a_childrens_teardown_without_an_abort() {
        let system = ActorSystem::create(format!("lane-kill-{}", uuid::Uuid::new_v4())).unwrap();
        let pre_stops = Arc::new(AtomicUsize::new(0));
        let boss = Boss {
            pre_stops: pre_stops.clone(),
        }
        .spawn()
        .named(format!("boss-{}", uuid::Uuid::new_v4()))
        .on_system(&system)
        .supervisor()
        .kill_grace(Duration::from_secs(3600))
        .await
        .expect("the boss starts");
        boss.send(()).await.expect("the child starts");
        boss.stop(StopReason::Graceful).await.expect("accepted");
        tokio::time::timeout(Duration::from_secs(60), async {
            while pre_stops.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the vetoing child ran its first pre_stop");
        let started = tokio::time::Instant::now();
        let lane = boss.stop_lane();
        assert_eq!(lane.try_raise(StopReason::Kill), RaiseResult::Recorded);
        let reason = tokio::time::timeout(Duration::from_secs(600), boss.wait_stopped())
            .await
            .expect("the supervisor ends");
        assert_eq!(reason, StopReason::Kill);
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the teardown did not wait out the child's window: {:?}",
            started.elapsed()
        );
        assert_eq!(
            lane.abort_record().view(),
            (None, false),
            "the supervisor ended with no abort issued or owed"
        );
    }
}
