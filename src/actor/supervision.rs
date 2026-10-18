//! Supervision configuration, restart budgets, and child registry.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio::time::Instant;

use crate::actor::runtime::saturating_deadline;
use crate::error::ActorError;
use crate::system::{ActorSystem, ReaperEntry, ReaperFeed};
use crate::types::{
    ActorId, ChildEvent, ChildInfo, ChildStoppedInternal, RestartStrategy, RestartType, Shutdown,
    StopLane, StopReason, SupervisionAction,
};

pub(crate) mod rules;
mod slot;

pub(crate) use rules::{
    manual_commit, CommitEffect, GroupMembership, ManualFacts, ManualOp, ManualReply, ReportClass,
    Route, TaskState,
};
#[cfg(test)]
pub(crate) use slot::joined_reason;
#[cfg(test)]
pub(crate) use slot::slot_pair;
pub(crate) use slot::{
    await_cell, await_predecessor, await_subtree, cancelled_before_factory, join_error_panic,
    run_attempt, run_ladder, slot_pair_for, Attempt, CellOutcome, CellWatch, OwnStop, Predecessor,
    SlotCtl, SlotIo, Verdicts,
};

/// Type-erased restart function stored per child.
///
/// The closure captures the child's original [`ActorId`], name, and the full
/// resolved [`ActorConfig`](crate::actor::runtime::ActorConfig) by value at
/// `spawn_child` time (OTP child-spec immutability), so every restart reuses
/// the exact spec. Given the restart sequence token and the executor half of
/// the attempt's slot, it produces the fresh
/// attempt's [`RestartOutcome`] - the value the parent's restart plane
/// (`SupervisionState::restart_set`) returns to the run loop once the
/// attempt is decided one way or the other. The third argument is the
/// subtree of the child's most recent incarnation, which the attempt waits
/// for before it builds anything.
pub(crate) type RestartFn = Box<
    dyn Fn(u64, SlotIo, Option<Predecessor>) -> Pin<Box<dyn Future<Output = RestartOutcome> + Send>>
        + Send
        + Sync,
>;

/// Outcome of one restart attempt, produced by a task in the parent's
/// restart plane (`SupervisionState::restart_set`) and consumed by the run
/// loop.
///
/// [`Adopted`](Self::Adopted) carries every per-incarnation handle the
/// adoption path needs (the runner and its link guard are rebuilt from
/// these) - including the [`ChildLinkGuard`] itself, armed the instant the
/// attempt's `spawn_actor` call succeeded, inside the restart task, before
/// the child's init ack was ever awaited. Not adopting an `Adopted` value
/// (a superseded attempt, a rejected chain slot, or the whole restart plane
/// being dropped because the parent itself died) is exactly what leaves that
/// guard to run its `Drop`, killing the stray incarnation through the same
/// Kill-raise/reaper ladder every other child teardown uses.
///
/// [`Failed`](Self::Failed) covers every attempt that never produced a live
/// child to adopt: a factory panic (nothing was ever spawned), a spawn
/// error, an init failure or panic (the fresh incarnation's own task has
/// already fully exited by the time this is reported), or a `start_timeout`
/// expiry (the hung incarnation's guard was already dropped - and its real
/// join already awaited - before this was produced). It carries the same
/// shape [`ChildStoppedInternal`] does and is routed through the identical
/// failure path: budget-charged strategy evaluation, exactly like any other
/// child death.
pub(crate) enum RestartOutcome {
    /// The fresh incarnation passed its init ack and is ready to become the
    /// child's live instance.
    Adopted {
        child_id: ActorId,
        incarnation: u64,
        new_stop_lane: StopLane,
        new_join: JoinHandle<StopReason>,
        guard: ChildLinkGuard,
        /// The executor half of the attempt's slot, handed to the runner of
        /// the adopted incarnation.
        io: SlotIo,
    },
    /// The attempt never produced a live child (or the live child it did
    /// produce is already known fully gone by the time this is reported).
    Failed {
        child_id: ActorId,
        incarnation: u64,
        reason: StopReason,
    },
    /// The attempt honored a stop order before it called its factory, or
    /// found its child's spec removed: no incarnation existed. When the spec
    /// went while the attempt built, the attempt dropped the factory's last
    /// reference, and `drop_panic` is the panic that drop raised, if any.
    Cancelled {
        child_id: ActorId,
        incarnation: u64,
        drop_panic: Option<String>,
    },
    /// The attempt's wait for the previous incarnation's subtree outlived its
    /// bound: no incarnation existed, and none is started beside the actors
    /// that still run.
    PredecessorUnresponsive { child_id: ActorId, incarnation: u64 },
    /// The attempt's incarnation ended under a stop order and was never
    /// adopted; `reason` is how it ended.
    Stopped {
        child_id: ActorId,
        incarnation: u64,
        reason: StopReason,
    },
}

/// The time an actor is given, after a `Kill` is raised on it, to reach its
/// next turn boundary before its task is aborted, when its spawn builder set
/// no other.
pub(crate) const DEFAULT_KILL_GRACE: Duration = Duration::from_millis(120);

/// Wait after an abort before the Unresponsive verdict: a task that yields
/// ends at the abort, so only a non-yielding task outlives it. Independent of
/// any kill grace.
pub(crate) const ABORT_SETTLE: Duration = Duration::from_millis(100);

/// Outcome of one child incarnation's runner task: the child's id, its real
/// stop reason, and the incarnation token - in the order the run loop's
/// death-plane branch consumes them.
pub(crate) type DeathOutcome = (ActorId, StopReason, u64);

/// Synchronous safety net for one child incarnation's runner task.
///
/// Constructed in the parent's own synchronous code, before the runner is
/// spawned, and moved into the runner's future as a captured value. If that
/// task is ever torn down before it finishes running - most notably, aborted
/// before it is ever polled at all - Rust still runs the drop glue of
/// everything it captured, so this guard's `Drop` still fires. That is
/// exactly what happens, level by level, when a supervisor's own task dies
/// with `StopReason::Kill`: it never awaits its own children, it simply drops
/// its supervision state (its death plane included), which fires the guard of
/// every live child's runner task in one step.
///
/// What a firing guard does is the child's [`InheritedStop`]. A worker is
/// raised `Kill` and its task aborted once its kill grace has passed. A
/// child that is itself a supervisor is raised
/// [`ParentKilled`](StopReason::ParentKilled) and stops its own children in
/// order, bounded by its own `Shutdown` window: the reaper raises `Kill` when
/// the window has passed and aborts the task its kill grace after that.
/// Under `Shutdown::Infinity` the orderly stop has no window and nothing
/// forces it. A child whose lane already holds a `Kill` when the guard fires
/// keeps that `Kill`, whatever its `Shutdown`, and its task is aborted once
/// its kill grace has passed.
///
/// Every effect below is safe to run more than once, and safe to run after
/// the child has already stopped on its own: raising on an already-closed
/// lane and aborting an already-finished task are both no-ops. The child's
/// status watch is never written here at all: `Stopped` is published only by
/// the child's own run loop reaching its normal exit, so observing it always
/// means the task actually got there.
/// [`ActorHandle::wait_stopped`](crate::actor::handle::ActorHandle::wait_stopped)
/// still resolves for a child whose task ended some other way, since its
/// status sender closing with no final write leaves nobody left to report
/// one.
pub(crate) struct ChildLinkGuard {
    stop_lane: StopLane,
    abort: AbortHandle,
    reaper: ReaperFeed,
    /// What the guard does to the child when it fires.
    inherited: rules::InheritedStop,
    /// The system this child belongs to, used to resolve a fresh reaper if
    /// the one this guard was handed turns out to belong to a runtime that
    /// is now gone (see `Drop`, below).
    system: Arc<ActorSystem>,
    /// The runtime of the supervisor that built this guard: the one whose
    /// reaper the guard was handed, and the one a retry resolves its reaper
    /// on.
    runtime: Handle,
    /// True until the child's fate has been recorded. The runner disarms
    /// the guard immediately after writing the fate cell: at that point the
    /// child has terminated and reported on its own, so the guard's drop has
    /// no teardown left to force and does nothing. Every abnormal drop (an
    /// abort before the runner's first poll, the owning `JoinSet` dropped
    /// with the parent, an unadopted restart outcome) still finds the guard
    /// armed and fires the full ladder.
    armed: bool,
}

impl ChildLinkGuard {
    pub(crate) fn new(
        stop_lane: StopLane,
        abort: AbortHandle,
        reaper: ReaperFeed,
        inherited: rules::InheritedStop,
        system: Arc<ActorSystem>,
        runtime: Handle,
    ) -> Self {
        Self {
            stop_lane,
            abort,
            reaper,
            inherited,
            system,
            runtime,
            armed: true,
        }
    }

    /// Marks the guarded child as fully terminated and reported; the guard's
    /// drop becomes a no-op. Called by the runner right after the fate cell
    /// is written.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildLinkGuard {
    fn drop(&mut self) {
        // A disarmed guard belongs to a child that already terminated and
        // wrote its fate; there is nothing left to tear down.
        if !self.armed {
            return;
        }
        // (a) Raise the child's inherited stop: a plain watch write,
        // infallible, and a no-op if the child has already finished its
        // message loop. (b) Enqueue its deadlines with the reaper: a
        // synchronous send on an unbounded feed (this is `Drop`: no awaiting,
        // no spawning that itself needs to be awaited). Every deadline
        // saturates, since a window and a grace are the user's and an
        // overflowing `Instant` sum would panic in drop glue.
        // A supervisor child is raised its orderly stop first; a `Kill` the
        // lane holds by then (raised before, or racing this raise) keeps the
        // abort it is owed, whatever the child's `Shutdown`.
        if matches!(self.inherited, rules::InheritedStop::Orderly { .. }) {
            self.stop_lane.raise(StopReason::ParentKilled);
        }
        let kill_stored = matches!(self.stop_lane.reason(), Some(StopReason::Kill));
        let entry = match rules::fired_stop(self.inherited, kill_stored) {
            rules::InheritedStop::Kill { grace } => {
                self.stop_lane.raise(StopReason::Kill);
                ReaperEntry::new(
                    saturating_deadline(Instant::now(), grace),
                    self.abort.clone(),
                    self.stop_lane.abort_record().clone(),
                )
            }
            rules::InheritedStop::Orderly { window, grace } => {
                // An unbounded orderly stop has no entry: nothing forces it.
                let Some(window) = window else {
                    return;
                };
                let kill_at = saturating_deadline(Instant::now(), window);
                ReaperEntry::two_stage(
                    kill_at,
                    self.stop_lane.clone(),
                    saturating_deadline(kill_at, grace),
                    self.abort.clone(),
                )
            }
        };
        // The reaper this guard was handed may belong to a runtime that is
        // gone now: the system feeds the entry to the reaper of the
        // supervisor's runtime once more before it gives up, which is what
        // lets the child's full grace survive a runtime change instead of
        // degrading straight to an abort.
        if let Err(mpsc::error::SendError(entry)) = self.reaper.send(entry) {
            self.system.feed_reaper(&self.runtime, entry);
        }
    }
}

/// How a manual stop was requested, recorded on the child so its death event
/// bypasses strategy evaluation (manual stops are never failures).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManualStop {
    /// `stop_child`: restart per policy afterwards, budget-free.
    Bounce,
    /// `terminate_child`: stay down until restart_child/delete_child (OTP).
    Terminate,
}

// ---------------------------------------------------------------------------
// ChildLifecycle
// ---------------------------------------------------------------------------

/// What a down child's last incarnation still owes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownPending {
    /// Nothing: the incarnation's end was delivered or decided.
    None,
    /// A completed manual stop holds a snapshot of the incarnation's end
    /// that the death plane's report delivers (see
    /// [`SupervisionState::pending_manual_events`]).
    Snapshot,
    /// The incarnation died and the restart decision for its death is still
    /// to be made; its report is evaluated exactly once.
    Evaluate,
}

/// Where a restart attempt came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The child's first incarnation, started by `spawn_child`.
    Initial,
    /// A restart the strategy decided.
    Strategy,
    /// The restart of a `Permanent` child after `stop_child`.
    Bounce,
    /// A `restart_child` call.
    Manual,
    /// The start a group restart's chain made in the member's slot.
    Chain,
}

/// The lifecycle of one child spec inside its supervisor's registry.
///
/// Exactly one variant holds at any moment, and [`transition`](ChildState::transition)
/// is the single, default-closed authority for moving between them: every
/// row of [`rules::is_legal`] is explicit, and anything else is rejected
/// outright instead of silently applying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildLifecycle {
    /// The child's current incarnation is live and processing messages.
    Running(u64),
    /// A manual stop (`stop_child`/`terminate_child`) has been committed:
    /// the lane has been raised, and the caller (or the group machinery) is
    /// awaiting this incarnation's fate cell. `kind` decides what happens
    /// once that fate is observed.
    Stopping { incarnation: u64, kind: ManualStop },
    /// A restart is in flight: `incarnation` is the dead/stopped instance
    /// this spec last held, `next` is the fresh incarnation token assigned
    /// to the attempt (adopted on a successful [`RestartOutcome::Adopted`],
    /// or recorded as `Down` on a [`RestartOutcome::Failed`]).
    Restarting { incarnation: u64, next: u64 },
    /// The spec is retained but no instance is running. `incarnation` is the
    /// last incarnation the spec held, and `pending` what its end still owes.
    Down {
        incarnation: u64,
        pending: DownPending,
    },
}

impl ChildLifecycle {
    /// The incarnation this spec currently holds a live or in-flight
    /// instance of: the running one, the one being stopped, or the attempt in
    /// flight (`next`). A down spec has none, and the token of a restart's
    /// retired incarnation is never an occupant.
    pub(crate) fn occupant(&self) -> Option<u64> {
        match self {
            ChildLifecycle::Running(incarnation) | ChildLifecycle::Stopping { incarnation, .. } => {
                Some(*incarnation)
            }
            ChildLifecycle::Restarting { next, .. } => Some(*next),
            ChildLifecycle::Down { .. } => None,
        }
    }

    /// The token the ledger holds for the incarnation a barrier awaits: the
    /// occupant's, or for a down spec the incarnation it last held.
    pub(crate) fn held_token(&self) -> u64 {
        match self {
            ChildLifecycle::Running(incarnation)
            | ChildLifecycle::Stopping { incarnation, .. }
            | ChildLifecycle::Down { incarnation, .. } => *incarnation,
            ChildLifecycle::Restarting { next, .. } => *next,
        }
    }
}

// ---------------------------------------------------------------------------
// SupervisionConfig
// ---------------------------------------------------------------------------

/// Configuration for an actor acting as a supervisor.
///
/// Maps to OTP supervisor child specs: strategy, intensity (max_restarts),
/// and period (restart_window).
#[derive(Debug, Clone)]
pub struct SupervisionConfig {
    /// The restart strategy to use.
    pub strategy: RestartStrategy,
    /// Maximum number of restarts allowed within `restart_window`.
    pub max_restarts: u32,
    /// The sliding window for counting restarts. A restart counts against the
    /// budget while it is no older than the window, the end of the window
    /// included, as in OTP's period check. A zero window therefore exhausts the
    /// budget only when restarts read the same instant, for example on a paused
    /// clock that does not move between them; on a running clock, or on a
    /// paused clock that auto-advances to a pending timer, it practically never
    /// exhausts a budget above zero. A `max_restarts` of 0 exhausts the budget
    /// at the first restart the strategy would make, whatever the window; a
    /// failure that makes no restart (a `Temporary` child's, or a `Transient`
    /// child's clean exit) does not charge it. OTP rejects a zero period; this
    /// crate accepts it.
    pub restart_window: Duration,
}

impl Default for SupervisionConfig {
    fn default() -> Self {
        Self {
            strategy: RestartStrategy::OneForOne,
            max_restarts: 3,
            restart_window: Duration::from_secs(5),
        }
    }
}

impl SupervisionConfig {
    /// OneForOne strategy with default budget.
    pub fn one_for_one() -> Self {
        Self::default()
    }

    /// OneForAll strategy with default budget.
    pub fn one_for_all() -> Self {
        Self {
            strategy: RestartStrategy::OneForAll,
            ..Self::default()
        }
    }

    /// RestForOne strategy with default budget.
    pub fn rest_for_one() -> Self {
        Self {
            strategy: RestartStrategy::RestForOne,
            ..Self::default()
        }
    }

    /// SimpleOneForOne strategy with default budget.
    pub fn simple_one_for_one() -> Self {
        Self {
            strategy: RestartStrategy::SimpleOneForOne,
            ..Self::default()
        }
    }

    /// Sets the restart budget (max restarts within a sliding window).
    pub fn max_restarts(mut self, max: u32, window: Duration) -> Self {
        self.max_restarts = max;
        self.restart_window = window;
        self
    }
}

// ---------------------------------------------------------------------------
// RestartBudget
// ---------------------------------------------------------------------------

/// Sliding-window restart budget tracker.
///
/// Uses a `VecDeque<Instant>` to record restart timestamps.
/// Expired entries (outside the window) are pruned on each check.
pub(crate) struct RestartBudget {
    max_restarts: u32,
    restart_window: Duration,
    timestamps: VecDeque<Instant>,
}

impl RestartBudget {
    pub(crate) fn new(max_restarts: u32, restart_window: Duration) -> Self {
        Self {
            max_restarts,
            restart_window,
            timestamps: VecDeque::new(),
        }
    }

    /// Checks if a restart is allowed. If yes, records it and returns `true`.
    /// If the budget is exhausted, returns `false`.
    pub(crate) fn check_and_record(&mut self) -> bool {
        let now = Instant::now();
        // checked_sub, never `now - self.restart_window`: a paused or
        // simulated clock (tokio::time::pause) can sit close to its own
        // epoch, so a window wider than elapsed time would underflow a raw
        // subtraction and panic. `None` here just means nothing is old
        // enough to prune yet - a safe no-op, so the count-based check below
        // still runs correctly.
        let cutoff = now.checked_sub(self.restart_window);

        // Prune expired entries
        if let Some(cutoff) = cutoff {
            while let Some(&front) = self.timestamps.front() {
                if front < cutoff {
                    self.timestamps.pop_front();
                } else {
                    break;
                }
            }
        }

        if self.timestamps.len() >= self.max_restarts as usize {
            return false;
        }

        self.timestamps.push_back(now);
        true
    }
}

// ---------------------------------------------------------------------------
// ChildSpec
// ---------------------------------------------------------------------------

/// Per-child supervision specification.
pub(crate) struct ChildSpec {
    pub restart_type: RestartType,
    pub shutdown: Shutdown,
    /// Bounds the supervisor's wait for this child's initialization to
    /// complete during a restart. `None` (default) waits indefinitely,
    /// matching Erlang/OTP supervisor semantics; setting a bound is a
    /// deliberate deviation that trades strict parity for a supervisor that
    /// can never be stalled by one child's hung init. Initial spawning never
    /// awaits initialization (see `spawn_child`), so this bound applies only
    /// to restarts.
    ///
    /// Stored here for restart-path lookups; the active reader is the
    /// restart closure's own captured copy (see `spawn_child_internal` in
    /// `context.rs`), which the restart path consumes to bound the ack wait.
    #[allow(dead_code)]
    pub start_timeout: Option<Duration>,
    /// The time this child is given, after a `Kill` is raised on it, to reach
    /// its next turn boundary before its task is aborted. An invariant of
    /// the spec, kept across every restart; its link guard, its runner's
    /// ladder and every kill path read it.
    pub kill_grace: Duration,
    /// Whether this child's own resolved configuration enables supervision
    /// (it is itself a supervisor of further children). An invariant of the
    /// spec, not of any one incarnation, so it survives every restart; it
    /// decides what the child inherits when its supervisor is stopped, never
    /// a grace.
    ///
    /// Stored here for parity with `start_timeout` above; every reader is a
    /// captured copy taken at `spawn_child` time (the initial guard, and the
    /// restart closure's own copy - see `spawn_child_internal` in
    /// `context.rs`).
    #[allow(dead_code)]
    pub is_supervisor: bool,
}

// ---------------------------------------------------------------------------
// ChildState
// ---------------------------------------------------------------------------

/// Runtime state for a supervised child.
pub(crate) struct ChildState {
    pub id: ActorId,
    pub name: Option<String>,
    pub spec: ChildSpec,
    /// The supervisor's control of the occupant's slot: the stop order it
    /// writes to the occupant's executor, and the cell the executor
    /// populates once the child's task has fully exited - after every one of
    /// its own drops, its `RegistryGuard` included, so a populated fate
    /// always means the child's registered name is already free. For a slot
    /// whose occupant is a restart attempt, the executor is the attempt
    /// itself until it hands the slot to the incarnation's runner.
    pub ctl: SlotCtl,
    /// This child's lifecycle: which incarnation is current, and whether it
    /// is running, being manually stopped, restarting, or retained-but-down.
    /// The single source of truth - see [`ChildLifecycle`].
    pub lifecycle: ChildLifecycle,
    /// Where the attempt in flight, or the last one, came from.
    pub origin: Origin,
    /// The subtree of the most recent incarnation spawned for this child,
    /// adopted or not: the next incarnation waits for it to end.
    pub predecessor: Option<Predecessor>,
}

impl ChildState {
    /// Records the subtree the slot's cell holds as the child's most recent,
    /// when the cell holds one. An attempt that spawned nothing leaves the
    /// earlier record in place.
    pub(crate) fn note_subtree(&mut self) {
        if let Some(predecessor) = self.ctl.recorded_predecessor() {
            self.predecessor = Some(predecessor);
        }
    }

    /// The slot control of `incarnation`, when it is the one the control
    /// belongs to: the occupant, or the attempt in flight. A slot is replaced
    /// when a restart is initiated, so an incarnation the ledger has moved
    /// past has none here.
    pub(crate) fn slot_of(&self, incarnation: u64) -> Option<&SlotCtl> {
        let owns = match self.lifecycle {
            ChildLifecycle::Running(n)
            | ChildLifecycle::Stopping { incarnation: n, .. }
            | ChildLifecycle::Down { incarnation: n, .. } => n == incarnation,
            ChildLifecycle::Restarting { next, .. } => next == incarnation,
        };
        owns.then_some(&self.ctl)
    }

    /// Applies `to` if [`rules::is_legal`] allows the move from the current
    /// state; otherwise the state is left untouched and the rejected target
    /// is handed back. The single point every lifecycle change in this crate
    /// goes through.
    pub(crate) fn transition(&mut self, to: ChildLifecycle) -> Result<(), ChildLifecycle> {
        if rules::is_legal(&self.lifecycle, &to) {
            self.lifecycle = to;
            Ok(())
        } else {
            Err(to)
        }
    }
}

// ---------------------------------------------------------------------------
// ChildRegistry
// ---------------------------------------------------------------------------

/// Ordered child registry with O(1) lookup by ID.
///
/// Children are stored in insertion order (start order).
/// `Vec<ChildState>` preserves ordering for RestForOne/OneForAll.
/// `HashMap<ActorId, usize>` provides O(1) lookup.
pub(crate) struct ChildRegistry {
    children: Vec<ChildState>,
    index: HashMap<ActorId, usize>,
}

impl ChildRegistry {
    pub(crate) fn new() -> Self {
        Self {
            children: Vec::new(),
            index: HashMap::new(),
        }
    }

    pub(crate) fn register(&mut self, state: ChildState) {
        let idx = self.children.len();
        self.index.insert(state.id.clone(), idx);
        self.children.push(state);
    }

    pub(crate) fn remove(&mut self, id: &ActorId) -> Option<ChildState> {
        let idx = self.index.remove(id)?;
        let child = self.children.remove(idx);
        // Rebuild index for items after the removed one
        for (new_idx, child) in self.children.iter().enumerate().skip(idx) {
            self.index.insert(child.id.clone(), new_idx);
        }
        Some(child)
    }

    pub(crate) fn get(&self, id: &ActorId) -> Option<&ChildState> {
        self.index.get(id).map(|&idx| &self.children[idx])
    }

    pub(crate) fn get_mut(&mut self, id: &ActorId) -> Option<&mut ChildState> {
        self.index
            .get(id)
            .copied()
            .map(|idx| &mut self.children[idx])
    }

    /// Every child, in start order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &ChildState> {
        self.children.iter()
    }

    /// Returns IDs of children started after the given child.
    #[cfg(test)]
    pub(crate) fn children_after(&self, id: &ActorId) -> Vec<ActorId> {
        if let Some(&idx) = self.index.get(id) {
            self.children[idx + 1..]
                .iter()
                .map(|c| c.id.clone())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Returns all child IDs.
    #[cfg(test)]
    pub(crate) fn all_ids(&self) -> Vec<ActorId> {
        self.children.iter().map(|c| c.id.clone()).collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.children.len()
    }

    /// Adopts a restarted child instance after the parent accepted its
    /// `RestartOutcome::Adopted` (seq matches the pending restart).
    ///
    /// The slot control stored for the attempt when the restart was
    /// initiated stays: it is the new incarnation's control.
    pub(crate) fn update_restarted(&mut self, child_id: &ActorId, seq: u64) -> bool {
        if let Some(child) = self.get_mut(child_id) {
            let pending =
                matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == seq);
            if pending && child.transition(ChildLifecycle::Running(seq)).is_ok() {
                return true;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Group restart state (OneForAll / RestForOne)
// ---------------------------------------------------------------------------

/// In-flight group restart: the affected live members are committed to the
/// barrier and stopped one at a time (reverse start order); once every awaited
/// death arrives, the members are
/// restarted SEQUENTIALLY in start order. The supervisor's loop never blocks
/// on this; deaths arrive through the ordinary runner channel.
pub(crate) struct GroupRestart {
    /// Members whose deaths we are still waiting for, each with the
    /// incarnation the group committed (for a member whose restart attempt
    /// was in flight, the attempt's incarnation, whether or not its actor
    /// was spawned): a later incarnation under the same id (a deleted and
    /// respawned child) is not the one awaited.
    pub awaiting: HashMap<ActorId, u64>,
    /// The members the stop phase still has to stop, in reverse start order:
    /// the queue stops them one at a time, each after the one ahead of it is
    /// settled ([`rules::stop_queue_next`]). The chain starts when it and
    /// `awaiting` are both empty.
    pub stop_queue: VecDeque<ActorId>,
    /// The queue reached its head and waits for the head's end. Only the
    /// queue sets it, and no manual call changes it.
    pub head_reached: bool,
    /// Members to restart (start order) once `awaiting` empties. Excludes
    /// Temporary children (OTP: terminated with the group, never restarted)
    /// and every member in `manual_overrides`.
    pub restart_order: Vec<ActorId>,
    /// Members a caller explicitly `terminate_child`'d while this group stop
    /// was in flight, each with the incarnation the call released: the
    /// caller's manual intent is honored over the group's default
    /// disposition, so these are taken out of `restart_order` when the call
    /// is made and reported as `Removed` instead of `RestartInitiated` when
    /// their individual death arrives. A retry of the group restart carries
    /// them back unchanged.
    pub manual_overrides: HashMap<ActorId, u64>,
    /// The sets the group keeps through its stop phases and its chains.
    pub scope: GroupScope,
}

/// What a pending group restart remembers across its retries.
///
/// `restarted` holds the members a chain of the group already restarted: a
/// chain adds a key when it adopts the incarnation it started for it, the set
/// is kept through every retry and cleared when the group completes, and a
/// key leaves it when its spec is removed. `outside` holds, for
/// `RestForOne`, the keys before the first failed member when the group's
/// first plan was made: they are not part of the group and stay outside it
/// through every retry, however far the retries' failed members move.
#[derive(Clone, Debug, Default)]
pub(crate) struct GroupScope {
    pub restarted: HashSet<ActorId>,
    pub outside: HashSet<ActorId>,
}

impl GroupRestart {
    /// Marks the awaited incarnation of `child_id` as ended. Returns `false`
    /// when `incarnation` is not the one awaited, in which case nothing
    /// changes: the end of another incarnation under the same id is not the
    /// end this group waits for.
    pub(crate) fn settle_awaited(&mut self, child_id: &ActorId, incarnation: u64) -> bool {
        if self.awaiting.get(child_id) == Some(&incarnation) {
            self.awaiting.remove(child_id);
            true
        } else {
            false
        }
    }
}

/// Phase of an in-flight OneForAll/RestForOne group restart.
pub(crate) enum GroupPhase {
    /// Affected members being stopped one at a time; awaiting their deaths.
    Stopping(GroupRestart),
    /// Restarting members one at a time in start order (OTP left-to-right).
    /// The FRONT of the queue is the member whose restart is in flight; the
    /// next member is initiated only once the front's [`RestartOutcome`] is
    /// adopted. Adoption happens only after the fresh incarnation's init ack
    /// arrives - the restart path awaits the same `pre_start`/`on_started`
    /// contract the top-level spawn awaits - so sequential initiation gives
    /// sequential init COMPLETION: no two consecutive members' inits overlap.
    /// When the front's own start fails, the whole group is laid out again
    /// from the registry (a retry) and returns to the stop phase.
    ///
    /// `overrides` keeps the stop phase's `manual_overrides` until the group
    /// completes: a member a `terminate_child` released stays released for
    /// the whole group restart. `started` holds, per member, the incarnation
    /// the chain itself started for it: a member that runs an incarnation
    /// the chain did not start is stopped and started again in its slot.
    Restarting {
        queue: VecDeque<ActorId>,
        overrides: HashMap<ActorId, u64>,
        started: HashMap<ActorId, u64>,
        scope: GroupScope,
    },
}

// ---------------------------------------------------------------------------
// Supervision state (held by ActorContext)
// ---------------------------------------------------------------------------

/// The plane a supervision task belongs to, recorded when the task is
/// registered: the restart plane holds restart attempts, the death plane the
/// runners and the replacement tasks that follow an incarnation after the
/// task that did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskPlane {
    Death,
    Restart,
}

/// Internal supervision state stored in the actor context.
pub(crate) struct SupervisionState {
    pub config: SupervisionConfig,
    pub registry: ChildRegistry,
    pub budget: RestartBudget,
    /// Type-erased restart functions keyed by child ID.
    pub restart_fns: HashMap<ActorId, RestartFn>,
    /// In-flight OneForAll/RestForOne group restart, if any.
    pub pending_group: Option<GroupPhase>,
    /// Failure events that arrived while a group restart was pending;
    /// evaluated FIFO after the group completes.
    pub queued_triggers: VecDeque<ChildStoppedInternal>,
    /// `on_child_stopped` snapshots for manual completions still awaiting
    /// their death-plane event, keyed by (child id, incarnation). Populated
    /// synchronously the moment a manual stop's real fate is classified -
    /// independent of the registry, so it survives a same-handler
    /// `delete_child`/respawn of the same name before the run loop's death
    /// arm gets a chance to drain the matching runner completion and
    /// deliver it exactly once.
    pub pending_manual_events: HashMap<(ActorId, u64), ChildEvent>,
    /// Every live child's runner task. Dropping this - which happens as
    /// soon as this whole `SupervisionState` is dropped - aborts every task
    /// still in it; a task aborted before it completes its own `join.await`
    /// still runs its captured [`ChildLinkGuard`]'s `Drop`, which is exactly
    /// how a `StopReason::Kill` teardown cascades to this supervisor's
    /// children without this supervisor ever awaiting them.
    pub death_set: JoinSet<DeathOutcome>,
    /// Every in-flight restart attempt. Dropping this - which happens as
    /// soon as this whole `SupervisionState` is dropped - aborts every task
    /// still in it; an `Adopted` attempt not yet retrieved by the run loop
    /// carries its own [`ChildLinkGuard`], so aborting (or simply dropping
    /// the completed-but-unread outcome) still kills the fresh incarnation
    /// it spawned through the same ladder - no orphaned incarnation, for a
    /// restart pending in ANY phase (pre-spawn, mid-init, or already
    /// adopted-but-not-yet-drained).
    pub restart_set: JoinSet<RestartOutcome>,
    /// The child incarnation each task of either plane works for, keyed by
    /// its task id and cleared when the task's completion is drained: a task
    /// that fails (panics or is cancelled) returns only a `JoinError`, and
    /// this is how the failure finds the child it concerns.
    pub task_owner: HashMap<tokio::task::Id, (ActorId, u64, TaskPlane)>,
    /// Woken whenever an incarnation of one of this supervisor's slots
    /// receives the Unresponsive verdict, so the run loop re-checks what
    /// waits on it.
    pub verdicts: Arc<Verdicts>,
    /// Set while `on_child_stopped` runs one of the final deliveries made
    /// when the supervisor's message loop ended: supervision calls made
    /// there are refused.
    pub final_delivery: bool,
    /// The reports a turn took and did not deliver when it ended the
    /// supervisor's loop (a completing group's worklist, a report a factory's
    /// drop panic left undelivered); they are collected for their own
    /// deliveries with the others the loop left pending.
    pub owed: VecDeque<ChildStoppedInternal>,
    /// The names of owed reports whose child's spec is gone, by child and
    /// incarnation, for their final deliveries.
    pub owed_names: HashMap<(ActorId, u64), String>,
    /// The subtree of the last incarnation of each child whose spec was
    /// removed while that subtree still ran, by child id. An entry lives
    /// exactly until its subtree has ended.
    retired: HashMap<ActorId, Predecessor>,
}

impl SupervisionState {
    pub(crate) fn new(config: SupervisionConfig) -> Self {
        let budget = RestartBudget::new(config.max_restarts, config.restart_window);
        Self {
            config,
            registry: ChildRegistry::new(),
            budget,
            restart_fns: HashMap::new(),
            pending_group: None,
            queued_triggers: VecDeque::new(),
            pending_manual_events: HashMap::new(),
            death_set: JoinSet::new(),
            restart_set: JoinSet::new(),
            task_owner: HashMap::new(),
            verdicts: Arc::new(Verdicts::default()),
            final_delivery: false,
            owed: VecDeque::new(),
            owed_names: HashMap::new(),
            retired: HashMap::new(),
        }
    }

    /// Leaves a report the run loop took and did not deliver for the final
    /// deliveries, after the ones already left, with the child's name.
    pub(crate) fn owe(&mut self, report: ChildStoppedInternal, name: Option<String>) {
        if let Some(name) = name {
            self.owed_names
                .insert((report.child_id.clone(), report.incarnation), name);
        }
        self.owed.push_back(report);
    }

    /// The subtree of a removed child under `id` that still runs, if any.
    pub(crate) fn retired_subtree(&mut self, id: &ActorId) -> Option<Predecessor> {
        self.retired.retain(|_, subtree| !subtree.is_gone());
        self.retired.get(id).cloned()
    }

    /// The subtree a start under `id` waits for: the last incarnation of the
    /// child the registry holds, else the subtree of a removed child. `None`
    /// when no subtree is known to run.
    pub(crate) fn predecessor_of(&mut self, id: &ActorId) -> Option<Predecessor> {
        match self.registry.get(id) {
            Some(child) => child.predecessor.clone(),
            None => self.retired_subtree(id),
        }
    }

    /// Initiates a restart for a child using its stored restart closure:
    /// transitions the ledger from `Down` to `Restarting` with the given
    /// (freshly drawn) incarnation token, and spawns the closure. A new
    /// incarnation begins only from `Down`: any other state refuses. Does NOT
    /// touch the budget (callers decide whether the restart is
    /// budget-charged - strategy restarts are, manual bounces are not).
    ///
    /// `seq` is drawn by the caller from
    /// [`ActorSystem::next_incarnation`](crate::system::ActorSystem::next_incarnation),
    /// the single counter every incarnation in the system draws from.
    /// `origin` is where the attempt comes from; an attempt the group
    /// restart's chain makes is recorded as the incarnation the chain started
    /// for the child.
    pub(crate) fn initiate(&mut self, child_id: &ActorId, seq: u64, origin: Origin) -> bool {
        let (ctl, io) = slot_pair_for(self.verdicts.clone(), Some((child_id.clone(), seq)));
        let predecessor;
        match self.registry.get_mut(child_id) {
            Some(child) => {
                let ChildLifecycle::Down { incarnation, .. } = child.lifecycle else {
                    return false;
                };
                predecessor = child.predecessor.clone();
                if child
                    .transition(ChildLifecycle::Restarting {
                        incarnation,
                        next: seq,
                    })
                    .is_err()
                {
                    return false;
                }
                // The attempt's slot is the occupant's control from the
                // attempt's first step.
                child.ctl = ctl;
                child.origin = origin;
            }
            None => return false,
        }
        if let Some(restart_fn) = self.restart_fns.get(child_id) {
            let fut = restart_fn(seq, io, predecessor);
            let task = self.restart_set.spawn(fut);
            self.task_owner
                .insert(task.id(), (child_id.clone(), seq, TaskPlane::Restart));
            if origin == Origin::Chain {
                if let Some(GroupPhase::Restarting { started, .. }) = self.pending_group.as_mut() {
                    started.insert(child_id.clone(), seq);
                }
            }
            true
        } else {
            false
        }
    }

    /// Removes a child's spec, and with it the child from the pending group
    /// restart's sets: a removed spec is in no group set, and a child later
    /// spawned under the same id is a new child, not a member. A barrier
    /// entry the group holds for the removed child's last incarnation stays.
    /// The subtree of that incarnation, while it still runs, is kept under the
    /// child's id.
    ///
    /// The child's factory is user code, so its drop may panic: it is
    /// returned once every write here is done, and the caller drops it after
    /// its own last write (a context method as its last step, so a panic is
    /// its callback's; the run loop through a caught drop).
    #[must_use = "the factory is dropped by the caller, after its last write"]
    pub(crate) fn remove_spec(&mut self, child_id: &ActorId) -> Option<RestartFn> {
        self.retired.retain(|_, subtree| !subtree.is_gone());
        if let Some(child) = self.registry.remove(child_id) {
            // A start under this id waits for the removed child's subtree.
            if let Some(subtree) = child.predecessor.filter(|subtree| !subtree.is_gone()) {
                self.retired.insert(child_id.clone(), subtree);
            }
        }
        let factory = self.restart_fns.remove(child_id);
        match self.pending_group.as_mut() {
            Some(GroupPhase::Stopping(group)) => {
                group.restart_order.retain(|id| id != child_id);
                group.manual_overrides.remove(child_id);
                group.scope.restarted.remove(child_id);
                group.scope.outside.remove(child_id);
            }
            Some(GroupPhase::Restarting {
                queue,
                overrides,
                started,
                scope,
            }) => {
                queue.retain(|id| id != child_id);
                overrides.remove(child_id);
                started.remove(child_id);
                scope.restarted.remove(child_id);
                scope.outside.remove(child_id);
            }
            None => {}
        }
        factory
    }

    /// Whether the pending group restart will restart the child: its key is
    /// in the stop phase's restart order or in the chain's queue, and a
    /// `terminate_child` did not release it. A `Temporary` key is in neither,
    /// so a member of that type is never in the set, however the group
    /// relates to it otherwise.
    pub(crate) fn in_restart_set(&self, child_id: &ActorId) -> bool {
        match self.pending_group.as_ref() {
            Some(GroupPhase::Stopping(group)) => {
                group.restart_order.contains(child_id)
                    && !group.manual_overrides.contains_key(child_id)
            }
            Some(GroupPhase::Restarting {
                queue, overrides, ..
            }) => queue.contains(child_id) && !overrides.contains_key(child_id),
            None => false,
        }
    }

    /// How the child relates to the pending group restart, for the manual
    /// calls.
    ///
    /// The relation is read from the ledger's entry for the child, never from
    /// its id alone: a child that was deleted and spawned again under the same
    /// id holds another incarnation than the group refers to, and is not a
    /// member.
    ///
    /// - `Awaited`: the stop phase awaits an incarnation of the child, the
    ///   ledger holds exactly that incarnation, and no completed manual stop
    ///   holds a snapshot of its end. `overridden` is set when a
    ///   `terminate_child` released that incarnation from the group.
    /// - `Queued`: the child waits in the group's restart set (the stop
    ///   phase's restart order, or the chain's queue).
    /// - `Released`: a `terminate_child` released an incarnation of the child
    ///   from the group, and the ledger holds that same incarnation, either
    ///   down or being stopped by a `terminate_child` (a bounce in progress
    ///   is never released).
    /// - `None`: any other child, one that is running, restarting or down
    ///   under another incarnation than the one released included.
    pub(crate) fn group_membership(&self, child_id: &ActorId) -> GroupMembership {
        let life = self.registry.get(child_id).map(|child| child.lifecycle);
        // The incarnation a `terminate_child` released from the group, and
        // whether the ledger holds it, down or stopping for good.
        let released = |overrides: &HashMap<ActorId, u64>| {
            overrides.get(child_id).is_some_and(|&released| {
                matches!(
                    life,
                    Some(
                        ChildLifecycle::Down { incarnation, .. }
                            | ChildLifecycle::Stopping {
                                incarnation,
                                kind: ManualStop::Terminate,
                            }
                    ) if incarnation == released
                )
            })
        };
        match self.pending_group.as_ref() {
            Some(GroupPhase::Stopping(group)) => {
                if let Some(&awaited) = group.awaiting.get(child_id) {
                    if life.map(|life| life.held_token()) != Some(awaited) {
                        return GroupMembership::None;
                    }
                    let handled = self
                        .pending_manual_events
                        .contains_key(&(child_id.clone(), awaited));
                    if !handled {
                        return GroupMembership::Awaited {
                            overridden: group.manual_overrides.get(child_id) == Some(&awaited),
                        };
                    }
                }
                if life.is_some() && group.restart_order.contains(child_id) {
                    GroupMembership::Queued
                } else if released(&group.manual_overrides) {
                    GroupMembership::Released
                } else {
                    GroupMembership::None
                }
            }
            Some(GroupPhase::Restarting {
                queue, overrides, ..
            }) => {
                if life.is_some() && queue.contains(child_id) {
                    GroupMembership::Queued
                } else if released(overrides) {
                    GroupMembership::Released
                } else {
                    GroupMembership::None
                }
            }
            None => GroupMembership::None,
        }
    }

    /// The scope and progress sets of the pending group restart: the keys a
    /// chain already restarted, and for `RestForOne` the keys before its
    /// first failed member. Empty when no group is pending.
    fn group_scope(&self) -> Option<&GroupScope> {
        match self.pending_group.as_ref() {
            Some(GroupPhase::Stopping(group)) => Some(&group.scope),
            Some(GroupPhase::Restarting { scope, .. }) => Some(scope),
            None => None,
        }
    }

    /// For `RestForOne`, the keys before `failed` in start order: the keys a
    /// group restart planned for `failed` leaves outside it. Empty for every
    /// other strategy.
    pub(crate) fn first_plan_outside(&self, failed: &ActorId) -> HashSet<ActorId> {
        if self.config.strategy != RestartStrategy::RestForOne {
            return HashSet::new();
        }
        self.registry
            .iter()
            .map(|child| &child.id)
            .take_while(|id| *id != failed)
            .cloned()
            .collect()
    }

    /// True when a group restart is pending and covers the key: every key
    /// under `OneForAll`, every key not before the group's first failed
    /// member under `RestForOne`. The scope is fixed by the group's first
    /// plan, and no retry shrinks it.
    pub(crate) fn in_keep_scope(&self, child_id: &ActorId) -> bool {
        let Some(scope) = self.group_scope() else {
            return false;
        };
        match self.config.strategy {
            RestartStrategy::OneForAll => true,
            RestartStrategy::RestForOne => !scope.outside.contains(child_id),
            RestartStrategy::OneForOne | RestartStrategy::SimpleOneForOne => false,
        }
    }

    /// Records that the pending group restart must leave the child down: the
    /// incarnation the ledger holds for it (its occupant, or for a down spec
    /// the incarnation it last held) is released, in either phase of the
    /// group. A child whose spec is gone records nothing.
    pub(crate) fn record_override(&mut self, child_id: &ActorId) {
        let Some(token) = self
            .registry
            .get(child_id)
            .map(|child| child.lifecycle.held_token())
        else {
            return;
        };
        match self.pending_group.as_mut() {
            Some(GroupPhase::Stopping(group)) => {
                group.manual_overrides.insert(child_id.clone(), token);
            }
            Some(GroupPhase::Restarting { overrides, .. }) => {
                overrides.insert(child_id.clone(), token);
            }
            None => {}
        }
    }

    /// Takes the child out of the pending group's restart set, so nothing the
    /// group does later restarts it.
    pub(crate) fn leave_restart_set(&mut self, child_id: &ActorId) {
        match self.pending_group.as_mut() {
            Some(GroupPhase::Stopping(group)) => group.restart_order.retain(|id| id != child_id),
            Some(GroupPhase::Restarting { queue, .. }) => queue.retain(|id| id != child_id),
            None => {}
        }
    }

    /// Dismisses the restart decision a dead child still owes: the end is
    /// kept for its report as a removal, and the child stays down (a spec
    /// that is not kept is removed with it). The report itself stays where it
    /// is, queued behind the pending group restart or in the death plane, and
    /// is delivered once from the snapshot when it is handled, so the
    /// evaluation never runs and nothing is charged. Returns the factory of a
    /// removed spec, for the caller to drop after its last write.
    #[must_use = "the factory is dropped by the caller, after its last write"]
    pub(crate) fn dismiss_owed_evaluation(&mut self, child_id: &ActorId) -> Option<RestartFn> {
        let child = self.registry.get(child_id)?;
        let ChildLifecycle::Down {
            incarnation,
            pending: DownPending::Evaluate,
        } = child.lifecycle
        else {
            return None;
        };
        let reason = self
            .queued_triggers
            .iter()
            .find(|event| &event.child_id == child_id && event.incarnation == incarnation)
            .map_or_else(|| child.ctl.end_reason(), |event| event.reason.clone());
        let event = ChildEvent {
            child_id: child_id.clone(),
            child_name: child.name.clone(),
            reason,
            action: SupervisionAction::Removed,
        };
        let prunable = matches!(self.config.strategy, RestartStrategy::SimpleOneForOne)
            || child.spec.restart_type == RestartType::Temporary;
        self.pending_manual_events
            .insert((child_id.clone(), incarnation), event);
        if prunable {
            return self.remove_spec(child_id);
        }
        if let Some(child) = self.registry.get_mut(child_id) {
            let _ = child.transition(ChildLifecycle::Down {
                incarnation,
                pending: DownPending::Snapshot,
            });
        }
        None
    }

    /// Rewrites the pending snapshot of the end of the child's last
    /// incarnation, when it reports a restart the group does not make after the
    /// release,
    /// to report the removal the release made.
    pub(crate) fn rewrite_restart_snapshot(&mut self, child_id: &ActorId) {
        let Some(token) = self
            .registry
            .get(child_id)
            .map(|child| child.lifecycle.held_token())
        else {
            return;
        };
        if let Some(event) = self
            .pending_manual_events
            .get_mut(&(child_id.clone(), token))
        {
            if event.action == SupervisionAction::RestartInitiated {
                event.action = SupervisionAction::Removed;
            }
        }
    }

    /// The facts a manual call reads of the child under `child_id` and of the
    /// pending group restart.
    pub(crate) fn manual_facts(&self, child_id: &ActorId) -> ManualFacts {
        let child = self.registry.get(child_id);
        ManualFacts {
            life: child.map(|child| child.lifecycle),
            task: child.map_or(TaskState::Ended, |child| child.ctl.task_state()),
            membership: self.group_membership(child_id),
            restart_type: child.map_or(RestartType::Permanent, |child| child.spec.restart_type),
            pool: matches!(self.config.strategy, RestartStrategy::SimpleOneForOne),
            restarted: self
                .group_scope()
                .is_some_and(|scope| scope.restarted.contains(child_id)),
            in_keep_scope: self.in_keep_scope(child_id),
            policy: child.map_or_else(Shutdown::default, |child| child.spec.shutdown),
            lane_vetoable: child.is_some_and(|child| child.ctl.lane_vetoable()),
            reasked: child.is_some_and(|child| child.ctl.reasked()),
            has_order: child.is_some_and(|child| child.ctl.has_order()),
        }
    }

    /// The answer of a manual call for a child, computed by
    /// [`rules::manual_answer`] from the facts of the call.
    pub(crate) fn manual_answer(&self, op: ManualOp, child_id: &ActorId) -> ManualReply {
        rules::manual_answer(op, &self.manual_facts(child_id))
    }

    /// Introspection info for every child, from the ledger, whether the
    /// supervisor can see that the occupant's task ended, and the pending
    /// group restart's restart set.
    pub(crate) fn children_info(&self) -> Vec<ChildInfo> {
        self.registry
            .iter()
            .map(|child| {
                let fate_present = child.ctl.task_state() == TaskState::Ended;
                let in_restart_set = self.in_restart_set(&child.id);
                let (is_alive, restart_pending) =
                    rules::child_info_flags(child.lifecycle, fate_present, in_restart_set);
                ChildInfo {
                    id: child.id.clone(),
                    name: child.name.clone(),
                    restart_type: child.spec.restart_type,
                    shutdown: child.spec.shutdown,
                    is_alive,
                    restart_pending,
                }
            })
            .collect()
    }

    /// Accounts for a task of the death plane that failed (panicked or was
    /// cancelled) instead of returning its outcome: the child incarnation it
    /// worked for is found by the task's id, and a replacement task is
    /// pushed into the death plane that reports the incarnation's end once it
    /// ended, so the ledger records the death exactly once. A panic of the
    /// failed task is returned for the caller to record, and so is the id of
    /// the replacement task when the incarnation had ended already (the task
    /// then returns at once).
    pub(crate) fn lost_death_task(
        &mut self,
        err: tokio::task::JoinError,
    ) -> (Option<ActorError>, Option<tokio::task::Id>) {
        let owner = self.task_owner.remove(&err.id());
        let panic = join_error_panic(err);
        let mut replacement = None;
        if let Some((child_id, incarnation, _)) = owner {
            let slot = self
                .registry
                .get(&child_id)
                .and_then(|child| child.slot_of(incarnation));
            let ended = slot.is_some_and(|ctl| ctl.status_ended());
            let lost = slot.and_then(|ctl| ctl.lost_end());
            let task = match lost {
                Some(lost) => {
                    let key = (child_id.clone(), incarnation);
                    let task = self.death_set.spawn(async move {
                        let reason = lost.wait().await;
                        (child_id, reason, incarnation)
                    });
                    self.task_owner
                        .insert(task.id(), (key.0, key.1, TaskPlane::Death));
                    task
                }
                None => {
                    // The incarnation's end cannot be followed (its slot is
                    // gone, or no incarnation was ever spawned): the death is
                    // reported at once.
                    let reason = match panic.clone() {
                        Some(err) => StopReason::Failure(err),
                        None => StopReason::Kill,
                    };
                    let key = (child_id.clone(), incarnation);
                    let task = self
                        .death_set
                        .spawn(async move { (child_id, reason, incarnation) });
                    self.task_owner
                        .insert(task.id(), (key.0, key.1, TaskPlane::Death));
                    task
                }
            };
            if ended || slot.is_none() {
                replacement = Some(task.id());
            }
        }
        (panic, replacement)
    }

    /// As [`lost_death_task`](Self::lost_death_task), for a task of the
    /// restart plane: an attempt that failed after its incarnation was
    /// spawned is followed to the incarnation's end; one that failed before
    /// is reported at once as a failed attempt. The returned outcome, if
    /// any, is to be routed like any attempt's outcome, and the id, if any,
    /// is that of a replacement task that returns at once.
    pub(crate) fn lost_restart_task(
        &mut self,
        err: tokio::task::JoinError,
    ) -> (
        Option<RestartOutcome>,
        Option<ActorError>,
        Option<tokio::task::Id>,
    ) {
        let owner = self.task_owner.remove(&err.id());
        let panic = join_error_panic(err);
        let Some((child_id, incarnation, _)) = owner else {
            return (None, panic, None);
        };
        let slot = self
            .registry
            .get(&child_id)
            .and_then(|child| child.slot_of(incarnation));
        let ended = slot.is_some_and(|ctl| ctl.status_ended());
        let lost = slot.and_then(|ctl| ctl.lost_end());
        match lost {
            Some(lost) => {
                let key = (child_id.clone(), incarnation);
                let task = self.death_set.spawn(async move {
                    let reason = lost.wait().await;
                    (child_id, reason, incarnation)
                });
                self.task_owner
                    .insert(task.id(), (key.0, key.1, TaskPlane::Death));
                (None, panic, ended.then(|| task.id()))
            }
            None => {
                let reason = StopReason::Failure(panic.clone().unwrap_or_else(|| {
                    ActorError::Panic("restart attempt task failed".to_string())
                }));
                (
                    Some(RestartOutcome::Failed {
                        child_id,
                        incarnation,
                        reason,
                    }),
                    panic,
                    None,
                )
            }
        }
    }

    /// The tasks the final collection waits for before it reads the planes
    /// (the runners and the attempts), keyed by task id.
    ///
    /// The two sets are told apart by the plane each task was registered on,
    /// never by the ledger:
    /// - Runners (death plane): an occupant whose task published its end,
    ///   and every incarnation that does not occupy its key (another one does,
    ///   or the key is down or gone), which the ledger leaves only after its
    ///   end was seen. A replacement task that follows an attempt's
    ///   incarnation is a runner too; it returns at once when the
    ///   incarnation ended, and is not awaited before.
    /// - Attempts (restart plane): a restart attempt whose incarnation's task
    ///   published its end.
    pub(crate) fn final_await_sets(
        &self,
    ) -> (
        std::collections::HashSet<tokio::task::Id>,
        std::collections::HashSet<tokio::task::Id>,
    ) {
        let mut runners = std::collections::HashSet::new();
        let mut attempts = std::collections::HashSet::new();
        // Each task is registered on the plane it runs in, so the entries
        // naming a plane never outnumber the tasks that plane holds. This is
        // a bound on the counts, not a check of membership: a `JoinSet` cannot
        // be asked whether it holds a task id.
        debug_assert!(
            self.task_owner
                .values()
                .filter(|(_, _, plane)| *plane == TaskPlane::Restart)
                .count()
                <= self.restart_set.len()
        );
        debug_assert!(
            self.task_owner
                .values()
                .filter(|(_, _, plane)| *plane == TaskPlane::Death)
                .count()
                <= self.death_set.len()
        );
        for (task, (child_id, incarnation, plane)) in &self.task_owner {
            let child = self.registry.get(child_id);
            match (plane, child.map(|child| (child.lifecycle, &child.ctl))) {
                (TaskPlane::Restart, Some((ChildLifecycle::Restarting { next, .. }, ctl)))
                    if next == *incarnation =>
                {
                    if ctl.status_ended() {
                        attempts.insert(*task);
                    }
                }
                (TaskPlane::Restart, _) => {}
                (
                    TaskPlane::Death,
                    Some((
                        ChildLifecycle::Running(n)
                        | ChildLifecycle::Stopping { incarnation: n, .. },
                        ctl,
                    )),
                ) if n == *incarnation => {
                    if ctl.status_ended() {
                        runners.insert(*task);
                    }
                }
                (TaskPlane::Death, Some((ChildLifecycle::Restarting { next, .. }, ctl)))
                    if next == *incarnation =>
                {
                    if ctl.status_ended() {
                        runners.insert(*task);
                    }
                }
                // Another incarnation holds the key, or the key is down or
                // gone: the incarnation of this task was retired after its
                // end was seen.
                (TaskPlane::Death, _) => {
                    runners.insert(*task);
                }
            }
        }
        (runners, attempts)
    }
}

// ---------------------------------------------------------------------------
// Strategy helpers (used by runtime)
// ---------------------------------------------------------------------------

/// Result of applying a supervision strategy to a child's death.
pub(crate) enum StrategyOutcome {
    /// Restart exactly this child (OneForOne / SimpleOneForOne).
    RestartOne(ActorId),
    /// Group restart (OneForAll / RestForOne): the members the group covers,
    /// in start order, each with what the group does about its stop and
    /// whether it starts it again (see [`rules::group_commit`]).
    RestartGroup {
        commit: Vec<(ActorId, u64, rules::MemberAction)>,
    },
    /// The child is not restarted (temporary, or transient after a clean stop).
    Remove,
    /// The restart budget is exhausted; the supervisor must stop.
    BudgetExhausted,
}

/// Determines the supervision action for a stopped child.
///
/// Charges the budget ONCE per triggering failure regardless of group size
/// (OTP counts supervisor restarts, not per-child spawns).
pub(crate) fn evaluate_strategy(
    sup: &mut SupervisionState,
    failed_child_id: &ActorId,
    reason: &crate::types::StopReason,
) -> StrategyOutcome {
    let child = match sup.registry.get(failed_child_id) {
        Some(c) => c,
        None => return StrategyOutcome::Remove,
    };

    let restart_type = child.spec.restart_type;
    let should_restart = match restart_type {
        RestartType::Permanent => true,
        // OTP transient parity: restart only on ABNORMAL exits. The clean
        // reasons are normal and shutdown - our Graceful and ParentRequest
        // (which is also the exit reason of a budget-exhausted supervisor),
        // and ParentKilled, a clean exit ordered from above. Kill (OTP
        // `killed`) is abnormal and does restart.
        RestartType::Transient => !matches!(
            reason,
            crate::types::StopReason::Graceful
                | crate::types::StopReason::ParentRequest
                | crate::types::StopReason::ParentKilled
        ),
        RestartType::Temporary => false,
    };

    if !should_restart {
        return StrategyOutcome::Remove;
    }

    // Check budget
    if !sup.budget.check_and_record() {
        return StrategyOutcome::BudgetExhausted;
    }

    // Determine the affected set based on strategy
    match sup.config.strategy {
        RestartStrategy::OneForOne | RestartStrategy::SimpleOneForOne => {
            StrategyOutcome::RestartOne(failed_child_id.clone())
        }
        RestartStrategy::OneForAll | RestartStrategy::RestForOne => {
            let children: Vec<rules::GroupMember> = sup.registry.iter().map(group_member).collect();
            StrategyOutcome::RestartGroup {
                commit: rules::group_commit(sup.config.strategy, &children, failed_child_id),
            }
        }
    }
}

impl SupervisionState {
    /// The plan of a retry after the chain's own start of `failed` failed:
    /// the group laid out again from the registry as it is now, with the
    /// children a `terminate_child` released from this group restart kept
    /// down (see [`rules::retry_plan`]).
    pub(crate) fn plan_retry(&self, failed: &ActorId) -> rules::RetryPlan {
        // A key is released by the incarnation the `terminate_child` named: a
        // key whose ledger entry holds another incarnation is not.
        let named = |overrides: &HashMap<ActorId, u64>| -> Vec<ActorId> {
            overrides
                .iter()
                .filter(|(id, &token)| {
                    self.registry
                        .get(id)
                        .is_some_and(|child| child.lifecycle.held_token() == token)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        let released: Vec<ActorId> = match self.pending_group.as_ref() {
            Some(GroupPhase::Restarting { overrides, .. }) => named(overrides),
            Some(GroupPhase::Stopping(group)) => named(&group.manual_overrides),
            None => Vec::new(),
        };
        let registry: Vec<rules::GroupMember> = self.registry.iter().map(group_member).collect();
        rules::retry_plan(self.config.strategy, &registry, &released, failed)
    }
}

/// One child as a group evaluation reads it.
fn group_member(child: &ChildState) -> rules::GroupMember {
    rules::GroupMember {
        id: child.id.clone(),
        life: child.lifecycle,
        restart_type: child.spec.restart_type,
    }
}

/// What the caller of [`advance_restart_chain`] does next.
pub(crate) enum ChainStep {
    /// Start this member, which is down.
    Initiate(ActorId),
    /// The member at the front is stopping, or an attempt for it is in
    /// flight: nothing is started until its end or its outcome is handled.
    Hold,
    /// Every member is done: the group phase is cleared and every trigger
    /// queued during its lifetime was appended to the worklist.
    Done,
}

/// Advances an in-flight [`GroupPhase::Restarting`] chain past every member
/// already `Running` on the incarnation the chain started for it, which is
/// done (an incarnation the chain did not start is never taken for the
/// chain's own: it is not in `started`).
///
/// The front member is decided by [`rules::chain_front`]: a down member is
/// started; a member that runs an incarnation the chain did not start is
/// ordered to stop and the chain holds until that incarnation's end, which
/// starts the member in its slot (a defense: every incarnation the group laid
/// out ended before the chain began); a member being stopped, or with the
/// attempt the chain started in flight, holds the chain until its end or
/// outcome is handled.
pub(crate) fn advance_restart_chain(
    sup: &mut SupervisionState,
    work: &mut VecDeque<ChildStoppedInternal>,
) -> ChainStep {
    loop {
        let (front, started) = match sup.pending_group.as_ref() {
            Some(GroupPhase::Restarting { queue, started, .. }) => {
                let front = queue.front().cloned();
                let started = front.as_ref().and_then(|id| started.get(id).copied());
                (front, started)
            }
            _ => return ChainStep::Done,
        };
        let Some(id) = front else {
            sup.pending_group = None;
            work.extend(sup.queued_triggers.drain(..));
            return ChainStep::Done;
        };
        let Some(child) = sup.registry.get(&id) else {
            // The spec is gone: there is nothing to restart.
            if let Some(GroupPhase::Restarting { queue, .. }) = sup.pending_group.as_mut() {
                queue.pop_front();
            }
            continue;
        };
        match rules::chain_front(child.lifecycle, started) {
            rules::FrontAction::Fresh => {
                if let Some(GroupPhase::Restarting { queue, scope, .. }) =
                    sup.pending_group.as_mut()
                {
                    queue.pop_front();
                    scope.restarted.insert(id.clone());
                }
            }
            rules::FrontAction::Initiate => return ChainStep::Initiate(id),
            rules::FrontAction::StopThenInitiate => {
                // Every member that had an occupant when the group was laid
                // out ended before the chain began, so the chain finds no
                // incarnation it did not start. The path stays as a
                // defense: it never starts over an occupant.
                debug_assert!(
                    false,
                    "the chain's front runs an incarnation it did not start"
                );
                child.ctl.order_stop(child.spec.shutdown, true);
                return ChainStep::Hold;
            }
            rules::FrontAction::Hold => {
                // A front whose attempt is in flight is the attempt the chain
                // itself started: any other attempt was committed to the
                // group and ended before the chain began.
                debug_assert!(
                    !matches!(
                        child.lifecycle,
                        ChildLifecycle::Restarting { next, .. } if started != Some(next)
                    ),
                    "the chain's front has an attempt the chain did not start"
                );
                return ChainStep::Hold;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests (pub(crate) internals, Rust Book Ch 11.3)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::runtime::spawn_runner;
    use crate::types::{ActorStatus, StatusValue, StopReason};
    use tokio::sync::watch;
    use tokio::task::JoinHandle;

    // - RestartBudget -------------------------------------------------------

    #[tokio::test]
    async fn budget_allows_within_limit() {
        let mut budget = RestartBudget::new(3, Duration::from_secs(60));
        assert!(budget.check_and_record());
        assert!(budget.check_and_record());
        assert!(budget.check_and_record());
    }

    #[tokio::test]
    async fn budget_denies_when_exhausted() {
        let mut budget = RestartBudget::new(2, Duration::from_secs(60));
        assert!(budget.check_and_record());
        assert!(budget.check_and_record());
        assert!(!budget.check_and_record());
    }

    #[tokio::test]
    async fn budget_recovers_after_window() {
        let mut budget = RestartBudget::new(1, Duration::from_millis(50));
        assert!(budget.check_and_record());
        assert!(!budget.check_and_record());
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(budget.check_and_record());
    }

    // - ChildRegistry -------------------------------------------------------

    fn dummy_child_with(id: &str, restart_type: RestartType, alive: bool) -> ChildState {
        let (ctl, _io) = slot_pair(Default::default());
        ChildState {
            id: ActorId::from(id),
            name: Some(id.to_string()),
            spec: ChildSpec {
                restart_type,
                shutdown: Shutdown::default(),
                start_timeout: None,
                kill_grace: crate::actor::supervision::DEFAULT_KILL_GRACE,
                is_supervisor: false,
            },
            ctl,
            lifecycle: if alive {
                ChildLifecycle::Running(0)
            } else {
                ChildLifecycle::Down {
                    incarnation: 0,
                    pending: DownPending::None,
                }
            },
            origin: Origin::Initial,
            predecessor: None,
        }
    }

    fn dummy_child(id: &str) -> ChildState {
        dummy_child_with(id, RestartType::Permanent, true)
    }

    #[tokio::test]
    async fn registry_register_and_get() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("a"));
        reg.register(dummy_child("b"));
        assert_eq!(reg.len(), 2);
        assert!(reg.get(&ActorId::from("a")).is_some());
        assert!(reg.get(&ActorId::from("b")).is_some());
        assert!(reg.get(&ActorId::from("c")).is_none());
    }

    #[tokio::test]
    async fn registry_remove_reindexes() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("a"));
        reg.register(dummy_child("b"));
        reg.register(dummy_child("c"));

        reg.remove(&ActorId::from("a"));
        assert_eq!(reg.len(), 2);
        assert!(reg.get(&ActorId::from("a")).is_none());
        assert!(reg.get(&ActorId::from("b")).is_some());
        assert!(reg.get(&ActorId::from("c")).is_some());
    }

    #[tokio::test]
    async fn registry_children_after() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("a"));
        reg.register(dummy_child("b"));
        reg.register(dummy_child("c"));

        let after_a = reg.children_after(&ActorId::from("a"));
        assert_eq!(after_a.len(), 2);
        assert_eq!(after_a[0].as_str(), "b");
        assert_eq!(after_a[1].as_str(), "c");

        let after_c = reg.children_after(&ActorId::from("c"));
        assert!(after_c.is_empty());
    }

    #[tokio::test]
    async fn registry_all_ids() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("x"));
        reg.register(dummy_child("y"));
        let ids: Vec<String> = reg
            .all_ids()
            .iter()
            .map(|id| id.as_str().to_string())
            .collect();
        assert_eq!(ids, vec!["x", "y"]);
    }

    #[tokio::test]
    async fn registry_update_restarted_accepts_pending_seq_only() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child_with("a", RestartType::Permanent, false));
        let seq = 7u64;
        reg.get_mut(&ActorId::from("a"))
            .unwrap()
            .transition(ChildLifecycle::Restarting {
                incarnation: 0,
                next: seq,
            })
            .unwrap();

        // Stale seq rejected
        assert!(!reg.update_restarted(&ActorId::from("a"), seq + 99));

        // Matching seq accepted; incarnation adopted
        assert!(reg.update_restarted(&ActorId::from("a"), seq));
        let child = reg.get(&ActorId::from("a")).unwrap();
        assert!(matches!(child.lifecycle, ChildLifecycle::Running(_)));
        assert_eq!(child.lifecycle.occupant(), Some(seq));
        assert_eq!(
            rules::report_class(child.lifecycle, seq),
            ReportClass::Occupant
        );
        assert_eq!(rules::report_class(child.lifecycle, 0), ReportClass::Stale);
    }

    // - GroupRestart --------------------------------------------------------

    #[test]
    fn an_end_settles_only_the_awaited_incarnation() {
        let mut group = GroupRestart {
            awaiting: HashMap::from([(ActorId::from("a"), 3)]),
            stop_queue: VecDeque::new(),
            head_reached: false,
            restart_order: Vec::new(),
            manual_overrides: HashMap::new(),
            scope: GroupScope::default(),
        };
        let a = ActorId::from("a");
        // The end of another incarnation under the same id awaits nothing,
        // a newer one or a retired one.
        assert!(!group.settle_awaited(&a, 9));
        assert!(!group.settle_awaited(&a, 2));
        assert!(!group.settle_awaited(&ActorId::from("b"), 3));
        assert_eq!(group.awaiting.get(&a), Some(&3));
        // The end of the awaited incarnation settles it, once.
        assert!(group.settle_awaited(&a, 3));
        assert!(group.awaiting.is_empty());
        assert!(!group.settle_awaited(&a, 3));
    }

    // - group membership -------------------------------------------------------

    fn a_group_state(
        life: Option<ChildLifecycle>,
        group: GroupPhase,
    ) -> (SupervisionState, ActorId) {
        let mut sup = make_sup_state(RestartStrategy::OneForAll);
        let id = ActorId::from("m");
        if let Some(life) = life {
            sup.registry
                .register(dummy_child_with("m", RestartType::Permanent, false));
            sup.registry.get_mut(&id).unwrap().lifecycle = life;
        }
        sup.pending_group = Some(group);
        (sup, id)
    }

    fn stop_phase(
        awaiting: &[(&str, u64)],
        restart_order: &[&str],
        overrides: &[(&str, u64)],
    ) -> GroupPhase {
        let ids = |ids: &[&str]| ids.iter().map(|id| ActorId::from(*id)).collect::<Vec<_>>();
        GroupPhase::Stopping(GroupRestart {
            awaiting: awaiting
                .iter()
                .map(|(id, n)| (ActorId::from(*id), *n))
                .collect(),
            stop_queue: VecDeque::new(),
            head_reached: false,
            restart_order: ids(restart_order),
            manual_overrides: overrides
                .iter()
                .map(|(id, n)| (ActorId::from(*id), *n))
                .collect(),
            scope: GroupScope::default(),
        })
    }

    fn chain_phase(queue: &[&str], overrides: &[(&str, u64)]) -> GroupPhase {
        GroupPhase::Restarting {
            queue: queue.iter().map(|id| ActorId::from(*id)).collect(),
            overrides: overrides
                .iter()
                .map(|(id, n)| (ActorId::from(*id), *n))
                .collect(),
            started: HashMap::new(),
            scope: GroupScope::default(),
        }
    }

    fn down(n: u64, pending: DownPending) -> Option<ChildLifecycle> {
        Some(ChildLifecycle::Down {
            incarnation: n,
            pending,
        })
    }

    #[test]
    fn a_member_is_released_only_while_its_released_incarnation_is_down_or_stopping_for_good() {
        for phase in [
            || stop_phase(&[], &[], &[("m", 5)]),
            || chain_phase(&[], &[("m", 5)]),
        ] {
            let membership = |life| {
                let (sup, id) = a_group_state(life, phase());
                sup.group_membership(&id)
            };
            // The released incarnation, down: released, whatever it owes.
            assert_eq!(
                membership(down(5, DownPending::Snapshot)),
                GroupMembership::Released
            );
            assert_eq!(
                membership(down(5, DownPending::None)),
                GroupMembership::Released
            );
            // The released incarnation, stopped for good by a terminate, is
            // released too; a bounce in progress never is.
            assert_eq!(
                membership(Some(ChildLifecycle::Stopping {
                    incarnation: 5,
                    kind: ManualStop::Terminate
                })),
                GroupMembership::Released
            );
            assert_eq!(
                membership(Some(ChildLifecycle::Stopping {
                    incarnation: 5,
                    kind: ManualStop::Bounce
                })),
                GroupMembership::None
            );
            assert_eq!(
                membership(Some(ChildLifecycle::Stopping {
                    incarnation: 6,
                    kind: ManualStop::Terminate
                })),
                GroupMembership::None
            );
            // Another incarnation under the same id is not a member, down or
            // not, and neither is a released incarnation that runs or restarts.
            assert_eq!(
                membership(down(6, DownPending::None)),
                GroupMembership::None
            );
            assert_eq!(
                membership(Some(ChildLifecycle::Running(6))),
                GroupMembership::None
            );
            assert_eq!(
                membership(Some(ChildLifecycle::Running(5))),
                GroupMembership::None
            );
            assert_eq!(
                membership(Some(ChildLifecycle::Restarting {
                    incarnation: 5,
                    next: 6
                })),
                GroupMembership::None
            );
            assert_eq!(membership(None), GroupMembership::None);
        }
    }

    #[test]
    fn a_retry_leaves_down_only_the_incarnation_a_terminate_named() {
        let overrides = [("m", 5)];
        let plan = |life| {
            let (sup, _id) = a_group_state(life, chain_phase(&["m"], &overrides));
            sup.plan_retry(&ActorId::from("m"))
        };
        // The ledger holds the incarnation the terminate named: the retry
        // leaves the key down.
        assert!(plan(down(5, DownPending::None)).restart_order.is_empty());
        // The ledger holds another incarnation under the id: the key is not
        // the one released, and the retry restarts it.
        assert_eq!(
            plan(down(6, DownPending::None)).restart_order,
            vec![ActorId::from("m")]
        );
    }

    #[test]
    fn a_member_is_awaited_only_while_the_ledger_holds_the_awaited_incarnation() {
        let phase = || stop_phase(&[("m", 5)], &[], &[]);
        let membership = |life| {
            let (sup, id) = a_group_state(life, phase());
            sup.group_membership(&id)
        };
        let awaited = GroupMembership::Awaited { overridden: false };
        assert_eq!(membership(Some(ChildLifecycle::Running(5))), awaited);
        // A death that owes its decision is still awaited: the group folds it.
        assert_eq!(membership(down(5, DownPending::Evaluate)), awaited);
        // A child spawned again under the id holds another incarnation.
        assert_eq!(
            membership(Some(ChildLifecycle::Running(6))),
            GroupMembership::None
        );
        assert_eq!(
            membership(down(6, DownPending::None)),
            GroupMembership::None
        );
        assert_eq!(membership(None), GroupMembership::None);
        // An override recorded for the awaited incarnation shows; one for
        // another incarnation does not.
        let (sup, id) = a_group_state(
            Some(ChildLifecycle::Running(5)),
            stop_phase(&[("m", 5)], &[], &[("m", 5)]),
        );
        assert_eq!(
            sup.group_membership(&id),
            GroupMembership::Awaited { overridden: true }
        );
        let (sup, id) = a_group_state(
            Some(ChildLifecycle::Running(5)),
            stop_phase(&[("m", 5)], &[], &[("m", 4)]),
        );
        assert_eq!(sup.group_membership(&id), awaited);
    }

    #[test]
    fn a_member_whose_end_was_handled_is_no_longer_awaited() {
        // The end of the awaited incarnation was classified by a completed
        // `terminate_child`: a snapshot of it is pending, and the member is
        // released.
        let (mut sup, id) = a_group_state(
            down(5, DownPending::Snapshot),
            stop_phase(&[("m", 5)], &[], &[("m", 5)]),
        );
        sup.pending_manual_events.insert(
            (id.clone(), 5),
            ChildEvent {
                child_id: id.clone(),
                child_name: None,
                reason: StopReason::Kill,
                action: crate::types::SupervisionAction::Removed,
            },
        );
        assert_eq!(sup.group_membership(&id), GroupMembership::Released);
        // Without the snapshot the end is still awaited.
        sup.pending_manual_events.clear();
        assert_eq!(
            sup.group_membership(&id),
            GroupMembership::Awaited { overridden: true }
        );
    }

    #[test]
    fn a_member_of_the_restart_set_is_queued_before_it_is_released() {
        let (sup, id) = a_group_state(
            down(5, DownPending::None),
            stop_phase(&[], &["m"], &[("m", 5)]),
        );
        assert_eq!(sup.group_membership(&id), GroupMembership::Queued);
        let (sup, id) = a_group_state(down(5, DownPending::None), chain_phase(&["m"], &[]));
        assert_eq!(sup.group_membership(&id), GroupMembership::Queued);
        let (sup, id) = a_group_state(down(5, DownPending::None), stop_phase(&[], &[], &[]));
        assert_eq!(sup.group_membership(&id), GroupMembership::None);
        let mut sup = make_sup_state(RestartStrategy::OneForAll);
        sup.registry
            .register(dummy_child_with("m", RestartType::Permanent, false));
        assert_eq!(
            sup.group_membership(&ActorId::from("m")),
            GroupMembership::None,
            "no group is pending"
        );
    }

    #[test]
    fn a_removed_spec_leaves_the_pending_groups_sets() {
        let (mut sup, id) = a_group_state(
            down(5, DownPending::Snapshot),
            stop_phase(&[("m", 5)], &["m", "n"], &[("m", 5)]),
        );
        drop(sup.remove_spec(&id));
        assert!(sup.registry.get(&id).is_none());
        let Some(GroupPhase::Stopping(group)) = sup.pending_group.as_ref() else {
            panic!("the group still pends");
        };
        assert_eq!(group.restart_order, vec![ActorId::from("n")]);
        assert!(group.manual_overrides.is_empty());
        // The barrier entry stays: it is the end of the removed child's last
        // incarnation.
        assert_eq!(group.awaiting.get(&id), Some(&5));
        let (mut sup, id) =
            a_group_state(down(5, DownPending::None), chain_phase(&["m"], &[("m", 5)]));
        drop(sup.remove_spec(&id));
        let Some(GroupPhase::Restarting {
            queue, overrides, ..
        }) = sup.pending_group.as_ref()
        else {
            panic!("the group still pends");
        };
        assert!(queue.is_empty() && overrides.is_empty());
    }

    // - the restart chain ------------------------------------------------------

    fn a_chain_with_front(life: Option<ChildLifecycle>, started: Option<u64>) -> SupervisionState {
        let mut sup = make_sup_state(RestartStrategy::OneForAll);
        if let Some(life) = life {
            sup.registry
                .register(dummy_child_with("m", RestartType::Permanent, false));
            sup.registry.get_mut(&ActorId::from("m")).unwrap().lifecycle = life;
        }
        let mut phase = chain_phase(&["m"], &[]);
        if let (GroupPhase::Restarting { started: map, .. }, Some(token)) = (&mut phase, started) {
            map.insert(ActorId::from("m"), token);
        }
        sup.pending_group = Some(phase);
        sup
    }

    #[tokio::test]
    async fn the_chain_decides_its_front_member_by_the_ledger() {
        let mut work = VecDeque::new();
        // A down member is started.
        let mut sup = a_chain_with_front(down(1, DownPending::None), None);
        assert!(matches!(
            advance_restart_chain(&mut sup, &mut work),
            ChainStep::Initiate(id) if id.as_str() == "m"
        ));
        // A member running the incarnation the chain started is done, and the
        // group completes with it.
        let mut sup = a_chain_with_front(Some(ChildLifecycle::Running(7)), Some(7));
        let queued = ChildStoppedInternal {
            child_id: ActorId::from("x"),
            reason: StopReason::Kill,
            incarnation: 1,
        };
        sup.queued_triggers.push_back(queued);
        assert!(matches!(
            advance_restart_chain(&mut sup, &mut work),
            ChainStep::Done
        ));
        assert!(sup.pending_group.is_none());
        assert_eq!(work.len(), 1, "the queued trigger is released");
        // A member running an incarnation the chain did not start is stopped
        // and then started in its slot. The group stops every incarnation it
        // laid out before the chain begins, so the chain does not meet one;
        // the decision is read from the pure rule.
        for started in [None, Some(6)] {
            assert_eq!(
                rules::chain_front(ChildLifecycle::Running(7), started),
                rules::FrontAction::StopThenInitiate
            );
        }
        assert_eq!(
            rules::chain_front(ChildLifecycle::Running(7), Some(7)),
            rules::FrontAction::Fresh
        );
        // A member being stopped, or with the attempt the chain started in
        // flight, holds the chain without any order.
        for life in [
            ChildLifecycle::Stopping {
                incarnation: 7,
                kind: ManualStop::Bounce,
            },
            ChildLifecycle::Restarting {
                incarnation: 7,
                next: 8,
            },
        ] {
            let mut sup = a_chain_with_front(Some(life), Some(8));
            assert!(matches!(
                advance_restart_chain(&mut sup, &mut work),
                ChainStep::Hold
            ));
            let child = sup.registry.get(&ActorId::from("m")).unwrap();
            assert!(!child.ctl.has_order());
        }
        // A spec that is gone is skipped.
        let mut sup = a_chain_with_front(None, None);
        assert!(matches!(
            advance_restart_chain(&mut sup, &mut work),
            ChainStep::Done
        ));
    }

    /// The members the group waits for (reverse start order) and the members
    /// it restarts (start order) a group commit names.
    fn stops_and_restarts(
        commit: &[(ActorId, u64, rules::MemberAction)],
    ) -> (Vec<ActorId>, Vec<ActorId>) {
        let stops = commit
            .iter()
            .rev()
            .filter(|(_, _, action)| action.held)
            .map(|(id, _, _)| id.clone())
            .collect();
        let restarts = commit
            .iter()
            .filter(|(_, _, action)| action.restarts)
            .map(|(id, _, _)| id.clone())
            .collect();
        (stops, restarts)
    }

    // - evaluate_strategy ---------------------------------------------------

    fn make_sup_state(strategy: RestartStrategy) -> SupervisionState {
        SupervisionState::new(SupervisionConfig {
            strategy,
            max_restarts: 3,
            restart_window: Duration::from_secs(60),
        })
    }

    #[tokio::test]
    async fn strategy_permanent_restarts_on_any_reason() {
        let mut sup = make_sup_state(RestartStrategy::OneForOne);
        sup.registry.register(dummy_child("child"));
        match evaluate_strategy(&mut sup, &ActorId::from("child"), &StopReason::Graceful) {
            StrategyOutcome::RestartOne(id) => assert_eq!(id.as_str(), "child"),
            _ => panic!("expected RestartOne"),
        }
    }

    #[tokio::test]
    async fn strategy_transient_removes_on_clean_reasons_restarts_on_abnormal() {
        let mut sup = make_sup_state(RestartStrategy::OneForOne);
        sup.registry
            .register(dummy_child_with("child", RestartType::Transient, true));
        let id = ActorId::from("child");
        // Clean: normal (Graceful) and shutdown (ParentRequest) stay down.
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::Graceful),
            StrategyOutcome::Remove
        ));
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::ParentRequest),
            StrategyOutcome::Remove
        ));
        // Abnormal: killed restarts (OTP `killed` is abnormal).
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::Kill),
            StrategyOutcome::RestartOne(_)
        ));
    }

    #[tokio::test]
    async fn strategy_temporary_always_removes() {
        let mut sup = make_sup_state(RestartStrategy::OneForOne);
        sup.registry
            .register(dummy_child_with("child", RestartType::Temporary, true));
        assert!(matches!(
            evaluate_strategy(&mut sup, &ActorId::from("child"), &StopReason::Kill),
            StrategyOutcome::Remove
        ));
    }

    #[tokio::test]
    async fn strategy_one_for_all_stops_live_reverse_restarts_forward() {
        let mut sup = make_sup_state(RestartStrategy::OneForAll);
        sup.registry.register(dummy_child("a"));
        sup.registry.register(dummy_child("b"));
        sup.registry.register(dummy_child("c"));
        // b failed (already dead)
        sup.registry.get_mut(&ActorId::from("b")).unwrap().lifecycle = ChildLifecycle::Down {
            incarnation: 0,
            pending: DownPending::None,
        };
        match evaluate_strategy(&mut sup, &ActorId::from("b"), &StopReason::Kill) {
            StrategyOutcome::RestartGroup { commit } => {
                let (stop_reverse, restart_order) = stops_and_restarts(&commit);
                let stops: Vec<&str> = stop_reverse.iter().map(|i| i.as_str()).collect();
                assert_eq!(stops, vec!["c", "a"], "live members, reverse start order");
                let restarts: Vec<&str> = restart_order.iter().map(|i| i.as_str()).collect();
                assert_eq!(restarts, vec!["a", "b", "c"], "all members, start order");
            }
            _ => panic!("expected RestartGroup"),
        }
    }

    #[tokio::test]
    async fn strategy_rest_for_one_affects_failed_and_later() {
        let mut sup = make_sup_state(RestartStrategy::RestForOne);
        sup.registry.register(dummy_child("a"));
        sup.registry.register(dummy_child("b"));
        sup.registry.register(dummy_child("c"));
        sup.registry.get_mut(&ActorId::from("a")).unwrap().lifecycle = ChildLifecycle::Down {
            incarnation: 0,
            pending: DownPending::None,
        };
        match evaluate_strategy(&mut sup, &ActorId::from("a"), &StopReason::Kill) {
            StrategyOutcome::RestartGroup { commit } => {
                let (stop_reverse, restart_order) = stops_and_restarts(&commit);
                let stops: Vec<&str> = stop_reverse.iter().map(|i| i.as_str()).collect();
                assert_eq!(stops, vec!["c", "b"]);
                let restarts: Vec<&str> = restart_order.iter().map(|i| i.as_str()).collect();
                assert_eq!(restarts, vec!["a", "b", "c"]);
            }
            _ => panic!("expected RestartGroup"),
        }
    }

    #[tokio::test]
    async fn strategy_group_excludes_temporary_from_restart_but_stops_it() {
        let mut sup = make_sup_state(RestartStrategy::OneForAll);
        sup.registry.register(dummy_child("a"));
        sup.registry
            .register(dummy_child_with("tmp", RestartType::Temporary, true));
        sup.registry.get_mut(&ActorId::from("a")).unwrap().lifecycle = ChildLifecycle::Down {
            incarnation: 0,
            pending: DownPending::None,
        };
        match evaluate_strategy(&mut sup, &ActorId::from("a"), &StopReason::Kill) {
            StrategyOutcome::RestartGroup { commit } => {
                let (stop_reverse, restart_order) = stops_and_restarts(&commit);
                let stops: Vec<&str> = stop_reverse.iter().map(|i| i.as_str()).collect();
                assert_eq!(stops, vec!["tmp"], "temporary sibling is stopped");
                let restarts: Vec<&str> = restart_order.iter().map(|i| i.as_str()).collect();
                assert_eq!(restarts, vec!["a"], "temporary sibling is not restarted");
            }
            _ => panic!("expected RestartGroup"),
        }
    }

    #[tokio::test]
    async fn strategy_budget_exhausted() {
        let mut sup = SupervisionState::new(SupervisionConfig {
            strategy: RestartStrategy::OneForOne,
            max_restarts: 1,
            restart_window: Duration::from_secs(60),
        });
        sup.registry.register(dummy_child("child"));
        // First restart - uses the budget
        assert!(matches!(
            evaluate_strategy(&mut sup, &ActorId::from("child"), &StopReason::Kill),
            StrategyOutcome::RestartOne(_)
        ));
        // Second restart - budget exhausted
        assert!(matches!(
            evaluate_strategy(&mut sup, &ActorId::from("child"), &StopReason::Kill),
            StrategyOutcome::BudgetExhausted
        ));
    }

    // A restart_window wider than any representable Instant forces
    // checked_sub to return None on every single check - the same underflow
    // a paused/simulated clock sitting close to its own epoch can trigger
    // (this crate does not depend on tokio's test-util feature, so the
    // window itself is used to force the underflow deterministically rather
    // than driving it through tokio::time::pause()). Two restarts fired
    // back-to-back with no await between them (genuinely zero elapsed
    // wall-clock time) must still be accounted for correctly, and the
    // arithmetic must never panic.
    #[tokio::test]
    async fn strategy_survives_zero_elapsed_restarts_with_underflowing_window() {
        let mut sup = SupervisionState::new(SupervisionConfig {
            strategy: RestartStrategy::OneForOne,
            max_restarts: 2,
            restart_window: Duration::MAX,
        });
        sup.registry.register(dummy_child("child"));
        let id = ActorId::from("child");

        // Two immediate restarts consume the budget, no panic on either.
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::Kill),
            StrategyOutcome::RestartOne(_)
        ));
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::Kill),
            StrategyOutcome::RestartOne(_)
        ));
        // A third restart, still with zero elapsed time, is correctly
        // denied: the count-based gate works even though the time-based
        // prune step above never fires (cutoff is None on every call).
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::Kill),
            StrategyOutcome::BudgetExhausted
        ));
    }

    // - ChildLinkGuard / death plane -----------------------------------------

    /// A dummy long-running task whose `AbortHandle` and `JoinHandle` a test
    /// can use to observe whether the reaper (or a guard's degrade path)
    /// actually terminated it.
    fn spawn_forever() -> (JoinHandle<()>, AbortHandle) {
        let join = tokio::spawn(std::future::pending::<()>());
        let abort = join.abort_handle();
        (join, abort)
    }

    /// A fresh, uniquely named system for a `ChildLinkGuard` to carry -
    /// these tests only need a system that exists, never one that hosts a
    /// particular actor.
    fn dummy_system() -> Arc<ActorSystem> {
        ActorSystem::create(format!("guard-test-{}", uuid::Uuid::new_v4())).unwrap()
    }

    #[tokio::test]
    async fn guard_drop_raises_kill_and_feeds_the_reaper_exactly_once() {
        let (lane, lane_rx) = StopLane::new();
        let (_status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
        let (_join, abort) = spawn_forever();
        let (reaper_tx, mut reaper_rx) = mpsc::unbounded_channel();

        let guard = ChildLinkGuard::new(
            lane.clone(),
            abort,
            reaper_tx,
            crate::actor::supervision::rules::InheritedStop::Kill {
                grace: Duration::from_millis(50),
            },
            dummy_system(),
            Handle::current(),
        );
        drop(guard);

        assert!(
            matches!(lane_rx.borrow().reason, Some(StopReason::Kill)),
            "drop must raise Kill on the child's stop lane"
        );
        assert_eq!(
            status_rx.borrow().status,
            ActorStatus::Running,
            "drop must leave the child's status watch untouched: only its \
             own run loop, or its task ending, ever writes a terminal status"
        );
        assert!(
            reaper_rx.try_recv().is_ok(),
            "drop must feed exactly one entry to the reaper"
        );
        assert!(
            reaper_rx.try_recv().is_err(),
            "drop must feed the reaper only once"
        );
    }

    #[tokio::test]
    async fn guard_drop_effects_are_harmless_on_an_already_stopped_child() {
        // Every effect a guard's drop performs is safe to run again after
        // the child already reported its own terminal state: this is
        // exactly what happens when a runner completes normally (the
        // captured guard still drops right after).
        let (lane, lane_rx) = StopLane::new();
        lane.raise(StopReason::Graceful);
        let (_join, abort) = spawn_forever();
        abort.abort();
        let (reaper_tx, mut reaper_rx) = mpsc::unbounded_channel();

        let guard = ChildLinkGuard::new(
            lane,
            abort,
            reaper_tx,
            crate::actor::supervision::rules::InheritedStop::Kill {
                grace: DEFAULT_KILL_GRACE,
            },
            dummy_system(),
            Handle::current(),
        );
        drop(guard);

        assert!(matches!(lane_rx.borrow().reason, Some(StopReason::Kill)));
        assert!(reaper_rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn guard_disarmed_by_the_runner_is_inert_on_normal_completion() {
        let (lane, _lane_rx) = StopLane::new();
        let child_join: JoinHandle<StopReason> = tokio::spawn(async { StopReason::Graceful });
        let abort = child_join.abort_handle();
        let (reaper_tx, mut reaper_rx) = mpsc::unbounded_channel();
        let guard = ChildLinkGuard::new(
            lane.clone(),
            abort,
            reaper_tx,
            crate::actor::supervision::rules::InheritedStop::Kill {
                grace: DEFAULT_KILL_GRACE,
            },
            dummy_system(),
            Handle::current(),
        );
        let (ctl, io) = slot_pair(Default::default());
        let mut fate_rx = ctl.cell();

        let mut death_set: JoinSet<DeathOutcome> = JoinSet::new();
        spawn_runner(
            &mut death_set,
            ActorId::from("normal-death"),
            0,
            child_join,
            lane,
            io,
            guard,
            DEFAULT_KILL_GRACE,
        );

        let (id, reason, incarnation) = death_set.join_next().await.unwrap().unwrap();
        assert_eq!(id.as_str(), "normal-death");
        assert!(matches!(reason, StopReason::Graceful));
        assert_eq!(incarnation, 0);

        assert!(fate_rx.wait_for(|cell| cell.fate.is_some()).await.is_ok());
        assert!(
            reaper_rx.try_recv().is_err(),
            "a disarmed guard must not occupy the reaper on normal completion"
        );
    }

    #[tokio::test]
    async fn guard_fires_exactly_once_when_aborted_before_its_runner_is_ever_polled() {
        // Single-threaded (current_thread) test runtime: a newly spawned
        // task is never polled until this task yields, so aborting the
        // JoinSet with no `.await` in between reliably exercises the
        // abort-before-first-poll path.
        let (lane, lane_rx) = StopLane::new();
        let child_join: JoinHandle<StopReason> = tokio::spawn(std::future::pending());
        let abort = child_join.abort_handle();
        let (reaper_tx, mut reaper_rx) = mpsc::unbounded_channel();
        let guard = ChildLinkGuard::new(
            lane.clone(),
            abort,
            reaper_tx,
            crate::actor::supervision::rules::InheritedStop::Kill {
                grace: DEFAULT_KILL_GRACE,
            },
            dummy_system(),
            Handle::current(),
        );
        let (ctl, io) = slot_pair(Default::default());

        let mut death_set: JoinSet<DeathOutcome> = JoinSet::new();
        spawn_runner(
            &mut death_set,
            ActorId::from("never-polled"),
            0,
            child_join,
            lane,
            io,
            guard,
            DEFAULT_KILL_GRACE,
        );
        death_set.abort_all();

        // Drains the aborted runner so its drop glue (the captured guard,
        // included) actually runs before the assertions below.
        while death_set.join_next().await.is_some() {}

        assert!(
            matches!(lane_rx.borrow().reason, Some(StopReason::Kill)),
            "the guard must still fire even though its runner never polled the child's task"
        );
        assert!(reaper_rx.try_recv().is_ok());
        assert!(reaper_rx.try_recv().is_err());
        // The fate cell is untouched: only the runner's own completion
        // writes it, and it never got that far.
        assert!(ctl.fate().is_none());
    }

    // - The guard of a child that is itself a supervisor --------------------

    use crate::actor::supervision::rules::InheritedStop;

    fn orderly(window: Option<Duration>, grace: Duration) -> InheritedStop {
        InheritedStop::Orderly { window, grace }
    }

    /// Lets the runtime run everything that is ready, including the reaper.
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn guard_drop_of_a_supervisor_child_raises_parent_killed_then_kill_then_the_abort() {
        let sys = dummy_system();
        let (lane, lane_rx) = StopLane::new();
        let (join, abort) = spawn_forever();
        let window = Duration::from_millis(400);
        let grace = Duration::from_millis(150);
        let guard = ChildLinkGuard::new(
            lane.clone(),
            abort,
            sys.reaper_handle(&Handle::current()),
            orderly(Some(window), grace),
            sys.clone(),
            Handle::current(),
        );
        drop(guard);
        settle().await;

        assert!(
            matches!(lane_rx.borrow().reason, Some(StopReason::ParentKilled)),
            "a supervisor child is raised ParentKilled, not Kill"
        );
        // Inside the window nothing more happens.
        tokio::time::advance(window - Duration::from_millis(10)).await;
        settle().await;
        assert!(matches!(
            lane_rx.borrow().reason,
            Some(StopReason::ParentKilled)
        ));
        assert!(!join.is_finished());
        // At the end of the window the reaper raises Kill and leaves the
        // task running for its kill grace.
        tokio::time::advance(Duration::from_millis(10)).await;
        settle().await;
        assert!(matches!(lane_rx.borrow().reason, Some(StopReason::Kill)));
        assert!(!join.is_finished(), "the task is aborted after its grace");
        tokio::time::advance(grace - Duration::from_millis(10)).await;
        settle().await;
        assert!(!join.is_finished());
        tokio::time::advance(Duration::from_millis(10)).await;
        let ended = tokio::time::timeout(Duration::from_secs(1), join)
            .await
            .expect("the task is aborted a grace after the Kill");
        assert!(ended.unwrap_err().is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn guard_drop_of_a_supervisor_child_whose_lane_holds_a_kill_keeps_that_kill_s_abort() {
        // A `Kill` the child's runner or restart attempt raised is owed an
        // abort a kill grace after it, whatever the child's `Shutdown`: the
        // guard that fires then feeds the worker's entry.
        let grace = Duration::from_millis(150);
        for window in [None, Some(Duration::ZERO), Some(Duration::from_secs(2))] {
            let sys = dummy_system();
            let (lane, lane_rx) = StopLane::new();
            lane.raise(StopReason::Kill);
            let (join, abort) = spawn_forever();
            let guard = ChildLinkGuard::new(
                lane.clone(),
                abort,
                sys.reaper_handle(&Handle::current()),
                orderly(window, grace),
                sys.clone(),
                Handle::current(),
            );
            drop(guard);
            settle().await;
            assert!(
                matches!(lane_rx.borrow().reason, Some(StopReason::Kill)),
                "the Kill stays on the lane (window {window:?})"
            );
            tokio::time::advance(grace - Duration::from_millis(10)).await;
            settle().await;
            assert!(!join.is_finished(), "window {window:?}");
            tokio::time::advance(Duration::from_millis(10)).await;
            let ended = tokio::time::timeout(Duration::from_secs(1), join)
                .await
                .unwrap_or_else(|_| panic!("aborted a grace after the guard (window {window:?})"));
            assert!(ended.unwrap_err().is_cancelled());
        }
    }

    #[tokio::test]
    async fn guard_drop_of_a_supervisor_child_under_infinity_feeds_no_entry() {
        let (lane, lane_rx) = StopLane::new();
        let (join, abort) = spawn_forever();
        let (reaper_tx, mut reaper_rx) = mpsc::unbounded_channel();
        let guard = ChildLinkGuard::new(
            lane,
            abort,
            reaper_tx,
            orderly(None, DEFAULT_KILL_GRACE),
            dummy_system(),
            Handle::current(),
        );
        drop(guard);

        assert!(matches!(
            lane_rx.borrow().reason,
            Some(StopReason::ParentKilled)
        ));
        assert!(
            reaper_rx.try_recv().is_err(),
            "an unbounded orderly stop has no deadline to keep"
        );
        settle().await;
        assert!(!join.is_finished(), "nothing aborts it");
    }

    #[tokio::test]
    async fn guard_drop_of_a_supervisor_child_under_kill_raises_kill_at_once_after_parent_killed() {
        let sys = dummy_system();
        let (lane, lane_rx) = StopLane::new();
        let (join, abort) = spawn_forever();
        let grace = Duration::from_secs(60);
        let guard = ChildLinkGuard::new(
            lane.clone(),
            abort,
            sys.reaper_handle(&Handle::current()),
            orderly(Some(Duration::ZERO), grace),
            sys.clone(),
            Handle::current(),
        );
        // The first thing a watcher of the lane can read is the orderly
        // stop; the reaper raises the Kill from its own task.
        drop(guard);
        let mut seen = vec![lane_rx.borrow().reason.clone()];
        for _ in 0..400 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let now = lane_rx.borrow().reason.clone();
            if seen.last() != Some(&now) {
                seen.push(now);
            }
            if matches!(seen.last(), Some(Some(StopReason::Kill))) {
                break;
            }
        }
        assert!(
            matches!(seen[0], Some(StopReason::ParentKilled)),
            "{seen:?}"
        );
        assert!(
            matches!(seen.last(), Some(Some(StopReason::Kill))),
            "a zero window raises the Kill from the reaper at once: {seen:?}"
        );
        // The abort still follows the grace, never the Kill itself.
        assert!(!join.is_finished());
        join.abort();
    }

    #[tokio::test]
    async fn guard_drop_with_an_edge_of_range_window_and_grace_does_not_panic() {
        let base = Instant::now();
        let mut lo = Duration::ZERO;
        let mut hi = Duration::from_secs(u64::MAX / 2);
        while hi - lo > Duration::from_nanos(1) {
            let mid = lo + (hi - lo) / 2;
            if base.checked_add(mid).is_some() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        for window in [lo - Duration::from_micros(500), Duration::MAX] {
            for grace in [Duration::from_millis(120), lo, Duration::MAX] {
                let (lane, lane_rx) = StopLane::new();
                let (join, abort) = spawn_forever();
                let (reaper_tx, mut reaper_rx) = mpsc::unbounded_channel();
                let guard = ChildLinkGuard::new(
                    lane,
                    abort,
                    reaper_tx,
                    orderly(Some(window), grace),
                    dummy_system(),
                    Handle::current(),
                );
                drop(guard);
                assert!(
                    matches!(lane_rx.borrow().reason, Some(StopReason::ParentKilled)),
                    "window {window:?} grace {grace:?}"
                );
                assert!(reaper_rx.try_recv().is_ok(), "the entry is fed");
                join.abort();
            }
        }
    }

    // - The kill grace ----------------------------------------------------------

    /// Spawns two children, one with no `kill_grace` call and one with its
    /// own, and reports the grace each stored in its spec.
    struct SpecReader {
        seen: std::sync::Arc<std::sync::Mutex<Vec<Duration>>>,
    }

    #[derive(Default)]
    struct Leaf;

    impl crate::actor::Actor for Leaf {
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

    impl crate::actor::Actor for SpecReader {
        type Message = ();
        type Response = ();

        async fn on_started(
            &mut self,
            ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            ctx.spawn_child(Leaf::default).named("plain").await?;
            ctx.spawn_child(Leaf::default)
                .named("graced")
                .kill_grace(Duration::from_millis(700))
                .await?;
            let sup = ctx.supervision_ref().expect("a supervisor");
            let mut seen = self.seen.lock().unwrap();
            for name in ["plain", "graced"] {
                seen.push(
                    sup.registry
                        .get(&ActorId::from(name))
                        .expect("the child is registered")
                        .spec
                        .kill_grace,
                );
            }
            Ok(())
        }

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn default_kill_grace_is_120_ms_and_abort_settle_is_100_ms() {
        use crate::actor::ActorExt;

        assert_eq!(DEFAULT_KILL_GRACE, Duration::from_millis(120));
        assert_eq!(ABORT_SETTLE, Duration::from_millis(100));

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sys = ActorSystem::create(format!("spec-grace-{}", uuid::Uuid::new_v4())).unwrap();
        let _sup = SpecReader { seen: seen.clone() }
            .spawn()
            .named("spec-reader")
            .on_system(&sys)
            .supervisor()
            .await
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![DEFAULT_KILL_GRACE, Duration::from_millis(700)],
            "a child with no kill_grace call stores the default; one with a call, its own"
        );
    }
}

#[cfg(test)]
mod explore;

#[cfg(test)]
pub(crate) mod lost_runner;
