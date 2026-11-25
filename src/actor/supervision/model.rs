//! The plain values the restart, budget, strategy, link-guard, system ladder
//! and verdict wait rules take and return. None of them holds a lock, a handle or a clock: an instant is
//! a value the caller read.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::actor::supervision::rules::InheritedStop;

/// The far-future horizon `saturating_deadline` caps every input at: the
/// same 30-year horizon Tokio itself uses for an overflowing timeout.
pub(crate) const FAR_FUTURE: Duration = Duration::from_secs(86_400 * 365 * 30);

/// Wait after an abort before the Unresponsive verdict: a task that yields
/// ends at the abort, so only a non-yielding task outlives it. Independent of
/// any kill grace.
pub(crate) const ABORT_SETTLE: Duration = Duration::from_millis(100);

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

/// Whether a child's end leads to a restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestartDecision {
    /// The strategy restarts the child, if the budget accepts the charge.
    Restart,
    /// The child stays down and its spec is removed.
    Remove,
}

/// What becomes of a child's spec when its start was ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IgnoredEnd {
    /// The spec is kept and the child stays down.
    KeepDown,
    /// The spec is removed.
    Remove,
}

/// What a strategy restarts once the budget accepted a charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StrategyScope {
    /// The child that ended, alone.
    One,
    /// The group the strategy covers, laid out by the group commit.
    Group,
}

/// A sliding-window restart budget: the instants of the restarts it
/// accepted that may still count, oldest first.
#[derive(Clone, Debug)]
pub(crate) struct BudgetState {
    pub(crate) max_restarts: u32,
    pub(crate) window: Duration,
    pub(crate) charges: VecDeque<Instant>,
}

impl BudgetState {
    pub(crate) fn new(max_restarts: u32, window: Duration) -> Self {
        Self {
            max_restarts,
            window,
            charges: VecDeque::new(),
        }
    }
}

/// The outcome of one charge against a restart budget.
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

/// The first step of a firing link guard: the stop it performs and whether
/// it raises `ParentKilled` before it reads the lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GuardFirst {
    pub(crate) stop: InheritedStop,
    pub(crate) parent_killed: bool,
}

/// The rest of a firing link guard, once the lane was read after the first
/// raise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GuardRest {
    /// Raise `Kill` now; the reaper aborts the task at `abort_at`.
    Kill { abort_at: Instant },
    /// The reaper raises `Kill` at `kill_at` and aborts the task at
    /// `abort_at`.
    Staged { kill_at: Instant, abort_at: Instant },
    /// An orderly stop without a bound: no entry, nothing forces it.
    Unforced,
}

/// A stop ladder the system runs on one actor, with its timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemLadder {
    /// A bounded stop whose first request is raised: the window ends at
    /// `window_end`, and `grace` is the target's kill grace.
    Bounded {
        window_end: Instant,
        grace: Duration,
    },
    /// One root of the shutdown walk: a graceful window of `per_actor`, then
    /// the kill grace, then the settle time, every wait ending by `deadline`.
    Walk {
        deadline: Instant,
        per_actor: Duration,
        grace: Duration,
    },
    /// A force stop of a target whose `Kill` its caller raised: the kill
    /// grace, the abort, then the verdict floor.
    Force { grace: Duration },
}

/// Where a system stop ladder stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemStep {
    /// Nothing was taken yet.
    Start,
    /// The graceful request is being raised.
    Requesting,
    /// The graceful window runs until `until`.
    Window { until: Instant },
    /// The `Kill` step is being taken.
    Killing,
    /// The `Kill` step was taken; the target has until `until` to end.
    /// `raised` says that this ladder raised a `Kill` the lane accepted.
    KillGrace { until: Instant, raised: bool },
    /// The abort is being issued.
    Aborting { raised: bool },
    /// The task was aborted; it has until `until` to end, or until the
    /// verdict floor when `until` is `None`.
    Settle {
        until: Option<Instant>,
        raised: bool,
    },
    /// The ladder gave its outcome.
    Over,
}

/// What a system stop ladder's owner observed since the last step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemSeen {
    /// The ladder begins; `accepted` is the decision on the call's first
    /// request (always true for the walk and the force stop).
    Begin { accepted: bool },
    /// The step the ladder asked for was taken; `raised` says that a `Kill`
    /// it raised was accepted by the lane.
    Taken { raised: bool },
    /// The status reached a terminal state within the wait.
    Ended,
    /// The wait passed without a terminal status. `kill_started` says that
    /// the bounded stop's `Kill` step was already taken by its call; it is
    /// read in the window only.
    Passed { kill_started: bool },
}

/// What a system stop ladder's owner does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemAction {
    None,
    /// Raise the graceful `ParentRequest`, then report `Taken`.
    Request,
    /// Take the `Kill` step, raising `Kill` when `raise`, then report `Taken`.
    Kill {
        raise: bool,
    },
    /// Abort the task, then report `Taken`.
    Abort,
    /// Wait until the instant for a terminal status, then report `Ended` or
    /// `Passed`.
    WaitUntil(Instant),
    /// Wait for a terminal status under the verdict floor of a wait whose
    /// budget is `budget` and whose cap is `cap`, then report `Ended` or
    /// `Passed`.
    WaitFloor {
        budget: Duration,
        cap: Duration,
    },
    /// The ladder is over.
    Finish(SystemEnd),
}

/// How a system stop ladder ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemEnd {
    /// The target reached a terminal status, classified by the one terminal
    /// classifier. `killed` says that this ladder raised a `Kill` the lane
    /// accepted: the walk classifies with it, the bounded stop and the force
    /// stop with the outcome their callers asked for.
    Terminal { killed: bool },
    /// The call's first request was refused: no step is owed.
    Refused,
    /// The target outlived the whole ladder.
    Unresponsive,
    /// The walk's deadline passed before the target ended: this root and
    /// every root after it go to the force sweep.
    Sweep { kill_raised: bool },
}

/// What a wait that can give the Unresponsive verdict observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerdictSeen {
    /// A wait on a cell whose executor is gone, at its first read: whether
    /// the plane reads ended and whether the verdict flag is already set.
    CellOpened { ended: bool, flag_set: bool },
    /// The floor wait of a cell whose executor is gone ended; `ended` says
    /// that the plane reached its end first.
    CellFloor { ended: bool },
    /// The floor wait of a lost runner's end ended; `ended` as above.
    LostFloor { ended: bool },
    /// A restart attempt's read of the previous subtree: ended, a stop order
    /// or the cancel flag present, the verdict instant passed.
    Predecessor {
        gone: bool,
        order: bool,
        expired: bool,
    },
    /// A handler's read of a stopped child's subtree, which has no order.
    Subtree { gone: bool, expired: bool },
}

/// What a wait that can give the Unresponsive verdict does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerdictAct {
    /// Wait for the end under the verdict floor of the incarnation's own
    /// budget.
    WaitFloor,
    /// Wait for the next change of what was read.
    Keep,
    /// The incarnation ended: release its owed mark and return the reason
    /// its plane carries.
    Fate,
    /// Return the Unresponsive verdict, recording it first when `write`.
    Unresponsive { write: bool },
    /// Record the Unresponsive verdict, then wait for the end without a
    /// bound and return the reason the plane carries.
    VerdictThenEnd,
    /// The subtree ended: go on.
    Proceed,
    /// A stop order or a cancel came first: mark the cell cancelled.
    Cancel,
    /// The subtree outlived its bound: record the previous incarnation's
    /// Unresponsive verdict.
    Escalate,
}
