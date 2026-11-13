//! Supervision configuration, restart budgets, and child registry.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio::time::Instant;

use crate::actor::runtime::saturating_deadline;
use crate::actor::subtree::AbortRecord;
use crate::error::ActorError;
use crate::system::{raise_noted, ActorSystem, ReaperEntry, ReaperFeed};
use crate::telemetry::AttemptOutcome;
use crate::types::{
    ActorId, ChildCounts, ChildEvent, ChildInfo, ChildStoppedInternal, RestartStrategy,
    RestartType, Shutdown, StopLane, StopReason, SupervisionAction,
};

pub(crate) mod rules;
pub(crate) mod slot;

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
    run_attempt, run_ladder, slot_pair_for, Attempt, AttemptTicket, CellOutcome, CellWatch,
    OwnStop, Predecessor, SlotCtl, SlotIo, Verdicts,
};

/// The acknowledgment of one child's initialization, with the child's id and
/// the incarnation that owes it.
pub(crate) type StrictAck = (ActorId, u64, oneshot::Receiver<Result<(), ActorError>>);

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
        /// The reason the incarnation ended with, when it ended before the
        /// attempt handed it over and the attempt had already taken the end
        /// from `new_join`, which is then spent: the runner takes the end
        /// from here and runs the ladder's last step without waiting.
        ended: Option<StopReason>,
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
/// raised `Kill` and its task aborted once its kill grace has passed, unless
/// the worker traps exit signals when the guard fires: it then gets the stop
/// of a supervisor child, with its own `Shutdown` as the window (the flag is
/// read at the drop, so a toggle just before the supervisor's end counts). A
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
/// status plane is never written here at all: `Stopped` is published only by
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
    /// The runtime of the child this guard watches, which is the
    /// supervisor's unless the child was placed elsewhere: the one whose
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
        // (a) Raise the child's inherited stop: a plain lane write,
        // infallible, and a no-op if the child has already finished its
        // message loop. (b) Enqueue its deadlines with the reaper: a
        // synchronous send on an unbounded feed (this is `Drop`: no awaiting,
        // no spawning that itself needs to be awaited). Every deadline
        // saturates, since a window and a grace are the user's and an
        // overflowing `Instant` sum would panic in drop glue.
        // A supervisor child, and a worker that traps exit signals at this
        // moment, is raised its orderly stop first; a `Kill` the lane holds by
        // then (raised before, or racing this raise) keeps the abort it is
        // owed, whatever the child's `Shutdown`. The flag is read once, here.
        // What a worker does instead when it traps exit signals at that
        // moment is the orderly stop bounded by its own `Shutdown`, read from
        // the child's record (see [`rules::inherited_stop_if_trapping`]); a
        // supervisor child has that stop in `inherited` already.
        let record = self.stop_lane.abort_record();
        let trapping = record.trap();
        let if_trapping = match self.inherited {
            rules::InheritedStop::Kill { grace } if trapping => {
                rules::inherited_stop_if_trapping(false, record.worker_shutdown(), grace)
            }
            _ => None,
        };
        let inherited = rules::guard_stop(self.inherited, if_trapping, trapping);
        if matches!(inherited, rules::InheritedStop::Orderly { .. }) {
            raise_noted(
                Some(&self.system),
                &self.stop_lane,
                StopReason::ParentKilled,
                crate::telemetry::StopOrigin::Requested,
            );
        }
        let kill_stored = matches!(self.stop_lane.reason(), Some(StopReason::Kill));
        let entry = match rules::fired_stop(inherited, kill_stored) {
            rules::InheritedStop::Kill { grace } => {
                // The entry below holds the child's abort handle, so no
                // registry entry is involved and no deadline is declared.
                raise_noted(
                    Some(&self.system),
                    &self.stop_lane,
                    StopReason::Kill,
                    crate::telemetry::StopOrigin::Requested,
                );
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
        // gone now: the system feeds the entry to the reaper of the child's
        // runtime once more before it gives up, which is what
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

/// When a supervisor shuts itself down because of its significant children.
///
/// Erlang/OTP: the supervisor flag `auto_shutdown`. A child is significant
/// when it is spawned with
/// [`significant(true)`](crate::actor::context::ChildSpawnBuilder::significant).
/// The check runs only for a child's end that the supervisor did not order:
/// a child that ends and is not restarted, a `Transient` child after a normal
/// exit or a `Temporary` child after any exit. An end the supervisor ordered
/// (`stop_child`, `terminate_child`, `restart_child`, or the stop of a group
/// restart) never triggers it. A child whose restart is under way has not
/// terminated.
///
/// # Examples
/// ```rust
/// use tokio_actors::{AutoShutdown, SupervisionConfig};
///
/// let config = SupervisionConfig::one_for_one().with_auto_shutdown(AutoShutdown::AnySignificant);
/// assert_eq!(config.auto_shutdown, AutoShutdown::AnySignificant);
/// assert_eq!(SupervisionConfig::default().auto_shutdown, AutoShutdown::Never);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoShutdown {
    /// The supervisor never shuts itself down because of a child. The default.
    #[default]
    Never,
    /// The supervisor shuts down when any significant child ends without a
    /// restart.
    AnySignificant,
    /// The supervisor shuts down when the last significant child that was
    /// still active ends without a restart.
    AllSignificant,
}

/// Configuration for an actor acting as a supervisor.
///
/// Maps to OTP supervisor child specs: strategy, intensity (max_restarts),
/// and period (restart_window).
///
/// An actor whose only job is supervising (an empty `handle` and short hooks)
/// is Erlang/OTP's supervisor shape: its supervision events never wait behind
/// a long user turn.
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
    /// When the supervisor shuts itself down because of its significant
    /// children. Default: [`AutoShutdown::Never`]. Not combinable with
    /// `SimpleOneForOne`, which Erlang/OTP refuses too: the supervisor's spawn
    /// answers [`SpawnError::InvalidSupervision`](crate::SpawnError::InvalidSupervision).
    pub auto_shutdown: AutoShutdown,
    /// Whether the supervisor's start waits for the children it spawned while
    /// starting. Default: `false`. See [`with_strict_start`](Self::with_strict_start).
    pub strict_start: bool,
}

impl Default for SupervisionConfig {
    fn default() -> Self {
        Self {
            strategy: RestartStrategy::OneForOne,
            max_restarts: 3,
            restart_window: Duration::from_secs(5),
            auto_shutdown: AutoShutdown::Never,
            strict_start: false,
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

    /// Sets when the supervisor shuts itself down because of its significant
    /// children (see [`AutoShutdown`]). Default: [`AutoShutdown::Never`].
    ///
    /// A non-`Never` mode needs at least one child spawned with
    /// [`significant(true)`](crate::actor::context::ChildSpawnBuilder::significant)
    /// to have any effect. The supervisor ends with
    /// [`StopReason::ParentRequest`] after it
    /// delivers the significant child's end to
    /// [`on_child_stopped`](crate::actor::Actor::on_child_stopped) as
    /// [`SupervisionAction::AutoShutdown`], so a `Transient` supervisor above it
    /// does not restart it. The supervisor's spawn is refused with
    /// [`SpawnError::InvalidSupervision`](crate::SpawnError::InvalidSupervision)
    /// for a non-`Never` mode under `SimpleOneForOne`, and a spawn of a
    /// significant child is refused under `Never` and for a `Permanent` child.
    pub fn with_auto_shutdown(mut self, mode: AutoShutdown) -> Self {
        self.auto_shutdown = mode;
        self
    }

    /// Makes the supervisor's own start wait for the children it spawned
    /// while starting (in `pre_start` and `on_started`).
    ///
    /// By default [`spawn_child`](crate::actor::context::ActorContext::spawn_child)
    /// returns once the child is registered, and a child whose initialization
    /// fails ends with `Failure` and is supervised like any crash. With strict
    /// start, after `on_started` returns and before the supervisor answers its
    /// own spawn, the supervisor waits for the initialization of each child
    /// spawned meanwhile, in the order they were spawned. The children still
    /// initialize concurrently. The first child that fails ends the
    /// supervisor's start with
    /// [`ActorError::ChildStart`]: the spawn
    /// answers [`SpawnError::Init`](crate::SpawnError::Init) carrying it, the
    /// children already started are stopped in reverse start order under their
    /// own [`Shutdown`], and the supervisor gets neither
    /// `on_child_stopped` nor `on_stopped`.
    ///
    /// The wait has no bound of its own, as Erlang/OTP's start has none; a
    /// `start_timeout` on the supervisor's spawn bounds it. A child that calls
    /// its supervisor during its own initialization (for example with a request
    /// to the supervisor's handle) waits for a supervisor that waits for it,
    /// until that bound, as in Erlang/OTP. Only children spawned
    /// while the supervisor starts are awaited; children it spawns later are
    /// not. The supervisor ends `Failure(ChildStart)` where Erlang/OTP exits
    /// with `{shutdown, ..}`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::SupervisionConfig;
    ///
    /// assert!(SupervisionConfig::one_for_one().with_strict_start().strict_start);
    /// assert!(!SupervisionConfig::one_for_one().strict_start);
    /// ```
    pub fn with_strict_start(mut self) -> Self {
        self.strict_start = true;
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

    /// Checks if a restart is allowed. If yes, records it. Returns the charge
    /// as it stands after the call: whether the restart was accepted, how
    /// many restarts the window holds (on a refusal, the maximum), and the
    /// budget's limits.
    pub(crate) fn check_and_record(&mut self) -> BudgetCharge {
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

        let accepted = self.timestamps.len() < self.max_restarts as usize;
        if accepted {
            self.timestamps.push_back(now);
        }
        BudgetCharge {
            accepted,
            used: u32::try_from(self.timestamps.len()).unwrap_or(u32::MAX),
            max: self.max_restarts,
            window: self.restart_window,
        }
    }
}

/// The outcome of one charge against a [`RestartBudget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BudgetCharge {
    /// Whether the restart was within the budget (and was recorded).
    pub(crate) accepted: bool,
    /// The restarts the window holds after the charge.
    pub(crate) used: u32,
    /// The most restarts the window allows.
    pub(crate) max: u32,
    /// The window the restarts are counted in.
    pub(crate) window: Duration,
}

// ---------------------------------------------------------------------------
// ChildSpec
// ---------------------------------------------------------------------------

/// Per-child supervision specification.
pub(crate) struct ChildSpec {
    pub restart_type: RestartType,
    pub shutdown: Shutdown,
    /// Bounds the wait for the initialization of each restart of this child.
    /// `None` (default) waits without a bound, under any `Shutdown`, as an
    /// Erlang/OTP supervisor does; setting a bound is a deliberate departure
    /// that trades strict parity for a supervisor that one child's hung
    /// initialization cannot stall. The first start of a child is never
    /// awaited (see `spawn_child`), so this bound applies to restarts only.
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
    /// Also read by `which_children` and `count_children` of a
    /// `SupervisorRef`; the stop and restart paths read the captured copies
    /// taken at `spawn_child` time (the initial guard, and the restart
    /// closure's own copy - see `spawn_child_internal` in `context.rs`).
    pub is_supervisor: bool,
    /// Whether the child is significant: its end, when the supervisor does
    /// not restart it, can shut the supervisor down under its
    /// [`AutoShutdown`] mode. An invariant of the spec.
    pub significant: bool,
    /// What the spawn passed on for the `sys` bundle. An invariant of the
    /// spec, kept across every restart: each incarnation is spawned with it.
    #[allow(dead_code)]
    pub sys: crate::actor::sys::SysOptions,
}

// ---------------------------------------------------------------------------
// ChildState
// ---------------------------------------------------------------------------

/// Runtime state for a supervised child.
pub(crate) struct ChildState {
    pub id: ActorId,
    pub name: Option<Arc<str>>,
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

    /// Applies `to` as [`transition`](Self::transition) does, at a site of
    /// the supervisor's run loop or teardown whose own reads make the move
    /// legal. A move the table refuses leaves the state as it was, is logged
    /// with the `tracing` feature, and fails a debug build.
    #[track_caller]
    pub(crate) fn transition_expected(&mut self, to: ChildLifecycle) {
        if let Err(to) = self.transition(to) {
            self.log_refused(to, std::panic::Location::caller());
            debug_assert!(
                false,
                "the ledger refused a move from {:?} to {to:?} for child {}",
                self.lifecycle, self.id
            );
        }
    }

    /// Applies `to` as [`transition`](Self::transition) does, at a site that
    /// runs inside a user's handler: a move the table refuses leaves the
    /// state as it was and is logged with the `tracing` feature, in every
    /// build.
    #[track_caller]
    pub(crate) fn transition_logged(&mut self, to: ChildLifecycle) {
        if let Err(to) = self.transition(to) {
            self.log_refused(to, std::panic::Location::caller());
        }
    }

    /// The log line of a move the table refused.
    #[allow(unused_variables)]
    fn log_refused(&self, to: ChildLifecycle, at: &'static std::panic::Location<'static>) {
        #[cfg(feature = "tracing")]
        caught_log!(error,
            child = %self.id,
            from = ?self.lifecycle,
            to = ?to,
            at = %at,
            "the ledger refused a move its caller's reads allow; the state is unchanged"
        );
    }
}

// ---------------------------------------------------------------------------
// ChildRegistry
// ---------------------------------------------------------------------------

/// Child registry in start order, with lookup by id.
///
/// Each child is registered under a sequence number above every live
/// child's, and `order` iterates by that number, so iteration is start order
/// with removed children skipped (RestForOne's later children, the newest
/// first walk of a teardown). `by_id` holds the rows. A restart changes a row
/// in place and keeps its number; removing a child touches only its own two
/// entries.
pub(crate) struct ChildRegistry {
    by_id: HashMap<ActorId, Entry>,
    order: BTreeMap<u64, ActorId>,
    /// The record of the incarnation this registry belongs to, set once by the
    /// first child spawned through the supervisor's context. The registry
    /// writes its length there at every add and remove, which is where a
    /// system snapshot reads a supervisor's child count without asking it.
    owner: Option<AbortRecord>,
}

/// A registered child and its place in start order.
struct Entry {
    seq: u64,
    state: ChildState,
}

impl ChildRegistry {
    pub(crate) fn new() -> Self {
        Self {
            by_id: HashMap::new(),
            order: BTreeMap::new(),
            owner: None,
        }
    }

    /// Names the record the child count is published to, unless one is named
    /// already. The count is published from the next add or remove on.
    pub(crate) fn adopt_owner(&mut self, record: &AbortRecord) {
        if self.owner.is_none() {
            self.owner = Some(record.clone());
        }
    }

    /// The record of the supervisor that owns this registry, if named.
    pub(crate) fn owner_record(&self) -> Option<AbortRecord> {
        self.owner.clone()
    }

    /// Publishes the number of children to the owner's record, if any.
    fn publish_count(&self) {
        if let Some(owner) = &self.owner {
            owner.set_children(self.by_id.len());
        }
    }

    /// Adds a child after every child registered so far. Ids are unique (the
    /// spawn refuses a duplicate first); a row under the same id would be
    /// replaced.
    pub(crate) fn register(&mut self, state: ChildState) {
        let seq = self
            .order
            .last_key_value()
            .map_or(0, |(seq, _)| seq.saturating_add(1));
        self.order.insert(seq, state.id.clone());
        let replaced = self.by_id.insert(state.id.clone(), Entry { seq, state });
        debug_assert!(replaced.is_none(), "a child id is registered twice");
        if let Some(replaced) = replaced {
            self.order.remove(&replaced.seq);
        }
        self.publish_count();
    }

    pub(crate) fn remove(&mut self, id: &ActorId) -> Option<ChildState> {
        let entry = self.by_id.remove(id)?;
        self.order.remove(&entry.seq);
        self.publish_count();
        Some(entry.state)
    }

    pub(crate) fn get(&self, id: &ActorId) -> Option<&ChildState> {
        self.by_id.get(id).map(|entry| &entry.state)
    }

    pub(crate) fn get_mut(&mut self, id: &ActorId) -> Option<&mut ChildState> {
        self.by_id.get_mut(id).map(|entry| &mut entry.state)
    }

    /// Every child, in start order.
    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &ChildState> {
        self.order
            .values()
            .filter_map(|id| self.by_id.get(id).map(|entry| &entry.state))
    }

    /// Returns IDs of children started after the given child.
    #[cfg(test)]
    pub(crate) fn children_after(&self, id: &ActorId) -> Vec<ActorId> {
        let Some(seq) = self.by_id.get(id).map(|entry| entry.seq) else {
            return Vec::new();
        };
        self.order
            .range(seq.saturating_add(1)..)
            .map(|(_, id)| id.clone())
            .collect()
    }

    /// Returns all child IDs.
    #[cfg(test)]
    pub(crate) fn all_ids(&self) -> Vec<ActorId> {
        self.iter().map(|c| c.id.clone()).collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.by_id.len()
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
    /// The acknowledgments of the children spawned while the supervisor
    /// starts, in spawn order, kept only under `strict_start`: the start waits
    /// for each of them before the supervisor answers its own spawn.
    pub strict_acks: Vec<StrictAck>,
    /// True from the supervisor's creation until its `on_started` returns,
    /// when `strict_start` is set: a child spawned in that span keeps its
    /// acknowledgment in `strict_acks`.
    pub init_phase: bool,
    /// The incarnations whose end the supervisor must not act on: a child
    /// spawned with `await_init` whose initialization failed has no spec,
    /// and its death report is dropped when it arrives (no callback, no
    /// charge). Each entry is removed by the report it silences.
    pub silenced: Vec<(ActorId, u64)>,
    /// The subtree of the last incarnation of each child whose spec was
    /// removed while that subtree still ran, by child id. An entry lives
    /// exactly until its subtree has ended.
    retired: HashMap<ActorId, Predecessor>,
}

impl SupervisionState {
    pub(crate) fn new(config: SupervisionConfig) -> Self {
        let budget = RestartBudget::new(config.max_restarts, config.restart_window);
        let init_phase = config.strict_start;
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
            strict_acks: Vec::new(),
            init_phase,
            silenced: Vec::new(),
            retired: HashMap::new(),
        }
    }

    /// Keeps the acknowledgment of a child spawned while the supervisor
    /// starts.
    pub(crate) fn push_strict_ack(
        &mut self,
        id: ActorId,
        incarnation: u64,
        rx: oneshot::Receiver<Result<(), ActorError>>,
    ) {
        self.strict_acks.push((id, incarnation, rx));
    }

    /// Takes the acknowledgments kept so far, in spawn order.
    pub(crate) fn take_strict_acks(&mut self) -> Vec<StrictAck> {
        std::mem::take(&mut self.strict_acks)
    }

    /// Whether the end of `child`, which the supervisor did not order and
    /// does not restart, shuts the supervisor down: the mode, the child's
    /// significance and whether another significant child is still active
    /// (see [`rules::auto_active`]). Read before the spec is pruned.
    pub(crate) fn auto_shutdown_due(&self, child: &ActorId) -> bool {
        let mode = self.config.auto_shutdown;
        if mode == AutoShutdown::Never {
            return false;
        }
        let Some(spec) = self.registry.get(child) else {
            return false;
        };
        let pool = matches!(self.config.strategy, RestartStrategy::SimpleOneForOne);
        // Only `AllSignificant` of a significant child reads the others.
        let others_active = mode == AutoShutdown::AllSignificant
            && spec.spec.significant
            && self.registry.iter().any(|other| {
                other.id != *child
                    && other.spec.significant
                    && rules::auto_active(
                        other.lifecycle,
                        other.spec.restart_type,
                        pool,
                        self.in_restart_set(&other.id),
                    )
            });
        rules::auto_shutdown_due(mode, spec.spec.significant, others_active)
    }

    /// Drops the entry that silences the report of this incarnation, if there
    /// is one, and tells whether it was there.
    pub(crate) fn take_silenced(&mut self, child: &ActorId, incarnation: u64) -> bool {
        match self
            .silenced
            .iter()
            .position(|(id, n)| id == child && *n == incarnation)
        {
            Some(at) => {
                self.silenced.swap_remove(at);
                true
            }
            None => false,
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

    /// The decision on the death of the incarnation `incarnation` of a child
    /// is made: the restart it owes has its ticket, or none is owed any more.
    /// Quiescence stops counting the death. Safe to repeat.
    pub(crate) fn death_decided(&self, incarnation: u64) {
        if let Some(owner) = &self.registry.owner {
            crate::telemetry::death_decided(owner, incarnation);
        }
    }

    /// Tells quiescence whether a group restart is in flight: called after
    /// every point where `pending_group` is set from nothing or cleared for
    /// good, so the group is owed work from its decision until its last
    /// member has its fate. Safe to repeat.
    pub(crate) fn note_group(&self) {
        if let Some(owner) = &self.registry.owner {
            crate::telemetry::group_pending(owner, self.pending_group.is_some());
        }
    }

    /// Keeps the ticket of the attempt that started the incarnation
    /// `incarnation` of `child_id`, until the attempt's outcome is taken.
    pub(crate) fn note_attempt(
        &mut self,
        child_id: &ActorId,
        incarnation: u64,
        ticket: AttemptTicket,
    ) {
        self.verdicts.keep_ticket(child_id, incarnation, ticket);
    }

    /// Ends the telemetry of the attempt that started the incarnation
    /// `incarnation` of `child_id`, once: its ticket, if it has one, is
    /// settled with the outcome `outcome` builds. Does nothing while no
    /// attempt holds a ticket.
    pub(crate) fn settle_attempt(
        &mut self,
        child_id: &ActorId,
        incarnation: u64,
        outcome: impl FnOnce() -> AttemptOutcome,
    ) {
        if let Some(ticket) = self.verdicts.take_ticket(child_id, incarnation) {
            ticket.settle(child_id, incarnation, outcome());
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
            child_name: child.name.as_deref().map(str::to_string),
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
            child.transition_logged(ChildLifecycle::Down {
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
                    name: child.name.as_deref().map(str::to_string),
                    restart_type: child.spec.restart_type,
                    shutdown: child.spec.shutdown,
                    is_alive,
                    restart_pending,
                    is_supervisor: child.spec.is_supervisor,
                }
            })
            .collect()
    }

    /// The child counts of `count_children`: every spec, the specs with an
    /// incarnation alive, and the specs by the kind of child they hold. A
    /// child between two incarnations is not active.
    pub(crate) fn child_counts(&self) -> ChildCounts {
        let mut counts = ChildCounts {
            specs: 0,
            active: 0,
            supervisors: 0,
            workers: 0,
        };
        for child in self.registry.iter() {
            let fate_present = child.ctl.task_state() == TaskState::Ended;
            let in_restart_set = self.in_restart_set(&child.id);
            let (is_alive, _) =
                rules::child_info_flags(child.lifecycle, fate_present, in_restart_set);
            counts.specs += 1;
            if is_alive {
                counts.active += 1;
            }
            if child.spec.is_supervisor {
                counts.supervisors += 1;
            } else {
                counts.workers += 1;
            }
        }
        counts
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
        #[cfg(test)]
        if let Some((child_id, _, _)) = &owner {
            crate::actor::runtime::end_hooks::run(
                crate::actor::runtime::end_hooks::RUNNER_LOST,
                child_id,
            );
        }
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
                    let owner = self.registry.owner_record();
                    let task = self.death_set.spawn(async move {
                        let reason = lost.wait().await;
                        if let Some(owner) = &owner {
                            crate::telemetry::death_landed(owner, incarnation);
                        }
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
                    let owner = self.registry.owner_record();
                    let task = self.death_set.spawn(async move {
                        if let Some(owner) = &owner {
                            crate::telemetry::death_landed(owner, incarnation);
                        }
                        (child_id, reason, incarnation)
                    });
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
        // The incarnation the ledger still holds as the attempt's, when its
        // lane was published: the death task below follows it.
        let followed = self
            .registry
            .get(&child_id)
            .filter(|child| {
                matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == incarnation)
            })
            .and(slot)
            .and_then(SlotCtl::lane);
        match lost {
            Some(lost) => {
                let owner = self.registry.owner_record();
                // The death task's claim comes before the attempt settles,
                // as an adoption's does: an incarnation that already ended
                // is owed from here.
                if let Some(lane) = &followed {
                    crate::actor::runtime::claim_death(
                        owner.as_ref(),
                        &child_id,
                        incarnation,
                        lane.abort_record(),
                    );
                }
                // No outcome follows: the attempt ends here, and its
                // incarnation is followed through the death plane.
                self.settle_attempt(&child_id, incarnation, || AttemptOutcome::Failed {
                    reason: StopReason::Failure(panic.clone().unwrap_or_else(|| {
                        ActorError::Panic("restart attempt task failed".to_string())
                    })),
                });
                #[cfg(test)]
                crate::actor::runtime::end_hooks::run(
                    crate::actor::runtime::end_hooks::SETTLED,
                    &child_id,
                );
                let key = (child_id.clone(), incarnation);
                let task = self.death_set.spawn(async move {
                    let reason = lost.wait().await;
                    if let Some(owner) = &owner {
                        crate::telemetry::death_landed(owner, incarnation);
                    }
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
    /// (the runners and the attempts), keyed by task id, and the attempts it
    /// reports from their status planes.
    ///
    /// The sets are told apart by the plane each task was registered on,
    /// never by the ledger:
    /// - Runners (death plane): an occupant whose task published its end,
    ///   and every incarnation that does not occupy its key (another one does,
    ///   or the key is down or gone), which the ledger leaves only after its
    ///   end was seen. A replacement task that follows an attempt's
    ///   incarnation is a runner too; it returns at once when the
    ///   incarnation ended, and is not awaited before. A runner whose
    ///   incarnation ended on its own with its task still dropping is read
    ///   later, by [`final_deaths`](Self::final_deaths).
    /// - Attempts (restart plane): a restart attempt whose incarnation's task
    ///   published its end, and one whose incarnation's start completed and
    ///   then ended on its own while the attempt hands it over at its next
    ///   poll ([`slot::FinalAttempt::Adopts`], also listed in `adopts` with
    ///   the watch on its cell).
    /// - Reported: the restart attempts that are not awaited, whose
    ///   incarnation's final reason is written and which hand nothing over
    ///   ([`slot::final_attempt`]), each with the outcome the attempt reports
    ///   itself: the collection reports them from the status plane, so an
    ///   attempt whose incarnation is still dropping what it holds gets the
    ///   outcome, the death report and the final delivery an awaited one
    ///   gets, and nothing waits for that drop.
    pub(crate) fn final_await_sets(&self) -> FinalSets {
        let mut sets = FinalSets::default();
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
                    match slot::final_attempt(ctl, child_id, *incarnation) {
                        slot::FinalAttempt::Await => {
                            sets.attempts.insert(*task);
                        }
                        slot::FinalAttempt::Adopts => {
                            sets.attempts.insert(*task);
                            sets.adopts
                                .push((*task, child_id.clone(), *incarnation, ctl.watch()));
                        }
                        slot::FinalAttempt::Report(outcome) => {
                            sets.reported.push((*task, *outcome))
                        }
                        slot::FinalAttempt::Leave => {}
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
                        sets.runners.insert(*task);
                    }
                }
                (TaskPlane::Death, Some((ChildLifecycle::Restarting { next, .. }, ctl)))
                    if next == *incarnation =>
                {
                    if ctl.status_ended() {
                        sets.runners.insert(*task);
                    }
                }
                // Another incarnation holds the key, or the key is down or
                // gone: the incarnation of this task was retired after its
                // end was seen.
                (TaskPlane::Death, _) => {
                    sets.runners.insert(*task);
                }
            }
        }
        sets
    }

    /// The final collection's read of the runners still in the death plane,
    /// in the children's start order, once the plane holds none that has
    /// returned: the runners to await, and the deaths to collect now.
    ///
    /// An incarnation the ledger still holds is read once from its status
    /// plane ([`slot::final_death`]): its runner is awaited when its task
    /// published its end, and its death is collected when its final reason
    /// is written while its task may still be dropping what it holds, with
    /// that reason, the one the runner returns. `early` holds the
    /// incarnations whose deaths the collection took that way, keyed by
    /// child and incarnation: the ledger holds them as down, and their
    /// runners are awaited once their tasks published their ends, so the
    /// plane is left with the incarnations still dropping; their reports
    /// are dropped when they are joined.
    pub(crate) fn final_deaths(
        &self,
        early: &HashSet<(ActorId, u64)>,
    ) -> (Vec<tokio::task::Id>, Vec<ChildStoppedInternal>) {
        let position: HashMap<&ActorId, usize> = self
            .registry
            .iter()
            .enumerate()
            .map(|(index, child)| (&child.id, index))
            .collect();
        let mut runners: Vec<(usize, u64, tokio::task::Id, &ActorId)> = self
            .task_owner
            .iter()
            .filter(|(_, (_, _, plane))| *plane == TaskPlane::Death)
            .filter_map(|(task, (child_id, incarnation, _))| {
                Some((*position.get(child_id)?, *incarnation, *task, child_id))
            })
            .collect();
        runners.sort_unstable_by_key(|(index, incarnation, _, _)| (*index, *incarnation));
        let mut ended = Vec::new();
        let mut deaths = Vec::new();
        for (_, incarnation, task, child_id) in runners {
            let Some(child) = self.registry.get(child_id) else {
                continue;
            };
            let Some(ctl) = child.slot_of(incarnation) else {
                continue;
            };
            if early.contains(&(child_id.clone(), incarnation)) {
                if ctl.status_ended() {
                    ended.push(task);
                }
                continue;
            }
            if matches!(child.lifecycle, ChildLifecycle::Down { .. }) {
                continue;
            }
            match slot::final_death(ctl) {
                slot::FinalDeath::Await => ended.push(task),
                slot::FinalDeath::Report(reason) => deaths.push(ChildStoppedInternal {
                    child_id: child_id.clone(),
                    reason,
                    incarnation,
                }),
                slot::FinalDeath::Leave => {}
            }
        }
        (ended, deaths)
    }

    /// The incarnations whose ends the final collection took from their
    /// status planes ([`final_await_sets`](Self::final_await_sets) for the
    /// attempts, [`final_deaths`](Self::final_deaths) for the runners) and
    /// whose tasks have not returned: the ledger holds each as down, while
    /// its task may still run. Each with whether its task is an attempt. The
    /// teardown waits for each to end, as it waits for every occupant, before
    /// the supervisor publishes its end.
    pub(crate) fn reported_ends(&self) -> Vec<(ActorId, u64, bool, CellWatch)> {
        self.task_owner
            .values()
            .filter_map(|(child_id, incarnation, plane)| {
                let child = self.registry.get(child_id)?;
                let reported = matches!(
                    child.lifecycle,
                    ChildLifecycle::Down { incarnation: n, .. } if n == *incarnation
                );
                reported.then(|| {
                    (
                        child_id.clone(),
                        *incarnation,
                        *plane == TaskPlane::Restart,
                        child.ctl.watch(),
                    )
                })
            })
            .collect()
    }

    /// Whether the ledger holds the incarnation `incarnation` of `child_id` as
    /// down. For a restart attempt whose task returns only now, that means
    /// the final collection reported it from its status plane.
    pub(crate) fn reported_early(&self, child_id: &ActorId, incarnation: u64) -> bool {
        self.registry.get(child_id).is_some_and(|child| {
            matches!(child.lifecycle, ChildLifecycle::Down { incarnation: n, .. } if n == incarnation)
        })
    }
}

/// What [`SupervisionState::final_await_sets`] returns.
#[derive(Default)]
pub(crate) struct FinalSets {
    /// The runners to await.
    pub runners: HashSet<tokio::task::Id>,
    /// The attempts to await.
    pub attempts: HashSet<tokio::task::Id>,
    /// The attempts among `attempts` that hand their incarnation over at
    /// their next poll, with the watch on the cell whose settling ends the
    /// wait on one early.
    pub adopts: Vec<(tokio::task::Id, ActorId, u64, CellWatch)>,
    /// The attempts reported from their status planes, with their outcomes.
    pub reported: Vec<(tokio::task::Id, RestartOutcome)>,
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
#[cfg(test)]
pub(crate) fn evaluate_strategy(
    sup: &mut SupervisionState,
    failed_child_id: &ActorId,
    reason: &crate::types::StopReason,
) -> StrategyOutcome {
    evaluate_strategy_charged(sup, failed_child_id, reason, &mut |_, _| {})
}

/// [`evaluate_strategy`], telling `charged` the incarnation and the charge of
/// the budget when the death is charged against it.
pub(crate) fn evaluate_strategy_charged(
    sup: &mut SupervisionState,
    failed_child_id: &ActorId,
    reason: &crate::types::StopReason,
    charged: &mut dyn FnMut(u64, BudgetCharge),
) -> StrategyOutcome {
    let child = match sup.registry.get(failed_child_id) {
        Some(c) => c,
        None => return StrategyOutcome::Remove,
    };

    let restart_type = child.spec.restart_type;
    let incarnation = child.lifecycle.held_token();
    let should_restart = match restart_type {
        RestartType::Permanent => true,
        // OTP transient parity: restart only on ABNORMAL exits. The clean
        // reasons are normal, shutdown and {shutdown, Term} - our Graceful,
        // ParentRequest (which is also the exit reason of a budget-exhausted
        // supervisor) and Shutdown, and ParentKilled, a clean exit ordered
        // from above. Kill (OTP `killed`) is abnormal and does restart. The
        // supervisor's `child_terminated` report reads the same test.
        RestartType::Transient => !reason.is_normal(),
        RestartType::Temporary => false,
    };

    if !should_restart {
        return StrategyOutcome::Remove;
    }

    // Check budget
    let charge = sup.budget.check_and_record();
    charged(incarnation, charge);
    if !charge.accepted {
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
            sup.note_group();
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

    use tokio::task::JoinHandle;

    // - RestartBudget -------------------------------------------------------

    // - ChildRegistry -------------------------------------------------------

    fn dummy_child_with(id: &str, restart_type: RestartType, alive: bool) -> ChildState {
        let (ctl, _io) = slot_pair(Default::default());
        ChildState {
            id: ActorId::from(id),
            name: Some(Arc::from(id.to_string())),
            spec: ChildSpec {
                restart_type,
                shutdown: Shutdown::default(),
                start_timeout: None,
                kill_grace: crate::actor::supervision::DEFAULT_KILL_GRACE,
                is_supervisor: false,
                significant: false,
                sys: crate::actor::sys::SysOptions::default(),
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

        // Iteration stays in start order across removals and new children:
        // a removed child leaves no gap and a new one comes last.
        let ids = |reg: &ChildRegistry| -> Vec<String> {
            reg.iter().map(|c| c.id.as_str().to_string()).collect()
        };
        reg.register(dummy_child("a"));
        reg.register(dummy_child("d"));
        assert_eq!(ids(&reg), ["b", "c", "a", "d"]);
        reg.remove(&ActorId::from("c"));
        reg.register(dummy_child("c"));
        assert_eq!(ids(&reg), ["b", "a", "d", "c"]);
        reg.remove(&ActorId::from("c"));
        reg.register(dummy_child("e"));
        assert_eq!(ids(&reg), ["b", "a", "d", "e"]);
        let newest_first: Vec<String> = reg
            .iter()
            .rev()
            .map(|c| c.id.as_str().to_string())
            .collect();
        assert_eq!(newest_first, ["e", "d", "a", "b"]);
        for id in ["a", "b", "d", "e"] {
            assert_eq!(reg.get(&ActorId::from(id)).unwrap().id.as_str(), id);
        }
        assert_eq!(reg.len(), 4);
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

        // A move the table refuses leaves the state as it was. At a site of
        // the run loop it fails a debug build; at a site inside a user's
        // handler it is only logged.
        let running = ChildLifecycle::Running(seq);
        let refused = ChildLifecycle::Running(seq + 1);
        let child = reg.get_mut(&ActorId::from("a")).unwrap();
        let expected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            child.transition_expected(refused);
        }));
        assert_eq!(expected.is_err(), cfg!(debug_assertions));
        assert_eq!(child.lifecycle, running);
        child.transition_logged(refused);
        assert_eq!(child.lifecycle, running);
    }

    // - GroupRestart --------------------------------------------------------

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
            ..SupervisionConfig::default()
        })
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
}

#[cfg(test)]
mod explore;

#[cfg(test)]
pub(crate) mod lost_runner;
