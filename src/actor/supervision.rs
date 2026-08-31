//! Supervision configuration, restart budgets, and child registry.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio::time::Instant;

use crate::system::ReaperEntry;
use crate::types::{
    ActorId, ActorStatus, ChildEvent, ChildFate, ChildInfo, ChildStoppedInternal, RestartStrategy,
    RestartType, Shutdown, StopLane, StopReason,
};

/// Type-erased restart function stored per child.
///
/// The closure captures the child's original [`ActorId`], name, and the full
/// resolved [`ActorConfig`](crate::actor::runtime::ActorConfig) by value at
/// `spawn_child` time (OTP child-spec immutability), so every restart reuses
/// the exact spec. Given the restart sequence token, it produces the fresh
/// attempt's [`RestartOutcome`] - the value the parent's restart plane
/// (`SupervisionState::restart_set`) returns to the run loop once the
/// attempt is decided one way or the other.
pub(crate) type RestartFn =
    Box<dyn Fn(u64) -> Pin<Box<dyn Future<Output = RestartOutcome> + Send>> + Send + Sync>;

/// Outcome of one restart attempt, produced by a task in the parent's
/// restart plane (`SupervisionState::restart_set`) and consumed by the run
/// loop.
///
/// [`Adopted`](Self::Adopted) carries every per-incarnation handle the
/// adoption path needs (watcher, fate cell, and link guard are rebuilt from
/// these) - including the [`ChildLinkGuard`] itself, armed the instant the
/// attempt's `spawn_actor` call succeeded, inside the restart task, before
/// the child's init ack was ever awaited. Not adopting an `Adopted` value
/// (a superseded attempt, a rejected chain slot, or the whole restart plane
/// being dropped because the parent itself died) is exactly what leaves that
/// guard to run its `Drop`, killing the stray incarnation through the same
/// Kill-raise/status-force/reaper ladder every other child teardown uses.
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
    },
    /// The attempt never produced a live child (or the live child it did
    /// produce is already known fully gone by the time this is reported).
    Failed {
        child_id: ActorId,
        incarnation: u64,
        reason: StopReason,
    },
}

/// Grace given to a cooperative Kill signal before the abort() backstop fires.
/// Kill is processed with biased priority, so a responsive actor dies in
/// microseconds; the grace exists only for an actor mid-callback.
pub(crate) const KILL_GRACE: Duration = Duration::from_millis(100);

/// Grace given to a child that is itself a supervisor before the reaper
/// aborts it: enough time for its own cascade (raising Kill on its own
/// children's lanes and, in turn, on theirs) to reach every descendant
/// before its task actually exits.
pub(crate) const SUPERVISOR_KILL_GRACE: Duration = Duration::from_millis(500);

/// Outcome of one child incarnation's watcher task: the child's id, its real
/// stop reason, and the incarnation token - in the order the run loop's
/// death-plane branch consumes them.
pub(crate) type DeathOutcome = (ActorId, StopReason, u64);

/// Synchronous safety net for one child incarnation's watcher task.
///
/// Constructed in the parent's own synchronous code, before the watcher is
/// spawned, and moved into the watcher's future as a captured value. If that
/// task is ever torn down before it finishes running - most notably, aborted
/// before it is ever polled at all - Rust still runs the drop glue of
/// everything it captured, so this guard's `Drop` still fires. That is
/// exactly what happens, level by level, when a supervisor's own task dies
/// with `StopReason::Kill`: it never awaits its own children, it simply drops
/// its supervision state (its death plane included), which aborts every live
/// child's watcher task in one step.
///
/// Every effect below is safe to run more than once, and safe to run after
/// the child has already stopped on its own: raising `Kill` on an
/// already-closed lane, publishing `Stopped` over an already-`Stopped` status
/// watch, and aborting an already-finished task are all no-ops.
pub(crate) struct ChildLinkGuard {
    stop_lane: StopLane,
    status_tx: watch::Sender<ActorStatus>,
    abort: AbortHandle,
    reaper: mpsc::Sender<ReaperEntry>,
    grace: Duration,
    /// True until the child's fate has been recorded. The watcher disarms
    /// the guard immediately after writing the fate cell: at that point the
    /// child has terminated and reported on its own, so the guard's drop has
    /// no teardown left to force and does nothing. Every abnormal drop (an
    /// abort before the watcher's first poll, the owning `JoinSet` dropped
    /// with the parent, an unadopted restart outcome) still finds the guard
    /// armed and fires the full ladder.
    armed: bool,
}

impl ChildLinkGuard {
    pub(crate) fn new(
        stop_lane: StopLane,
        status_tx: watch::Sender<ActorStatus>,
        abort: AbortHandle,
        reaper: mpsc::Sender<ReaperEntry>,
        grace: Duration,
    ) -> Self {
        Self {
            stop_lane,
            status_tx,
            abort,
            reaper,
            grace,
            armed: true,
        }
    }

    /// Marks the guarded child as fully terminated and reported; the guard's
    /// drop becomes a no-op. Called by the watcher right after the fate cell
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
        // (a) Raise Kill: a plain watch write, infallible, and a no-op if
        // the child has already finished its message loop.
        self.stop_lane.raise(StopReason::Kill);
        // (b) Force-publish the terminal status: a `watch` write always
        // succeeds, even with every receiver already dropped, and overwrites
        // whatever status was last observed for a child that has not yet
        // reported its own `Stopped` (or never will, because it is being
        // aborted instead).
        self.status_tx.send_replace(ActorStatus::Stopped);
        // (c) Enqueue a grace deadline with the reaper; a full or closed
        // feed degrades to an immediate abort instead of blocking (this is
        // `Drop`: no awaiting, no spawning).
        let deadline = Instant::now() + self.grace;
        let entry = ReaperEntry::new(deadline, self.abort.clone());
        if self.reaper.try_send(entry).is_err() {
            self.abort.abort();
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

/// The lifecycle of one child spec inside its supervisor's registry.
///
/// Exactly one variant holds at any moment, and [`transition`](Self::transition)
/// is the single, default-closed authority for moving between them: every
/// arrow drawn below is explicit, and anything else is rejected outright
/// instead of silently applying.
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
    /// or reverted back to `Down` on a [`RestartOutcome::Failed`]).
    Restarting { incarnation: u64, next: u64 },
    /// The spec is retained but no instance is running. `event_pending`
    /// marks a manual completion whose `on_child_stopped` snapshot is still
    /// waiting for the run loop's death arm to observe the matching
    /// incarnation and deliver it (see [`SupervisionState::pending_manual_events`]).
    Down {
        incarnation: u64,
        event_pending: bool,
    },
}

impl ChildLifecycle {
    /// The incarnation this state currently considers its "own": the live
    /// one for `Running`, the awaited one for `Stopping`, the dead/superseded
    /// one for `Restarting` (its `next` is a distinct, not-yet-adopted token),
    /// and the retained one for `Down`.
    pub(crate) fn incarnation(&self) -> u64 {
        match self {
            ChildLifecycle::Running(incarnation)
            | ChildLifecycle::Stopping { incarnation, .. }
            | ChildLifecycle::Restarting { incarnation, .. }
            | ChildLifecycle::Down { incarnation, .. } => *incarnation,
        }
    }

    /// True if a death/fate reported for `incarnation` belongs to whichever
    /// instance this lifecycle currently treats as live or in flight: its own
    /// incarnation always, plus a `Restarting` attempt's not-yet-adopted
    /// `next` token (so that attempt's own failure report is not mistaken for
    /// a death of some other, unrelated incarnation).
    pub(crate) fn accepts_incarnation(&self, incarnation: u64) -> bool {
        self.incarnation() == incarnation
            || matches!(self, ChildLifecycle::Restarting { next, .. } if *next == incarnation)
    }

    /// The default-closed transition table: every legal move between child
    /// lifecycle states, listed once. Anything not matched here - a repeated
    /// commit, a direct `Down` -> `Running` skip, restarting while already
    /// restarting, and so on - is rejected.
    fn is_legal(from: &ChildLifecycle, to: &ChildLifecycle) -> bool {
        use ChildLifecycle::*;
        matches!(
            (from, to),
            (Running(_), Stopping { .. })
                | (Running(_), Down { .. })
                | (Running(_), Restarting { .. })
                | (Stopping { .. }, Down { .. })
                | (Stopping { .. }, Restarting { .. })
                | (Down { .. }, Restarting { .. })
                | (Down { .. }, Down { .. })
                | (Restarting { .. }, Running(_))
                | (Restarting { .. }, Down { .. })
        )
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
    /// The sliding window for counting restarts.
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
    pub fn new(max_restarts: u32, restart_window: Duration) -> Self {
        Self {
            max_restarts,
            restart_window,
            timestamps: VecDeque::new(),
        }
    }

    /// Checks if a restart is allowed. If yes, records it and returns `true`.
    /// If the budget is exhausted, returns `false`.
    pub fn check_and_record(&mut self) -> bool {
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
    /// Whether this child's own resolved configuration enables supervision
    /// (it is itself a supervisor of further children). An invariant of the
    /// spec, not of any one incarnation, so it survives every restart;
    /// determines the grace this child's link guard allows before the
    /// reaper aborts it (see [`SUPERVISOR_KILL_GRACE`]).
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
    /// Watch cell carrying this incarnation's terminal outcome, populated by
    /// its watcher once the child's task has fully exited - after every one
    /// of its own drops, its `RegistryGuard` included, so a populated fate
    /// cell always means the child's registered name is already free.
    pub fate_rx: watch::Receiver<Option<ChildFate>>,
    /// Abort handle of the child's OWN task (taken before the watcher consumed
    /// the JoinHandle). The Kill -> abort escalation backstop. Never exposed
    /// outside the supervision tree.
    pub abort: AbortHandle,
    /// The child's stop lane: the transport for every stop/kill signal the
    /// escalation ladder sends this child.
    pub stop_lane: StopLane,
    /// This child's lifecycle: which incarnation is current, and whether it
    /// is running, being manually stopped, restarting, or retained-but-down.
    /// The single source of truth - see [`ChildLifecycle`].
    pub lifecycle: ChildLifecycle,
}

impl ChildState {
    /// True if a death event with this incarnation belongs to the instance
    /// the registry currently tracks (current, or the in-flight restart).
    pub fn accepts_incarnation(&self, incarnation: u64) -> bool {
        self.lifecycle.accepts_incarnation(incarnation)
    }

    /// Applies `to` if [`ChildLifecycle::is_legal`] allows the move from the
    /// current state; otherwise the state is left untouched and the rejected
    /// target is handed back. The single point every lifecycle change in this
    /// crate goes through.
    pub fn transition(&mut self, to: ChildLifecycle) -> Result<(), ChildLifecycle> {
        if ChildLifecycle::is_legal(&self.lifecycle, &to) {
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
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
            index: HashMap::new(),
        }
    }

    pub fn register(&mut self, state: ChildState) {
        let idx = self.children.len();
        self.index.insert(state.id.clone(), idx);
        self.children.push(state);
    }

    pub fn remove(&mut self, id: &ActorId) -> Option<ChildState> {
        let idx = self.index.remove(id)?;
        let child = self.children.remove(idx);
        // Rebuild index for items after the removed one
        for (new_idx, child) in self.children.iter().enumerate().skip(idx) {
            self.index.insert(child.id.clone(), new_idx);
        }
        Some(child)
    }

    pub fn get(&self, id: &ActorId) -> Option<&ChildState> {
        self.index.get(id).map(|&idx| &self.children[idx])
    }

    pub fn get_mut(&mut self, id: &ActorId) -> Option<&mut ChildState> {
        self.index
            .get(id)
            .copied()
            .map(|idx| &mut self.children[idx])
    }

    pub fn children_info(&self) -> Vec<ChildInfo> {
        self.children
            .iter()
            .map(|c| ChildInfo {
                id: c.id.clone(),
                name: c.name.clone(),
                restart_type: c.spec.restart_type,
                shutdown: c.spec.shutdown,
                is_alive: matches!(c.lifecycle, ChildLifecycle::Running(_)),
                restart_pending: matches!(c.lifecycle, ChildLifecycle::Restarting { .. }),
            })
            .collect()
    }

    /// Returns IDs of children started after the given child (for RestForOne).
    pub fn children_after(&self, id: &ActorId) -> Vec<ActorId> {
        if let Some(&idx) = self.index.get(id) {
            self.children[idx + 1..]
                .iter()
                .map(|c| c.id.clone())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Returns all child IDs (for OneForAll).
    pub fn all_ids(&self) -> Vec<ActorId> {
        self.children.iter().map(|c| c.id.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.children.len()
    }

    /// Drain all children (for parent shutdown).
    pub fn drain_all(&mut self) -> Vec<ChildState> {
        self.index.clear();
        std::mem::take(&mut self.children)
    }

    /// Adopts a restarted child instance after the parent accepted its
    /// `RestartOutcome::Adopted` (seq matches the pending restart).
    pub fn update_restarted(
        &mut self,
        child_id: &ActorId,
        seq: u64,
        new_stop_lane: StopLane,
        new_fate_rx: watch::Receiver<Option<ChildFate>>,
        new_abort: AbortHandle,
    ) -> bool {
        if let Some(child) = self.get_mut(child_id) {
            let pending =
                matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == seq);
            if pending && child.transition(ChildLifecycle::Running(seq)).is_ok() {
                child.stop_lane = new_stop_lane;
                child.fate_rx = new_fate_rx;
                child.abort = new_abort;
                return true;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Group restart state (OneForAll / RestForOne)
// ---------------------------------------------------------------------------

/// In-flight group restart: the affected live members have been told to stop
/// (reverse start order); once every awaited death arrives, the members are
/// restarted SEQUENTIALLY in start order. The supervisor's loop never blocks
/// on this; deaths arrive through the ordinary watcher channel.
pub(crate) struct GroupRestart {
    /// Members whose deaths we are still waiting for.
    pub awaiting: HashSet<ActorId>,
    /// Members to restart (start order) once `awaiting` empties. Excludes
    /// Temporary children (OTP: terminated with the group, never restarted)
    /// and, once recorded, any member in `manual_overrides`.
    pub restart_order: Vec<ActorId>,
    /// Members a caller explicitly `terminate_child`'d while this group stop
    /// was in flight: the caller's manual intent is honored over the group's
    /// default disposition, so these are excluded from `restart_order` when
    /// the chain starts and reported as `Removed` instead of
    /// `RestartInitiated` when their individual death arrives.
    pub manual_overrides: HashSet<ActorId>,
}

/// Phase of an in-flight OneForAll/RestForOne group restart.
pub(crate) enum GroupPhase {
    /// Affected members told to stop; awaiting their deaths.
    Stopping(GroupRestart),
    /// Restarting members one at a time in start order (OTP left-to-right).
    /// The FRONT of the queue is the member whose restart is in flight (or,
    /// for a member the chain finds already restarting independently, the
    /// one it is holding for); the next member is initiated only once the
    /// front's [`RestartOutcome`] is adopted. Adoption happens only after the
    /// fresh incarnation's init ack arrives - the restart path awaits the
    /// same `pre_start`/`on_started` contract the top-level spawn awaits - so
    /// sequential initiation gives sequential init COMPLETION: no two
    /// consecutive members' inits overlap, uniformly whether the chain itself
    /// initiated the attempt or is simply holding for one already in flight.
    Restarting(VecDeque<ActorId>),
}

// ---------------------------------------------------------------------------
// Supervision state (held by ActorContext)
// ---------------------------------------------------------------------------

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
    /// arm gets a chance to drain the matching watcher completion and
    /// deliver it exactly once.
    pub pending_manual_events: HashMap<(ActorId, u64), ChildEvent>,
    /// Every live child's watcher task. Dropping this - which happens as
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
}

impl SupervisionState {
    pub fn new(config: SupervisionConfig) -> Self {
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
        }
    }

    /// Initiates a restart for a child using its stored restart closure:
    /// transitions the ledger to `Restarting` with the given (freshly drawn)
    /// incarnation token, and spawns the closure. Does NOT touch the budget
    /// (callers decide whether the restart is budget-charged - strategy
    /// restarts are, manual bounces are not).
    ///
    /// `seq` is drawn by the caller from
    /// [`ActorSystem::next_incarnation`](crate::system::ActorSystem::next_incarnation)
    /// - the single counter every incarnation in the system draws from.
    pub fn initiate(&mut self, child_id: &ActorId, seq: u64) -> bool {
        match self.registry.get_mut(child_id) {
            Some(child) => {
                let incarnation = child.lifecycle.incarnation();
                if child
                    .transition(ChildLifecycle::Restarting {
                        incarnation,
                        next: seq,
                    })
                    .is_err()
                {
                    return false;
                }
            }
            None => return false,
        }
        if let Some(restart_fn) = self.restart_fns.get(child_id) {
            let fut = restart_fn(seq);
            self.restart_set.spawn(fut);
            true
        } else {
            false
        }
    }

    /// True if the child is a member of the in-flight group restart (either
    /// phase). Manual child-management APIs refuse to touch such members.
    ///
    /// A member already recorded in `manual_overrides` is excluded even
    /// though it may still sit in `awaiting`/`restart_order` (the run loop's
    /// death arm owns removing it from both, once its death event actually
    /// arrives): its own disposition was already committed synchronously by
    /// the overriding `terminate_child` call, so it reads as an ordinary
    /// `Down` child to every other manual API from that point on - notably
    /// `delete_child`, which the truth contract requires to succeed in the
    /// very same handler invocation that awaited the override.
    pub fn in_pending_group(&self, child_id: &ActorId) -> bool {
        match self.pending_group.as_ref() {
            Some(GroupPhase::Stopping(group)) => {
                !group.manual_overrides.contains(child_id)
                    && (group.awaiting.contains(child_id) || group.restart_order.contains(child_id))
            }
            Some(GroupPhase::Restarting(queue)) => queue.contains(child_id),
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Strategy helpers (used by runtime)
// ---------------------------------------------------------------------------

/// Result of applying a supervision strategy to a child's death.
pub(crate) enum StrategyOutcome {
    /// Restart exactly this child (OneForOne / SimpleOneForOne).
    RestartOne(ActorId),
    /// Group restart (OneForAll / RestForOne): stop the live members in
    /// `stop_reverse` (already in reverse start order), await their deaths,
    /// then restart `restart_order` in start order.
    RestartGroup {
        /// Live members to stop, reverse start order. Excludes the failed
        /// child (already dead).
        stop_reverse: Vec<ActorId>,
        /// Members to restart in start order (Temporary members excluded).
        restart_order: Vec<ActorId>,
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
        // (which is also the exit reason of a budget-exhausted supervisor).
        // Kill (OTP `killed`) is abnormal and does restart.
        RestartType::Transient => !matches!(
            reason,
            crate::types::StopReason::Graceful | crate::types::StopReason::ParentRequest
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
        RestartStrategy::OneForAll => {
            let members = sup.registry.all_ids();
            group_outcome(&sup.registry, members, failed_child_id)
        }
        RestartStrategy::RestForOne => {
            let mut members = vec![failed_child_id.clone()];
            members.extend(sup.registry.children_after(failed_child_id));
            group_outcome(&sup.registry, members, failed_child_id)
        }
    }
}

/// Builds the group outcome from the affected member set (start order).
fn group_outcome(
    registry: &ChildRegistry,
    members: Vec<ActorId>,
    failed: &ActorId,
) -> StrategyOutcome {
    let stop_reverse: Vec<ActorId> = members
        .iter()
        .rev()
        .filter(|id| *id != failed)
        .filter(|id| {
            registry
                .get(id)
                .map(|c| matches!(c.lifecycle, ChildLifecycle::Running(_)))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    // OTP: temporary siblings are terminated with the group but never restarted.
    let restart_order: Vec<ActorId> = members
        .into_iter()
        .filter(|id| {
            registry
                .get(id)
                .map(|c| !matches!(c.spec.restart_type, RestartType::Temporary))
                .unwrap_or(false)
        })
        .collect();
    StrategyOutcome::RestartGroup {
        stop_reverse,
        restart_order,
    }
}

/// Advances an in-flight [`GroupPhase::Restarting`] chain past every member
/// already `Running` on a fresher incarnation - an independent restart (a
/// solo bounce, or one this same supervisor already adopted before the group
/// chain reached it) that completed on its own is recognized by comparison
/// and never blind-re-initiated. Returns the id the caller should hand to
/// [`SupervisionState::initiate`] next: either a `Down` member ready to
/// start, or a member already `Restarting` independently, in which case
/// `initiate` is a documented no-op (its transition table rejects
/// `Restarting` -> `Restarting`) and this is simply how the chain HOLDS -
/// waiting for that already in-flight attempt's own [`RestartOutcome`]
/// instead of spawning a second, redundant one. `None` means every remaining
/// member turned out to already be fresh: the group phase is cleared and
/// every trigger queued during its lifetime is appended to `work` for
/// ordinary evaluation.
pub(crate) fn advance_restart_chain(
    sup: &mut SupervisionState,
    work: &mut VecDeque<ChildStoppedInternal>,
) -> Option<ActorId> {
    loop {
        let front = match sup.pending_group.as_ref() {
            Some(GroupPhase::Restarting(queue)) => queue.front().cloned(),
            _ => return None,
        };
        match front {
            Some(id) => {
                let already_fresh = sup
                    .registry
                    .get(&id)
                    .map(|c| matches!(c.lifecycle, ChildLifecycle::Running(_)))
                    .unwrap_or(false);
                if already_fresh {
                    if let Some(GroupPhase::Restarting(queue)) = sup.pending_group.as_mut() {
                        queue.pop_front();
                    }
                    continue;
                }
                return Some(id);
            }
            None => {
                sup.pending_group = None;
                work.extend(sup.queued_triggers.drain(..));
                return None;
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
    use crate::actor::runtime::spawn_watcher;
    use crate::types::StopReason;
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
        let (stop_lane, _stop_rx) = StopLane::new();
        let (_fate_tx, fate_rx) = watch::channel(None);
        ChildState {
            id: ActorId::from(id),
            name: Some(id.to_string()),
            spec: ChildSpec {
                restart_type,
                shutdown: Shutdown::default(),
                start_timeout: None,
                is_supervisor: false,
            },
            fate_rx,
            abort: tokio::spawn(async {}).abort_handle(),
            stop_lane,
            lifecycle: if alive {
                ChildLifecycle::Running(0)
            } else {
                ChildLifecycle::Down {
                    incarnation: 0,
                    event_pending: false,
                }
            },
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
    async fn registry_drain_all() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("a"));
        reg.register(dummy_child("b"));
        let drained = reg.drain_all();
        assert_eq!(drained.len(), 2);
        assert_eq!(reg.len(), 0);
    }

    #[tokio::test]
    async fn registry_update_restarted_accepts_pending_seq_only() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("a"));
        let seq = 7u64;
        reg.get_mut(&ActorId::from("a"))
            .unwrap()
            .transition(ChildLifecycle::Restarting {
                incarnation: 0,
                next: seq,
            })
            .unwrap();

        // Stale seq rejected
        let (lane, _rx) = StopLane::new();
        assert!(!reg.update_restarted(
            &ActorId::from("a"),
            seq + 99,
            lane,
            watch::channel(None).1,
            tokio::spawn(async {}).abort_handle()
        ));

        // Matching seq accepted; incarnation adopted
        let (lane, _rx) = StopLane::new();
        assert!(reg.update_restarted(
            &ActorId::from("a"),
            seq,
            lane,
            watch::channel(None).1,
            tokio::spawn(async {}).abort_handle()
        ));
        let child = reg.get(&ActorId::from("a")).unwrap();
        assert!(matches!(child.lifecycle, ChildLifecycle::Running(_)));
        assert_eq!(child.lifecycle.incarnation(), seq);
        assert!(child.accepts_incarnation(seq));
        assert!(!child.accepts_incarnation(0));
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
            event_pending: false,
        };
        match evaluate_strategy(&mut sup, &ActorId::from("b"), &StopReason::Kill) {
            StrategyOutcome::RestartGroup {
                stop_reverse,
                restart_order,
            } => {
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
            event_pending: false,
        };
        match evaluate_strategy(&mut sup, &ActorId::from("a"), &StopReason::Kill) {
            StrategyOutcome::RestartGroup {
                stop_reverse,
                restart_order,
            } => {
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
            event_pending: false,
        };
        match evaluate_strategy(&mut sup, &ActorId::from("a"), &StopReason::Kill) {
            StrategyOutcome::RestartGroup {
                stop_reverse,
                restart_order,
            } => {
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

    #[tokio::test]
    async fn guard_drop_raises_kill_forces_status_and_feeds_the_reaper_exactly_once() {
        let (lane, lane_rx) = StopLane::new();
        let (status_tx, status_rx) = watch::channel(ActorStatus::Running);
        let (_join, abort) = spawn_forever();
        let (reaper_tx, mut reaper_rx) = mpsc::channel(4);

        let guard = ChildLinkGuard::new(
            lane.clone(),
            status_tx,
            abort,
            reaper_tx,
            Duration::from_millis(50),
        );
        drop(guard);

        assert!(
            matches!(lane_rx.borrow().reason, Some(StopReason::Kill)),
            "drop must raise Kill on the child's stop lane"
        );
        assert_eq!(
            *status_rx.borrow(),
            ActorStatus::Stopped,
            "drop must force-publish a terminal status"
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
        // exactly what happens when a watcher completes normally (the
        // captured guard still drops right after).
        let (lane, lane_rx) = StopLane::new();
        lane.raise(StopReason::Graceful);
        let (status_tx, status_rx) = watch::channel(ActorStatus::Stopped);
        let (_join, abort) = spawn_forever();
        abort.abort();
        let (reaper_tx, mut reaper_rx) = mpsc::channel(4);

        let guard = ChildLinkGuard::new(lane, status_tx, abort, reaper_tx, KILL_GRACE);
        drop(guard);

        assert!(matches!(lane_rx.borrow().reason, Some(StopReason::Kill)));
        assert_eq!(*status_rx.borrow(), ActorStatus::Stopped);
        assert!(reaper_rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn guard_degrades_to_immediate_abort_when_the_reaper_channel_is_full() {
        let (lane, _lane_rx) = StopLane::new();
        let (status_tx, _status_rx) = watch::channel(ActorStatus::Running);
        let (join, abort) = spawn_forever();

        // Capacity 1, pre-filled: the guard's own `try_send` below has no
        // room and must degrade to aborting the child directly instead.
        let (reaper_tx, _reaper_rx) = mpsc::channel(1);
        let (_filler_join, filler_abort) = spawn_forever();
        reaper_tx
            .try_send(ReaperEntry::new(Instant::now(), filler_abort))
            .unwrap();

        let guard = ChildLinkGuard::new(lane, status_tx, abort, reaper_tx, Duration::from_secs(60));
        drop(guard);

        let result = tokio::time::timeout(Duration::from_millis(500), join).await;
        match result {
            Ok(Err(join_err)) => assert!(
                join_err.is_cancelled(),
                "a full reaper channel must degrade to an immediate abort"
            ),
            other => panic!("expected the child to be aborted promptly, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn guard_disarmed_by_the_watcher_is_inert_on_normal_completion() {
        let (lane, _lane_rx) = StopLane::new();
        let (status_tx, status_rx) = watch::channel(ActorStatus::Running);
        let child_join: JoinHandle<StopReason> = tokio::spawn(async { StopReason::Graceful });
        let abort = child_join.abort_handle();
        let (reaper_tx, mut reaper_rx) = mpsc::channel(4);
        let guard = ChildLinkGuard::new(lane, status_tx, abort, reaper_tx, KILL_GRACE);
        let (fate_tx, mut fate_rx) = watch::channel(None);

        let mut death_set: JoinSet<DeathOutcome> = JoinSet::new();
        spawn_watcher(
            &mut death_set,
            ActorId::from("normal-death"),
            0,
            child_join,
            fate_tx,
            guard,
        );

        let (id, reason, incarnation) = death_set.join_next().await.unwrap().unwrap();
        assert_eq!(id.as_str(), "normal-death");
        assert!(matches!(reason, StopReason::Graceful));
        assert_eq!(incarnation, 0);

        assert!(fate_rx.wait_for(|f| f.is_some()).await.is_ok());
        assert_eq!(
            *status_rx.borrow(),
            ActorStatus::Running,
            "a disarmed guard must not rewrite the status on normal completion"
        );
        assert!(
            reaper_rx.try_recv().is_err(),
            "a disarmed guard must not occupy the reaper on normal completion"
        );
    }

    #[tokio::test]
    async fn guard_fires_exactly_once_when_aborted_before_its_watcher_is_ever_polled() {
        // Single-threaded (current_thread) test runtime: a newly spawned
        // task is never polled until this task yields, so aborting the
        // JoinSet with no `.await` in between reliably exercises the
        // abort-before-first-poll path.
        let (lane, lane_rx) = StopLane::new();
        let (status_tx, status_rx) = watch::channel(ActorStatus::Running);
        let child_join: JoinHandle<StopReason> = tokio::spawn(std::future::pending());
        let abort = child_join.abort_handle();
        let (reaper_tx, mut reaper_rx) = mpsc::channel(4);
        let guard = ChildLinkGuard::new(lane, status_tx, abort, reaper_tx, KILL_GRACE);
        let (fate_tx, fate_rx) = watch::channel(None);

        let mut death_set: JoinSet<DeathOutcome> = JoinSet::new();
        spawn_watcher(
            &mut death_set,
            ActorId::from("never-polled"),
            0,
            child_join,
            fate_tx,
            guard,
        );
        death_set.abort_all();

        // Drains the aborted watcher so its drop glue (the captured guard,
        // included) actually runs before the assertions below.
        while death_set.join_next().await.is_some() {}

        assert!(
            matches!(lane_rx.borrow().reason, Some(StopReason::Kill)),
            "the guard must still fire even though its watcher never polled `join.await`"
        );
        assert_eq!(*status_rx.borrow(), ActorStatus::Stopped);
        assert!(reaper_rx.try_recv().is_ok());
        assert!(reaper_rx.try_recv().is_err());
        // The fate cell is untouched: only the watcher's own completion
        // writes it, and it never got that far.
        assert!(fate_rx.borrow().is_none());
    }
}
