//! The supervision decisions that are pure functions of their inputs: no
//! runtime type, no clock read, no I/O. Instants and durations enter as plain
//! values, so every decision can be checked over its whole domain. The
//! callers (the stop ladder in `slot`, the manual stop calls in `context`)
//! perform exactly the effects these functions name.

use std::time::{Duration, Instant};

use crate::actor::runtime::FAR_FUTURE;
use crate::actor::supervision::{ChildLifecycle, DownPending, ManualStop, Origin, ABORT_SETTLE};
use crate::types::{
    ActorId, LaneState, RaiseResult, RestartStrategy, RestartType, Shutdown, StopReason,
    StopSeverity, SupervisionAction,
};

/// A request a stop raises on an incarnation's lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Signal {
    /// The vetoable request of a graceful stop.
    ParentRequest,
    /// The untrappable request that bypasses every callback.
    Kill,
}

impl Signal {
    pub(crate) fn reason(self) -> StopReason {
        match self {
            Signal::ParentRequest => StopReason::ParentRequest,
            Signal::Kill => StopReason::Kill,
        }
    }
}

/// The first signal of a stop, purely a function of the `Shutdown` policy:
/// `Kill` asks for the kill at once, `Timeout` and `Infinity` ask politely.
pub(crate) fn first_signal(policy: Shutdown) -> Signal {
    match policy {
        Shutdown::Kill => Signal::Kill,
        Shutdown::Timeout(_) | Shutdown::Infinity => Signal::ParentRequest,
    }
}

// ---------------------------------------------------------------------------
// The stop ladder
// ---------------------------------------------------------------------------

/// Where an incarnation's stop ladder stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LadderStep {
    /// No stop order has been read.
    Idle,
    /// The graceful request was raised. Under `Timeout` the window ends at
    /// `deadline`; under `Infinity` there is no deadline.
    Graceful { deadline: Option<Instant> },
    /// `Kill` was raised; the incarnation has until `deadline` to leave its
    /// loop before the task is aborted.
    KillGrace { deadline: Instant },
    /// The task was aborted; it has until `deadline` to end before the
    /// Unresponsive verdict is written.
    AbortGrace { deadline: Instant },
    /// The verdict was written: nothing is left to wait for but the task.
    Spent,
}

impl LadderStep {
    /// Where the ladder stands, without its deadlines.
    pub(crate) fn position(&self) -> AttemptLadder {
        match self {
            LadderStep::Idle => AttemptLadder::Off,
            LadderStep::Graceful { .. } => AttemptLadder::Window,
            LadderStep::KillGrace { .. } => AttemptLadder::KillGrace,
            LadderStep::AbortGrace { .. } => AttemptLadder::AbortGrace,
            LadderStep::Spent => AttemptLadder::Spent,
        }
    }

    /// The instant at which the ladder next acts, if it is waiting for one.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        match self {
            LadderStep::Graceful { deadline } => *deadline,
            LadderStep::KillGrace { deadline } | LadderStep::AbortGrace { deadline } => {
                Some(*deadline)
            }
            LadderStep::Idle | LadderStep::Spent => None,
        }
    }
}

/// What happened to the ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LadderEvent {
    /// A stop order was read.
    Order {
        policy: Shutdown,
        issued_at: Instant,
        initial_raised: bool,
    },
    /// The step's deadline passed.
    DeadlineReached,
}

/// What the ladder's owner does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LadderAction {
    None,
    /// Raise this request on the incarnation's stop lane.
    Raise(Signal),
    /// Abort the incarnation's task.
    Abort,
    /// Record that the incarnation outlived its whole ladder.
    WriteVerdict,
}

/// `base + after`, capped so the result stays a safe timer deadline.
fn deadline_after(base: Instant, after: Duration) -> Instant {
    base.checked_add(after.min(FAR_FUTURE)).unwrap_or(base)
}

/// The ladder position of a kill path an attempt starts on its own: `Kill` was
/// raised at `now`, and the incarnation has `grace` to leave its loop.
pub(crate) fn kill_grace_from(now: Instant, grace: Duration) -> LadderStep {
    LadderStep::KillGrace {
        deadline: deadline_after(now, grace),
    }
}

/// The one decision procedure of a stop ladder. `grace` is the
/// incarnation's kill grace: the time between its `Kill` and its abort.
///
/// An order starts the ladder: unless the order's first signal was already
/// raised (`initial_raised`), the ladder raises it and the policy clock
/// starts now; otherwise the clock started when the order was issued. A
/// `Kill` policy goes straight to the kill grace, a `Timeout` runs its
/// window first, `Infinity` waits without a deadline. A deadline reached in
/// the graceful window raises `Kill`; in the kill grace it aborts the task;
/// in the abort wait it writes the verdict.
pub(crate) fn ladder_step(
    step: LadderStep,
    event: LadderEvent,
    now: Instant,
    grace: Duration,
) -> (LadderStep, LadderAction) {
    match (step, event) {
        (
            LadderStep::Idle,
            LadderEvent::Order {
                policy,
                issued_at,
                initial_raised,
            },
        ) => {
            let (base, action) = if initial_raised {
                (issued_at, LadderAction::None)
            } else {
                (now, LadderAction::Raise(first_signal(policy)))
            };
            let next = match policy {
                Shutdown::Kill => LadderStep::KillGrace {
                    deadline: deadline_after(base, grace),
                },
                Shutdown::Timeout(window) => LadderStep::Graceful {
                    deadline: Some(deadline_after(base, window)),
                },
                Shutdown::Infinity => LadderStep::Graceful { deadline: None },
            };
            (next, action)
        }
        (LadderStep::Graceful { deadline: Some(_) }, LadderEvent::DeadlineReached) => (
            LadderStep::KillGrace {
                deadline: deadline_after(now, grace),
            },
            LadderAction::Raise(Signal::Kill),
        ),
        (LadderStep::KillGrace { .. }, LadderEvent::DeadlineReached) => (
            LadderStep::AbortGrace {
                deadline: deadline_after(now, ABORT_SETTLE),
            },
            LadderAction::Abort,
        ),
        (LadderStep::AbortGrace { .. }, LadderEvent::DeadlineReached) => {
            (LadderStep::Spent, LadderAction::WriteVerdict)
        }
        (step, _) => (step, LadderAction::None),
    }
}

// ---------------------------------------------------------------------------
// The manual stop calls
// ---------------------------------------------------------------------------

/// How a child relates to the group restart in flight, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GroupMembership {
    /// No group restart is in flight, or the child is not part of it.
    None,
    /// The group's stop phase awaits this member's end. `overridden` is set
    /// when a `terminate_child` recorded that the group must not restart it.
    Awaited { overridden: bool },
    /// The member waits in the group's restart queue.
    Queued,
    /// A `terminate_child` released the member from the group: its end was
    /// handled and the group will not restart it.
    Released,
}

/// What the supervisor can see of the incarnation's task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskState {
    /// The task runs and its runner owns the ladder.
    Live,
    /// The task runs but its runner failed: no one runs its ladder.
    RunnerLost,
    /// The task ended (its fate is recorded, or its runner failed and the
    /// task has since ended); its death is not yet handled.
    Ended,
}

/// The reply a manual call can give without waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManualReply {
    Ok,
    ChildNotFound,
    ChildRestarting,
    ChildRunning,
    /// `stop_child` of a member the pending group restart will stop and
    /// restart anyway: the call changed nothing.
    Absorbed,
}

/// One step of a manual stop call, executed in order with no await between
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitEffect {
    /// Raise the first signal of the child's `Shutdown` policy on its lane.
    RaiseFirst,
    /// Write the child's stop order, so its runner escalates the stop.
    WriteOrder,
    /// Record that the pending group restart must leave the child down.
    RecordOverride,
    /// Take the child out of the pending group's restart set, so nothing the
    /// group does later restarts it.
    LeaveRestartOrder,
    /// Wait for the child's end on its slot, then classify the call.
    Join,
    /// One more request to a vetoing `Infinity` child whose stop this call
    /// joins.
    Reask,
    /// The same request, given only if the one-more-ask record of the
    /// child's slot is unset, and recording it: a pending group restart and
    /// the `terminate_child` calls that join the member during it share one
    /// ask.
    ReaskOnce,
    /// Classify the call with the reason the already ended task ended with.
    Classify,
    /// Dismiss the restart decision a dead child still owes: its report is
    /// delivered from a snapshot that says the child was not restarted, and
    /// the child stays down.
    Dismiss,
    /// A pending snapshot of the child's end that reports a restart is
    /// rewritten to report the removal the release made.
    RewriteSnapshot,
    /// Answer the call.
    Answer(ManualReply),
}

/// The decision of a manual stop call: the ledger state it commits, then the
/// effects it performs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Commit {
    pub to: Option<ChildLifecycle>,
    pub effects: Vec<CommitEffect>,
}

fn answer(reply: ManualReply) -> Commit {
    Commit {
        to: None,
        effects: vec![CommitEffect::Answer(reply)],
    }
}

/// What a manual call reads of its child and of the pending group restart.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ManualFacts {
    /// The child's ledger entry; `None` when no spec exists.
    pub life: Option<ChildLifecycle>,
    /// What the supervisor can see of the occupant's task.
    pub task: TaskState,
    /// The child's relation to the pending group restart.
    pub membership: GroupMembership,
    /// The child's restart type.
    pub restart_type: RestartType,
    /// The supervisor is a pool: a stopped child's spec is removed.
    pub pool: bool,
    /// A chain of the pending group restart already restarted the key.
    pub restarted: bool,
    /// A group restart is pending and covers the key: every key under
    /// `OneForAll`, every key not before the first failed member under
    /// `RestForOne`. A `terminate_child` of a key the group does not hold yet
    /// records that the group must leave it down.
    pub in_keep_scope: bool,
    /// The child's `Shutdown` policy.
    pub policy: Shutdown,
    /// The occupant's lane holds a request its `pre_stop` may still veto.
    pub lane_vetoable: bool,
    /// The one-more-ask record of the occupant's slot is set.
    pub reasked: bool,
    /// The occupant's slot already holds a stop order.
    pub has_order: bool,
}

impl ManualFacts {
    fn prunable(&self) -> bool {
        self.pool || self.restart_type == RestartType::Temporary
    }

    fn permanent(&self) -> bool {
        self.restart_type == RestartType::Permanent
    }

    /// A vetoing `Infinity` child whose stop a join can ask once more.
    fn ask_possible(&self) -> bool {
        self.task != TaskState::Ended && self.policy == Shutdown::Infinity && self.lane_vetoable
    }
}

/// How `stop_child` treats a child, from its membership in the pending group
/// restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopRoute {
    /// The ordinary bounce.
    Bounce,
    /// Handled as the same child's `terminate_child` is.
    AsTerminate,
    /// The group restart covers the call: nothing changes.
    Absorbed,
    /// The call is refused until the group restart completes.
    Refused,
}

/// The route of a `stop_child`. Pending-group membership decides first, read
/// with the child's restart type: a `Permanent` member whose restart by the
/// current plan is still ahead is absorbed, one the group will not restart
/// (released) or whose restart attempt is in flight at the chain's front is
/// refused, and a `Permanent` key a chain already restarted is refused. A
/// `Transient` or `Temporary` child follows its own policy: it is stopped as
/// `terminate_child` stops it, which keeps it down through the group restart.
fn stop_route(f: &ManualFacts) -> StopRoute {
    let permanent = f.permanent();
    let attempt = matches!(f.life, Some(ChildLifecycle::Restarting { .. }));
    let own_policy = if permanent {
        StopRoute::Refused
    } else {
        StopRoute::AsTerminate
    };
    match f.membership {
        GroupMembership::Queued if attempt => StopRoute::Refused,
        GroupMembership::Awaited { overridden: false } | GroupMembership::Queued => {
            if permanent {
                StopRoute::Absorbed
            } else {
                StopRoute::AsTerminate
            }
        }
        GroupMembership::Awaited { overridden: true } | GroupMembership::Released => own_policy,
        GroupMembership::None if f.restarted => own_policy,
        GroupMembership::None if !permanent && f.in_keep_scope => StopRoute::AsTerminate,
        GroupMembership::None => StopRoute::Bounce,
    }
}

/// The decision of `stop_child` (`ManualStop::Bounce`) and `terminate_child`
/// (`ManualStop::Terminate`), from the facts of the call.
///
/// An attempt in flight is refused first, whatever the membership. A
/// `stop_child` of a member of a pending group restart is then decided by the
/// membership ([`StopRoute`]); `terminate_child` is total: it succeeds for
/// every known child in every state but an attempt in flight, and a
/// `terminate_child` made while a group restart is pending records that the
/// group must leave a child down that the group has not told to stop.
/// Outside a group, a live child is committed to a stop in one step (ledger,
/// first signal, stop order) and the call waits for the end; a stop already
/// in progress is joined, one more request going to a vetoing `Infinity`
/// child, and a `terminate_child` joining a bounce upgrades it; a child whose
/// task ended before the call is classified at once; a child that is down is
/// left as it is, an owed restart decision dismissed by `terminate_child`.
pub(crate) fn manual_commit(kind: ManualStop, f: &ManualFacts) -> Commit {
    let Some(life) = f.life else {
        return answer(ManualReply::ChildNotFound);
    };
    match kind {
        ManualStop::Terminate => terminate_commit(life, f),
        ManualStop::Bounce => match stop_route(f) {
            StopRoute::Absorbed => answer(ManualReply::Absorbed),
            StopRoute::Refused => answer(ManualReply::ChildRestarting),
            StopRoute::AsTerminate => terminate_commit(life, f),
            StopRoute::Bounce => bounce_commit(life, f),
        },
    }
}

/// The kind a manual stop is classified with when it completes: a
/// `terminate_child`'s, and also a `stop_child`'s that is handled as the
/// child's `terminate_child`.
pub(crate) fn effective_kind(kind: ManualStop, f: &ManualFacts) -> ManualStop {
    if kind == ManualStop::Bounce && stop_route(f) == StopRoute::AsTerminate {
        ManualStop::Terminate
    } else {
        kind
    }
}

/// The ordinary cells of `stop_child`: a child no pending group restart
/// holds.
fn bounce_commit(life: ChildLifecycle, f: &ManualFacts) -> Commit {
    use CommitEffect::{Answer, Classify, Join, RaiseFirst, Reask, WriteOrder};
    match life {
        ChildLifecycle::Restarting { .. } => answer(ManualReply::ChildRestarting),
        ChildLifecycle::Running(incarnation) => match f.task {
            TaskState::Ended => Commit {
                to: None,
                effects: vec![Classify, Answer(ManualReply::Ok)],
            },
            TaskState::Live => Commit {
                to: Some(ChildLifecycle::Stopping {
                    incarnation,
                    kind: ManualStop::Bounce,
                }),
                effects: vec![RaiseFirst, WriteOrder, Join],
            },
            TaskState::RunnerLost => Commit {
                to: Some(ChildLifecycle::Stopping {
                    incarnation,
                    kind: ManualStop::Bounce,
                }),
                effects: vec![WriteOrder, Join],
            },
        },
        ChildLifecycle::Stopping { .. } => Commit {
            to: None,
            effects: if f.ask_possible() {
                vec![Reask, Join]
            } else {
                vec![Join]
            },
        },
        // A death that owes its restart decision is refused: no bounce
        // decides it, and the decision restarts the child once it is made.
        ChildLifecycle::Down {
            pending: DownPending::Evaluate,
            ..
        } => answer(ManualReply::ChildRestarting),
        ChildLifecycle::Down { .. } => answer(ManualReply::Ok),
    }
}

/// The cells of `terminate_child`, which `stop_child` takes for a child that
/// is not `Permanent` and that a pending group restart holds, or has
/// restarted, or covers.
fn terminate_commit(life: ChildLifecycle, f: &ManualFacts) -> Commit {
    use CommitEffect::{
        Answer, Classify, Dismiss, Join, LeaveRestartOrder, RaiseFirst, Reask, ReaskOnce,
        RecordOverride, RewriteSnapshot, WriteOrder,
    };
    // An attempt in flight is refused whatever the membership: a handler that
    // waited for it could wait on an adoption only the loop it occupies can
    // make.
    if matches!(life, ChildLifecycle::Restarting { .. }) {
        return answer(ManualReply::ChildRestarting);
    }
    let terminate = |incarnation| ChildLifecycle::Stopping {
        incarnation,
        kind: ManualStop::Terminate,
    };
    // The one ask a join gives: shared with the group restart when the call
    // records the key in it.
    let ask = |shared: bool| -> Option<CommitEffect> {
        if !f.ask_possible() {
            None
        } else if shared {
            (!f.reasked).then_some(ReaskOnce)
        } else {
            Some(Reask)
        }
    };
    let with = |head: Option<CommitEffect>, tail: &[CommitEffect]| -> Vec<CommitEffect> {
        head.into_iter().chain(tail.iter().copied()).collect()
    };
    // The upgrade of a bounce in progress.
    let upgraded =
        |incarnation, in_flight| (in_flight == ManualStop::Bounce).then(|| terminate(incarnation));
    match f.membership {
        GroupMembership::None => {
            let keep = f.in_keep_scope;
            let record = |spec_kept: bool| (keep && spec_kept).then_some(RecordOverride);
            match life {
                ChildLifecycle::Restarting { .. } => answer(ManualReply::ChildRestarting),
                ChildLifecycle::Running(incarnation) => match f.task {
                    TaskState::Ended => Commit {
                        to: None,
                        effects: with(
                            Some(Classify),
                            &with(record(!f.prunable()), &[Answer(ManualReply::Ok)]),
                        ),
                    },
                    TaskState::Live => Commit {
                        to: Some(terminate(incarnation)),
                        effects: with(
                            Some(RaiseFirst),
                            &with(Some(WriteOrder), &with(record(true), &[Join])),
                        ),
                    },
                    TaskState::RunnerLost => Commit {
                        to: Some(terminate(incarnation)),
                        effects: with(Some(WriteOrder), &with(record(true), &[Join])),
                    },
                },
                ChildLifecycle::Stopping {
                    incarnation,
                    kind: in_flight,
                } => Commit {
                    to: upgraded(incarnation, in_flight),
                    effects: with(ask(keep), &with(record(true), &[Join])),
                },
                ChildLifecycle::Down {
                    pending: DownPending::Evaluate,
                    ..
                } => Commit {
                    to: None,
                    effects: with(
                        Some(Dismiss),
                        &with(record(!f.prunable()), &[Answer(ManualReply::Ok)]),
                    ),
                },
                ChildLifecycle::Down { .. } => Commit {
                    to: None,
                    effects: with(record(true), &[Answer(ManualReply::Ok)]),
                },
            }
        }
        // The group's stop phase awaits this member's end: the call records
        // that the member stays down and waits for that end, upgrading a
        // bounce in progress so the end is classified as a terminate. A
        // running member the stop queue has not reached has no order yet and
        // gets it from the call, since the call would otherwise wait on a
        // queue that only the loop it occupies can advance. A member whose
        // stop is in progress, by the queue's order, a caller's manual stop
        // or an earlier call of this kind that was abandoned, is joined, with
        // the one more ask that a group restart and the calls that join the
        // member during it share.
        GroupMembership::Awaited { .. } => {
            let release = [RecordOverride, LeaveRestartOrder];
            match life {
                ChildLifecycle::Running(_) => {
                    let stop: Vec<CommitEffect> = match (f.task, f.has_order) {
                        (TaskState::Ended, _) => Vec::new(),
                        (TaskState::Live, false) => vec![RaiseFirst, WriteOrder],
                        (TaskState::RunnerLost, false) => vec![WriteOrder],
                        (_, true) => ask(true).into_iter().collect(),
                    };
                    Commit {
                        to: None,
                        effects: release.into_iter().chain(stop).chain([Join]).collect(),
                    }
                }
                ChildLifecycle::Stopping {
                    incarnation,
                    kind: in_flight,
                } => Commit {
                    to: upgraded(incarnation, in_flight),
                    effects: release.into_iter().chain(ask(true)).chain([Join]).collect(),
                },
                _ => answer(ManualReply::ChildRestarting),
            }
        }
        // The member waits in the group's restart set: the call takes it out
        // of the set, so nothing the group does later starts it.
        GroupMembership::Queued => match life {
            ChildLifecycle::Down {
                pending: DownPending::Evaluate,
                ..
            } => Commit {
                to: None,
                effects: vec![
                    Dismiss,
                    RecordOverride,
                    LeaveRestartOrder,
                    Answer(ManualReply::Ok),
                ],
            },
            ChildLifecycle::Down {
                pending: DownPending::Snapshot,
                ..
            } => Commit {
                to: None,
                effects: vec![
                    RecordOverride,
                    LeaveRestartOrder,
                    RewriteSnapshot,
                    Answer(ManualReply::Ok),
                ],
            },
            ChildLifecycle::Down { .. } => Commit {
                to: None,
                effects: vec![RecordOverride, LeaveRestartOrder, Answer(ManualReply::Ok)],
            },
            _ => answer(ManualReply::ChildRestarting),
        },
        // A `terminate_child` released the member: its stop in progress is
        // joined (the one more ask shared with the group), and a member that
        // is down stays so.
        GroupMembership::Released => match life {
            ChildLifecycle::Down { .. } => answer(ManualReply::Ok),
            ChildLifecycle::Stopping {
                kind: ManualStop::Terminate,
                ..
            } => Commit {
                to: None,
                effects: with(ask(true), &[Join]),
            },
            _ => answer(ManualReply::ChildRestarting),
        },
    }
}

/// The manual calls whose answer a child's state decides.
// The variants are named after the calls.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManualOp {
    /// Answered through [`manual_commit`], which executes the call; the
    /// answer alone is read by the conformance check.
    #[cfg_attr(not(test), allow(dead_code))]
    StopChild,
    #[cfg_attr(not(test), allow(dead_code))]
    TerminateChild,
    RestartChild,
    DeleteChild,
}

/// The answer a manual call gives without waiting, from the facts of the
/// call: `Ok` when the call goes on (a stop it commits or joins, an
/// immediate success), or the refusal or the absorption it answers with.
///
/// `stop_child` and `terminate_child` are answered by [`manual_commit`], the
/// answer being its bare `Answer` effect, or `Ok` for a call that goes on.
///
/// `restart_child` and `delete_child`: a member of a pending group is the
/// group's until it completes. Both calls are refused for a member the group
/// awaits or will restart, `restart_child` also for a member a
/// `terminate_child` released from it and for a key a chain of the group
/// already restarted, while `delete_child` answers a released member by its
/// own state. A child is otherwise answered by its state: a running child
/// whose task still runs is `ChildRunning`; one whose task ended, one being
/// stopped, one with an attempt in flight and one whose death awaits its
/// restart decision are `ChildRestarting`; a down child is `Ok`. A death is
/// never classified to answer.
pub(crate) fn manual_answer(op: ManualOp, f: &ManualFacts) -> ManualReply {
    let kind = match op {
        ManualOp::StopChild => Some(ManualStop::Bounce),
        ManualOp::TerminateChild => Some(ManualStop::Terminate),
        ManualOp::RestartChild | ManualOp::DeleteChild => None,
    };
    if let Some(kind) = kind {
        return match manual_commit(kind, f).effects.as_slice() {
            [CommitEffect::Answer(reply)] => *reply,
            _ => ManualReply::Ok,
        };
    }
    let Some(life) = f.life else {
        return ManualReply::ChildNotFound;
    };
    let refused = match f.membership {
        GroupMembership::Awaited { .. } | GroupMembership::Queued => true,
        GroupMembership::Released => op == ManualOp::RestartChild,
        GroupMembership::None => false,
    };
    if refused {
        return ManualReply::ChildRestarting;
    }
    match life {
        ChildLifecycle::Running(_) if f.task == TaskState::Ended => ManualReply::ChildRestarting,
        ChildLifecycle::Running(_) if op == ManualOp::RestartChild && f.restarted => {
            ManualReply::ChildRestarting
        }
        ChildLifecycle::Running(_) => ManualReply::ChildRunning,
        ChildLifecycle::Stopping { .. } | ChildLifecycle::Restarting { .. } => {
            ManualReply::ChildRestarting
        }
        ChildLifecycle::Down {
            pending: DownPending::Evaluate,
            ..
        } => ManualReply::ChildRestarting,
        ChildLifecycle::Down { .. } => ManualReply::Ok,
    }
}

// ---------------------------------------------------------------------------
// The child ledger
// ---------------------------------------------------------------------------

/// The default-closed transition table of a child's ledger entry: the eight
/// legal moves, each keyed by the incarnation it concerns. A move that names
/// another incarnation than the entry holds is rejected like any other
/// pair the table does not list.
///
/// - a running incarnation is stopped by a manual stop, or ends;
/// - a stopping incarnation ends, and a bounce in progress is upgraded to a
///   terminate;
/// - a restart attempt is adopted as the running incarnation, or ends as the
///   one the ledger recorded for it;
/// - a new incarnation begins only from a down spec, with a token drawn after
///   the last incarnation it held;
/// - a down spec records what its last end still owes.
pub(crate) fn is_legal(from: &ChildLifecycle, to: &ChildLifecycle) -> bool {
    use ChildLifecycle::{Down, Restarting, Running, Stopping};
    match (*from, *to) {
        (Running(n), Stopping { incarnation: m, .. }) => n == m,
        (Running(n), Down { incarnation: m, .. }) => n == m,
        (Stopping { incarnation: n, .. }, Down { incarnation: m, .. }) => n == m,
        (
            Stopping {
                incarnation: n,
                kind: ManualStop::Bounce,
            },
            Stopping {
                incarnation: m,
                kind: ManualStop::Terminate,
            },
        ) => n == m,
        (Restarting { next, .. }, Running(m)) => next == m,
        (Restarting { next, .. }, Down { incarnation: m, .. }) => next == m,
        (
            Down { incarnation: n, .. },
            Restarting {
                incarnation: p,
                next,
            },
        ) => n == p && next > n,
        (Down { incarnation: n, .. }, Down { incarnation: m, .. }) => n == m,
        _ => false,
    }
}

/// How a report of an incarnation's end relates to the ledger entry it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReportClass {
    /// The report is of the entry's occupant.
    Occupant,
    /// The entry is a down spec whose last end owes its restart decision,
    /// and the report is of that end.
    OwedEvaluation,
    /// Anything else: the report is of an incarnation the entry has moved
    /// past, and changes nothing about it.
    Stale,
}

/// The class of a report of incarnation `n` against a ledger entry.
pub(crate) fn report_class(life: ChildLifecycle, n: u64) -> ReportClass {
    if life.occupant() == Some(n) {
        ReportClass::Occupant
    } else if matches!(
        life,
        ChildLifecycle::Down {
            incarnation,
            pending: DownPending::Evaluate,
        } if incarnation == n
    ) {
        ReportClass::OwedEvaluation
    } else {
        ReportClass::Stale
    }
}

/// `(is_alive, restart_pending)` of a child as `children()` reports it.
/// `fate_present` is whether the supervisor can see that the occupant's task
/// ended; `in_restart_set` whether the pending group restart will restart
/// the child.
///
/// A started incarnation is alive until its stop, if any, completed, and a
/// restart is pending while an attempt is in flight, while a death awaits
/// its restart decision, and while the pending group will restart the child.
pub(crate) fn child_info_flags(
    life: ChildLifecycle,
    fate_present: bool,
    in_restart_set: bool,
) -> (bool, bool) {
    match life {
        ChildLifecycle::Running(_) => (!fate_present, fate_present),
        ChildLifecycle::Stopping { .. } => (!fate_present, false),
        ChildLifecycle::Restarting { .. } => (false, true),
        ChildLifecycle::Down { pending, .. } => {
            (false, pending == DownPending::Evaluate || in_restart_set)
        }
    }
}

/// What a group restart's chain does with the member at its front.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrontAction {
    /// The member runs on the incarnation the chain started: it is done.
    Fresh,
    /// The member is down: the chain starts it.
    Initiate,
    /// The member runs an incarnation the chain did not start: the chain
    /// stops it, holds until its end, then starts it in its slot.
    StopThenInitiate,
    /// The member is being stopped or an attempt for it is in flight: the
    /// chain holds.
    Hold,
}

/// The chain's decision for its front member, from the member's ledger entry
/// and the incarnation the chain started for it, if any.
pub(crate) fn chain_front(life: ChildLifecycle, started: Option<u64>) -> FrontAction {
    match life {
        ChildLifecycle::Down { .. } => FrontAction::Initiate,
        ChildLifecycle::Running(k) if started == Some(k) => FrontAction::Fresh,
        ChildLifecycle::Running(_) => FrontAction::StopThenInitiate,
        ChildLifecycle::Stopping { .. } | ChildLifecycle::Restarting { .. } => FrontAction::Hold,
    }
}

/// What the supervisor decides when a manual stop completes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManualOutcome {
    /// The action the child's `on_child_stopped` snapshot carries; `None`
    /// when no snapshot is made because the end is evaluated as a crash.
    pub action: Option<SupervisionAction>,
    /// What the ended incarnation owes in the ledger.
    pub pending: DownPending,
    /// The spec is removed instead of kept.
    pub remove: bool,
    /// A restart of the child is initiated, free of the restart budget.
    pub initiate: bool,
}

/// The decision of a completed manual stop, from its kind, the reason the
/// incarnation really ended with, the child's restart type, whether the
/// supervisor is a pool and whether the child is in the pending group
/// restart's restart set.
///
/// A bounce that met a crash is the crash: the end is recorded as owing its
/// restart decision, and evaluated and charged as any crash. A stop of either
/// kind that ended a member of the pending group's restart set is that
/// group's: the end is kept for its report as a restart, and nothing is
/// started or removed. Otherwise a terminate keeps the child down, a bounce
/// restarts a `Permanent` child without charge, and a pool child or a
/// `Temporary` one is removed.
pub(crate) fn classify_manual(
    kind: ManualStop,
    reason: &StopReason,
    restart_type: RestartType,
    pool: bool,
    in_restart_set: bool,
) -> ManualOutcome {
    if kind == ManualStop::Bounce && matches!(reason, StopReason::Failure(_)) {
        return ManualOutcome {
            action: None,
            pending: DownPending::Evaluate,
            remove: false,
            initiate: false,
        };
    }
    if in_restart_set {
        // A stop of either kind that ended a member the pending group will
        // restart belongs to that group: the group restarts the member in
        // its slot, and the end is reported as that restart.
        return ManualOutcome {
            action: Some(SupervisionAction::RestartInitiated),
            pending: DownPending::Snapshot,
            remove: false,
            initiate: false,
        };
    }
    let prunable = pool || restart_type == RestartType::Temporary;
    let stays_down = ManualOutcome {
        action: Some(SupervisionAction::Removed),
        pending: if prunable {
            DownPending::None
        } else {
            DownPending::Snapshot
        },
        remove: prunable,
        initiate: false,
    };
    match kind {
        ManualStop::Terminate => stays_down,
        ManualStop::Bounce if pool => stays_down,
        ManualStop::Bounce if restart_type == RestartType::Permanent => ManualOutcome {
            action: Some(SupervisionAction::RestartInitiated),
            pending: DownPending::Snapshot,
            remove: false,
            initiate: true,
        },
        ManualStop::Bounce => stays_down,
    }
}

/// How the failure of a restart attempt is handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// The chain's own start failed: the chain retries it, charged.
    ChainRetry,
    /// The group committed the child: the failure belongs to the group,
    /// which restarts the child in its slot, charge-free.
    Member,
    /// A manual restart failed: the child stays down with its spec, reported
    /// removed, charge-free, and no strategy runs.
    Removed,
    /// The ordinary death path: evaluated by the strategy, or queued behind
    /// the pending group restart.
    Evaluate,
}

/// The route of a failed restart attempt. The chain front's own attempt is
/// tested first, then the child's membership in the pending group, then the
/// attempt's origin: a failed chain start reads as a queued member and still
/// takes the retry, and a manual attempt of a member the group committed
/// takes the member route.
pub(crate) fn attempt_failure_route(
    chain_front_own: bool,
    membership: GroupMembership,
    origin: Origin,
) -> Route {
    if chain_front_own {
        Route::ChainRetry
    } else if matches!(
        membership,
        GroupMembership::Awaited { .. } | GroupMembership::Queued
    ) {
        Route::Member
    } else if origin == Origin::Manual {
        Route::Removed
    } else {
        Route::Evaluate
    }
}

// ---------------------------------------------------------------------------
// The restart attempt
// ---------------------------------------------------------------------------

/// Where a restart attempt's own ladder stands, without its deadlines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptLadder {
    /// No order was read and no start timeout acted.
    Off,
    /// An order's graceful window runs.
    Window,
    /// `Kill` was raised; the incarnation has its kill grace.
    KillGrace,
    /// The incarnation was aborted; it has the settle time.
    AbortGrace,
    /// The verdict was written.
    Spent,
}

/// The stage of a restart attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptStage {
    /// Before the factory is called.
    Factory,
    /// The incarnation was spawned and initializes.
    Init,
    /// The attempt returned its outcome; the supervisor owns the rest.
    #[cfg_attr(not(test), allow(dead_code))]
    InTransit,
}

/// What an attempt's executor observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptEvent {
    /// The attempt read its command cell before calling the factory.
    /// `ordered`: the cell holds a stop order or the cancel flag of a group
    /// restart; the flag acts at this stage and while the attempt waits for
    /// the previous incarnation's subtree, and nowhere after the spawn.
    Begin { ordered: bool },
    /// A stop order was read while the incarnation initializes.
    Order,
    /// The init acknowledgment arrived `Ok`. `ordered`: the command cell
    /// holds an order. `ended`: the incarnation's task already ended.
    Ack { ordered: bool, ended: bool },
    /// The init returned an error, or the acknowledgment channel closed.
    InitEnded,
    /// The start timeout expired.
    StartTimeout,
    /// The ladder's two waits passed and the incarnation still runs.
    LadderSpent,
    /// The incarnation's task ended. `ordered`: the command cell holds an
    /// order. `timeout_raised`: the start timeout's expiry raised the first
    /// `Kill`.
    Ended { ordered: bool, timeout_raised: bool },
}

/// What an attempt's executor knows besides the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AttemptView {
    pub ladder: AttemptLadder,
    /// A `Kill` is already stored on the incarnation's lane.
    pub kill_stored: bool,
    /// The init acknowledgment was read.
    pub acked: bool,
}

/// How an attempt reports the end of its incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptReport {
    /// The incarnation ended under the order the attempt read.
    Stopped,
    /// The start timeout's expiry raised the first `Kill`: the start failed.
    StartTimeout,
    /// The incarnation ended before it was adopted, under no order.
    Failed,
}

/// What an attempt's executor does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptAction {
    None,
    /// Record the cancel and return `Cancelled`: no incarnation existed.
    Cancel,
    /// Call the factory and spawn the incarnation.
    Spawn,
    /// Start the ladder from the order.
    ArmLadder,
    /// Return the incarnation, acknowledged, to the supervisor.
    Adopt,
    /// Run the kill path: raise `Kill` unless one is stored, then the kill
    /// grace, the abort and the settle time.
    KillPath {
        raise: bool,
    },
    /// Write the Unresponsive verdict and keep waiting for the task.
    WriteVerdict,
    /// Record the fate and return this report.
    Report(AttemptReport),
}

/// The one decision procedure of a restart attempt's executor.
///
/// An order or a cancel present before the factory cancels the attempt (the
/// caller reads both as one input, `ordered`). During the init
/// an order starts the one ladder at once. An acknowledgment read under no
/// order, before the ladder acted, hands the incarnation over, even when its
/// task already ended. The start timeout acts whenever the init has not
/// acknowledged and the ladder is not past its window: it raises `Kill`
/// unless one is stored, whatever stored it, and runs the kill grace, the
/// abort and the settle time itself. A join still running after the ladder's
/// two waits gets the Unresponsive verdict, never a fate. The end of the task
/// is reported as a start failure only when the start timeout raised the
/// first `Kill`, as stopped when it ended under an order, and as failed
/// otherwise.
pub(crate) fn attempt_step(
    stage: AttemptStage,
    event: AttemptEvent,
    view: AttemptView,
) -> AttemptAction {
    match (stage, event) {
        (AttemptStage::Factory, AttemptEvent::Begin { ordered: true }) => AttemptAction::Cancel,
        (AttemptStage::Factory, AttemptEvent::Begin { ordered: false }) => AttemptAction::Spawn,
        (AttemptStage::Init, AttemptEvent::Order) => AttemptAction::ArmLadder,
        (AttemptStage::Init, AttemptEvent::Ack { ordered, .. }) => {
            if !ordered && view.ladder == AttemptLadder::Off {
                AttemptAction::Adopt
            } else {
                AttemptAction::None
            }
        }
        (AttemptStage::Init, AttemptEvent::InitEnded) => {
            if view.ladder == AttemptLadder::Off {
                AttemptAction::KillPath {
                    raise: !view.kill_stored,
                }
            } else {
                AttemptAction::None
            }
        }
        (AttemptStage::Init, AttemptEvent::StartTimeout) => {
            if !view.acked && matches!(view.ladder, AttemptLadder::Off | AttemptLadder::Window) {
                AttemptAction::KillPath {
                    raise: !view.kill_stored,
                }
            } else {
                AttemptAction::None
            }
        }
        (AttemptStage::Init, AttemptEvent::LadderSpent) => AttemptAction::WriteVerdict,
        (
            AttemptStage::Init,
            AttemptEvent::Ended {
                ordered,
                timeout_raised,
            },
        ) => AttemptAction::Report(if timeout_raised {
            AttemptReport::StartTimeout
        } else if ordered {
            AttemptReport::Stopped
        } else {
            AttemptReport::Failed
        }),
        _ => AttemptAction::None,
    }
}

// ---------------------------------------------------------------------------
// The subtree wait and the budgets it is bounded by
// ---------------------------------------------------------------------------

/// What one wake-up of a wait on a previous incarnation's subtree decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitAction {
    /// Nothing decides yet: the wait goes on.
    Keep,
    /// The subtree has ended: the waiter goes on.
    Proceed,
    /// A stop order reached the waiting restart attempt: it is cancelled and
    /// nothing is built.
    Cancel,
    /// The subtree outlived its bound: the waiter reports the previous
    /// incarnation unresponsive.
    Escalate,
}

/// The one decision of a wait on a previous incarnation's subtree, taken at
/// every wake-up of a restart attempt that waits before its factory, of a
/// `terminate_child` that waits for the subtree of the child it stopped, and
/// of a `spawn_child` that waits for the subtree of a removed child.
///
/// A stop order or a cancel (read together as `order`) cancels a waiting
/// attempt first, whether or not the subtree has ended, since an attempt that
/// finds either before its factory builds nothing. Without an order, the end
/// of the subtree lets the waiter go on, and the expiry of the bound, with
/// the subtree still alive, is the verdict. A handler's wait never has an
/// order.
pub(crate) fn successor_wait_step(gone: bool, order: bool, expired: bool) -> WaitAction {
    if order {
        WaitAction::Cancel
    } else if gone {
        WaitAction::Proceed
    } else if expired {
        WaitAction::Escalate
    } else {
        WaitAction::Keep
    }
}

/// The nanoseconds of a duration, saturating: a duration beyond what a `u64`
/// of nanoseconds holds reads as `u64::MAX`, which means unbounded.
pub(crate) fn saturating_nanos(dur: Duration) -> u64 {
    u64::try_from(dur.as_nanos()).unwrap_or(u64::MAX)
}

/// The time an incarnation can outlive a killed parent, where one
/// incarnation's inherited stop bounds a wait. A worker is killed and aborted:
/// its kill grace plus the settle time after the abort, whatever its own
/// `Shutdown`. A supervisor child stops in order: its own `Shutdown` window
/// first (none under `Shutdown::Kill`), then its kill grace and the settle
/// time. `None` when the sum saturates, which reads as unbounded, and for a
/// supervisor child under `Shutdown::Infinity`, whose orderly stop has no
/// bound.
pub(crate) fn own_budget(
    is_supervisor: bool,
    shutdown: Shutdown,
    kill_grace: Duration,
) -> Option<Duration> {
    let tail = saturating_nanos(kill_grace).saturating_add(saturating_nanos(ABORT_SETTLE));
    let sum = match (is_supervisor, shutdown) {
        (true, Shutdown::Timeout(window)) => saturating_nanos(window).saturating_add(tail),
        (true, Shutdown::Infinity) => return None,
        _ => tail,
    };
    (sum != u64::MAX).then(|| Duration::from_nanos(sum))
}

/// The value a child adds to the budget its subtree carries: its
/// [`own_budget`] as a plain duration, `Duration::MAX` where that reads as
/// unbounded. A supervisor child under `Shutdown::Infinity` adds its kill
/// grace plus the settle time: the wait on its orderly stop is unbounded
/// while it runs, which the subtree token's count of such children carries,
/// and finite once none remains.
pub(crate) fn own_fin(is_supervisor: bool, shutdown: Shutdown, kill_grace: Duration) -> Duration {
    let budget = if is_supervisor && shutdown == Shutdown::Infinity {
        own_budget(false, shutdown, kill_grace)
    } else {
        own_budget(is_supervisor, shutdown, kill_grace)
    };
    budget.unwrap_or(Duration::MAX)
}

/// What the link guard of a child does when the supervisor that owns it ends
/// without stopping it: how the child is told to stop and when it is
/// forced to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InheritedStop {
    /// A worker: `Kill` is raised at once, and the task is aborted once
    /// `grace` has passed.
    Kill { grace: Duration },
    /// A supervisor child: `ParentKilled` is raised at once and the child
    /// stops its own children in order. After `window` (`None` is no bound)
    /// `Kill` is raised, and the task is aborted `grace` after that `Kill`.
    Orderly {
        window: Option<Duration>,
        grace: Duration,
    },
}

/// The inherited stop of a child: a worker gets `Kill`, whatever its own
/// `Shutdown`; a supervisor child gets an orderly stop bounded by its own
/// `Shutdown` window, which is zero under `Shutdown::Kill` and none under
/// `Shutdown::Infinity`.
pub(crate) fn inherited_stop(
    is_supervisor: bool,
    shutdown: Shutdown,
    kill_grace: Duration,
) -> InheritedStop {
    if !is_supervisor {
        return InheritedStop::Kill { grace: kill_grace };
    }
    InheritedStop::Orderly {
        window: match shutdown {
            Shutdown::Kill => Some(Duration::ZERO),
            Shutdown::Timeout(window) => Some(window),
            Shutdown::Infinity => None,
        },
        grace: kill_grace,
    }
}

/// What a firing guard does once it has raised its stop: when the child's
/// lane already holds a `Kill` (its runner or a restart attempt raised it),
/// that `Kill` is owed an abort its own kill grace after it, whatever the
/// child's `Shutdown`, so the guard feeds the worker's entry instead of an
/// orderly one.
pub(crate) fn fired_stop(inherited: InheritedStop, kill_stored: bool) -> InheritedStop {
    match inherited {
        InheritedStop::Orderly { grace, .. } if kill_stored => InheritedStop::Kill { grace },
        other => other,
    }
}

/// The bound of a wait on a subtree that carries `below_nanos` of budget
/// beneath its root: that budget plus the settle time as scheduling slack.
/// Unbounded (`None`) only while a supervisor child with an unbounded orderly
/// stop is alive below, or when the saturated sum is; once none remains the
/// bound is the finite sum, counted by the caller from that moment.
pub(crate) fn successor_bound(below_nanos: u64, inf_live: u32) -> Option<Duration> {
    if inf_live > 0 {
        return None;
    }
    let sum = below_nanos.saturating_add(saturating_nanos(ABORT_SETTLE));
    (sum != u64::MAX).then(|| Duration::from_nanos(sum))
}

/// The earliest instant a wait that can give the Unresponsive verdict for
/// an occupant (a lost runner's end, a closed cell, a subtree, a sweep's
/// leftover) may give it: no verdict before the abort that could end the
/// occupant has had the settle time to do so, within a ceiling that keeps
/// the wait bounded.
///
/// `budget_at` is the wait's own deadline. `aborted_at` is the first abort
/// recorded against the occupant (for a subtree, the latest such record
/// among its actors, the end of an actor that was owed an abort counted as
/// one), and `owed` says that a live occupant is owed an abort that was not
/// issued yet. The floor is the later of `budget_at` and `aborted_at` plus
/// [`ABORT_SETTLE`]; while an abort is owed it is at least `budget_at` plus
/// `cap`, the wait's own budget counted once more from its deadline, which
/// keeps the wait finite when the owed abort never comes. The floor never
/// passes `budget_at` plus `cap` plus [`ABORT_SETTLE`]: an abort issued up
/// to the cap keeps its whole settle time, since cutting it would give the
/// verdict before the abort could end the occupant, and the ceiling acts
/// exactly when the latest abort or owed end is recorded past `budget_at`
/// plus `cap`, which takes the reaper or the cascade below the occupant
/// running a whole budget late (a reaper whose runtime does not run it, a
/// host stall, user code holding the runtime's threads). So a wait held for
/// an owed abort ends at most `cap` plus [`ABORT_SETTLE`] past `budget_at`.
/// With nothing owed and nothing recorded the floor is `budget_at` exactly.
/// Every sum saturates.
///
/// A supervisor child whose orderly stop has no bound (`Shutdown::Infinity`)
/// has no reaper entry, so nothing is owed for it, and its wait has no
/// deadline at all; a worker under `Shutdown::Infinity` is killed with an
/// entry like every other worker, so its wait follows its abort. Past the
/// ceiling, a verdict can precede the abort of an occupant whose reaper is
/// that late, and the later levels of a cascade below an actor that ended
/// or was aborted near the cap get less than their settle time: the ceiling
/// is what keeps the wait bounded.
pub(crate) fn verdict_floor(
    budget_at: Instant,
    cap: Duration,
    aborted_at: Option<Instant>,
    owed: bool,
) -> Instant {
    let settled = aborted_at.map_or(budget_at, |at| {
        budget_at.max(deadline_after(at, ABORT_SETTLE))
    });
    let held = if owed {
        settled.max(deadline_after(budget_at, cap))
    } else {
        settled
    };
    held.min(deadline_after(deadline_after(budget_at, cap), ABORT_SETTLE))
}

#[cfg(test)]
mod subtree_wait_tests {
    use super::*;
    use crate::actor::runtime::FAR_FUTURE;

    /// The floor never ends a wait before its budget deadline, keeps the
    /// settle time of an abort issued up to the cap, and stops at the cap
    /// plus the settle time for an abort issued past it, whether or not an
    /// abort is still owed.
    #[test]
    fn verdict_floor_stops_at_the_cap_plus_the_settle_time() {
        let base = Instant::now();
        let ms = Duration::from_millis;
        let cap = ms(400);
        let ceiling = base + cap + ABORT_SETTLE;
        for owed in [false, true] {
            // An abort exactly at the cap keeps its whole settle time.
            assert_eq!(verdict_floor(base, cap, Some(base + cap), owed), ceiling);
            // One past the cap, by a millisecond or by seconds, stops at the
            // ceiling.
            assert_eq!(
                verdict_floor(base, cap, Some(base + cap + ms(1)), owed),
                ceiling
            );
            assert_eq!(
                verdict_floor(base, cap, Some(base + ms(5_000)), owed),
                ceiling
            );
            // An abort inside the cap: its settle time, or the cap while an
            // abort is still owed.
            let inside = if owed {
                base + cap
            } else {
                base + ms(250) + ABORT_SETTLE
            };
            assert_eq!(verdict_floor(base, cap, Some(base + ms(250)), owed), inside);
            // Never before the budget deadline, whatever was recorded.
            let before = base.checked_sub(ms(900)).unwrap_or(base);
            for at in [None, Some(before), Some(base), Some(base + ms(1))] {
                assert!(verdict_floor(base, cap, at, owed) >= base, "at {at:?}");
            }
            // A cap at the far-future limit saturates and still stands.
            assert!(verdict_floor(base, Duration::MAX, Some(base + ms(5_000)), owed) >= base);
        }
        // Owed with nothing recorded: the cap; nothing owed, nothing
        // recorded: the deadline exactly.
        assert_eq!(verdict_floor(base, cap, None, true), base + cap);
        assert_eq!(verdict_floor(base, cap, None, false), base);
        // A zero cap: an abort at the deadline keeps its settle time, and a
        // later one stops at the same ceiling.
        assert_eq!(
            verdict_floor(base, Duration::ZERO, Some(base), true),
            base + ABORT_SETTLE
        );
        assert_eq!(
            verdict_floor(base, Duration::ZERO, Some(base + ms(1)), false),
            base + ABORT_SETTLE
        );
    }

    const ALL: [bool; 2] = [false, true];

    #[test]
    fn successor_wait_step_over_its_domain_rows() {
        for gone in ALL {
            for order in ALL {
                for expired in ALL {
                    let want = if order {
                        WaitAction::Cancel
                    } else if gone {
                        WaitAction::Proceed
                    } else if expired {
                        WaitAction::Escalate
                    } else {
                        WaitAction::Keep
                    };
                    assert_eq!(
                        successor_wait_step(gone, order, expired),
                        want,
                        "gone {gone} order {order} expired {expired}"
                    );
                }
            }
        }
    }

    #[test]
    fn successor_bound_over_its_domain_rows() {
        let settle = saturating_nanos(ABORT_SETTLE);
        let small = 7_000_000;
        // No unbounded supervisor child below: the budget plus the settle
        // time, unbounded only where the sum saturates.
        assert_eq!(
            successor_bound(0, 0),
            Some(Duration::from_nanos(settle)),
            "no budget"
        );
        assert_eq!(
            successor_bound(small, 0),
            Some(Duration::from_nanos(small + settle))
        );
        assert_eq!(successor_bound(u64::MAX, 0), None);
        assert_eq!(successor_bound(u64::MAX - settle, 0), None);
        assert_eq!(
            successor_bound(u64::MAX - settle - 1, 0),
            Some(Duration::from_nanos(u64::MAX - 1))
        );
        // One or more unbounded supervisor children alive: unbounded whatever
        // the budget.
        for live in [1, 2, u32::MAX] {
            for below in [0, small, u64::MAX, u64::MAX - 1] {
                assert_eq!(
                    successor_bound(below, live),
                    None,
                    "below {below} live {live}"
                );
            }
        }
    }

    #[test]
    fn own_budget_follows_the_child_s_kill_grace() {
        let settle = ABORT_SETTLE;
        let hundred = Duration::from_millis(120);
        let long = Duration::from_millis(1500);
        // A worker's budget is its grace plus the settle time, whatever its
        // own shutdown policy.
        for shutdown in [
            Shutdown::Kill,
            Shutdown::Timeout(Duration::from_secs(5)),
            Shutdown::Infinity,
        ] {
            assert_eq!(own_budget(false, shutdown, Duration::ZERO), Some(settle));
            assert_eq!(own_budget(false, shutdown, hundred), Some(hundred + settle));
            assert_eq!(own_budget(false, shutdown, long), Some(long + settle));
            assert_eq!(own_budget(false, shutdown, Duration::MAX), None);
            assert_eq!(own_fin(false, shutdown, hundred), hundred + settle);
            assert_eq!(own_fin(false, shutdown, Duration::MAX), Duration::MAX);
        }
        // A grace just beyond a u64 of nanoseconds is unbounded, not small.
        let beyond = Duration::new(18_446_744_073, 709_551_621);
        assert_eq!(own_budget(false, Shutdown::Kill, beyond), None);
        assert_eq!(own_fin(false, Shutdown::Kill, beyond), Duration::MAX);
        // A grace of 30 years fits and stays finite.
        assert_eq!(
            own_budget(false, Shutdown::Kill, FAR_FUTURE),
            Some(FAR_FUTURE + settle)
        );
    }

    #[test]
    fn a_supervisor_child_s_budget_is_its_window_then_its_kill_grace_and_the_settle_time() {
        let settle = ABORT_SETTLE;
        let window = Duration::from_secs(3);
        let default = Duration::from_millis(120);
        let long = Duration::from_millis(1500);
        // Under a timeout: the window, the grace and the settle time.
        let timeout = Shutdown::Timeout(window);
        for grace in [Duration::ZERO, default, long] {
            assert_eq!(
                own_budget(true, timeout, grace),
                Some(window + grace + settle),
                "grace {grace:?}"
            );
            assert_eq!(own_fin(true, timeout, grace), window + grace + settle);
        }
        // A zero window adds nothing, and so does `Shutdown::Kill`.
        assert_eq!(
            own_budget(true, Shutdown::Timeout(Duration::ZERO), default),
            Some(default + settle)
        );
        for grace in [Duration::ZERO, default, long] {
            assert_eq!(
                own_budget(true, Shutdown::Kill, grace),
                Some(grace + settle),
                "grace {grace:?}"
            );
            assert_eq!(own_fin(true, Shutdown::Kill, grace), grace + settle);
        }
        // A grace or a window that saturates is unbounded, never small.
        assert_eq!(own_budget(true, timeout, Duration::MAX), None);
        assert_eq!(own_fin(true, timeout, Duration::MAX), Duration::MAX);
        assert_eq!(own_budget(true, Shutdown::Kill, Duration::MAX), None);
        assert_eq!(
            own_budget(true, Shutdown::Timeout(Duration::MAX), default),
            None
        );
        assert_eq!(
            own_fin(true, Shutdown::Timeout(Duration::MAX), default),
            Duration::MAX
        );
        // A window of 30 years fits and stays finite.
        assert_eq!(
            own_budget(true, Shutdown::Timeout(FAR_FUTURE), default),
            Some(FAR_FUTURE + default + settle)
        );
    }

    #[test]
    fn own_fin_is_finite_and_own_budget_unbounded_for_an_infinity_supervisor_child() {
        let settle = ABORT_SETTLE;
        let default = Duration::from_millis(120);
        let long = Duration::from_millis(1500);
        for grace in [Duration::ZERO, default, long] {
            assert_eq!(
                own_budget(true, Shutdown::Infinity, grace),
                None,
                "grace {grace:?}"
            );
            assert_eq!(
                own_fin(true, Shutdown::Infinity, grace),
                grace + settle,
                "grace {grace:?}"
            );
        }
        // A grace that saturates leaves no finite stand-in either.
        assert_eq!(own_budget(true, Shutdown::Infinity, Duration::MAX), None);
        assert_eq!(
            own_fin(true, Shutdown::Infinity, Duration::MAX),
            Duration::MAX
        );
        // A worker under `Infinity` is bounded by its grace.
        assert_eq!(
            own_budget(false, Shutdown::Infinity, default),
            Some(default + settle)
        );
    }

    #[test]
    fn fired_stop_keeps_the_abort_of_a_stored_kill() {
        let grace = Duration::from_millis(120);
        for window in [None, Some(Duration::ZERO), Some(Duration::from_secs(3))] {
            let orderly = InheritedStop::Orderly { window, grace };
            // Nothing stored: the orderly stop stands.
            assert_eq!(fired_stop(orderly, false), orderly);
            // A stored `Kill`: the worker's entry, at the child's own grace.
            assert_eq!(fired_stop(orderly, true), InheritedStop::Kill { grace });
        }
        let worker = InheritedStop::Kill { grace };
        assert_eq!(fired_stop(worker, false), worker);
        assert_eq!(fired_stop(worker, true), worker);
    }

    #[test]
    fn inherited_stop_over_its_rows() {
        let default = Duration::from_millis(120);
        let window = Duration::from_secs(3);
        for grace in [
            Duration::ZERO,
            default,
            Duration::from_millis(1500),
            Duration::MAX,
        ] {
            // A worker is killed, whatever its own shutdown.
            for shutdown in [
                Shutdown::Kill,
                Shutdown::Timeout(window),
                Shutdown::Infinity,
            ] {
                assert_eq!(
                    inherited_stop(false, shutdown, grace),
                    InheritedStop::Kill { grace }
                );
            }
            // A supervisor child stops in order within its window.
            assert_eq!(
                inherited_stop(true, Shutdown::Timeout(window), grace),
                InheritedStop::Orderly {
                    window: Some(window),
                    grace
                }
            );
            assert_eq!(
                inherited_stop(true, Shutdown::Timeout(Duration::MAX), grace),
                InheritedStop::Orderly {
                    window: Some(Duration::MAX),
                    grace
                }
            );
            assert_eq!(
                inherited_stop(true, Shutdown::Kill, grace),
                InheritedStop::Orderly {
                    window: Some(Duration::ZERO),
                    grace
                }
            );
            assert_eq!(
                inherited_stop(true, Shutdown::Infinity, grace),
                InheritedStop::Orderly {
                    window: None,
                    grace
                }
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Group stops, the teardown and the verdict
// ---------------------------------------------------------------------------

/// One child as a group evaluation reads it.
#[derive(Clone, Debug)]
pub(crate) struct GroupMember {
    pub id: ActorId,
    pub life: ChildLifecycle,
    pub restart_type: RestartType,
}

/// A member's part in a group restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemberAction {
    /// The member has an occupant: the group waits for its end, and the
    /// occupant is stopped in its turn in the stop queue.
    pub held: bool,
    /// The occupant is a restart attempt: the attempt that has not spawned
    /// its actor yet is cancelled at once, by a flag that an attempt past its
    /// check ignores. The stop of one that spawned is ordered in its turn.
    pub cancel: bool,
    /// The group starts the member again once every committed member ended.
    pub restarts: bool,
}

/// The incarnation a ledger entry names for a barrier: the occupant's, or
/// for a down spec the one it last held.
fn commit_token(life: ChildLifecycle) -> u64 {
    match life {
        ChildLifecycle::Running(n)
        | ChildLifecycle::Stopping { incarnation: n, .. }
        | ChildLifecycle::Down { incarnation: n, .. } => n,
        ChildLifecycle::Restarting { next, .. } => next,
    }
}

/// The members a group restart covers and what it does about each, in start
/// order. `children` is the whole registry in start order and `failed` the
/// child whose end triggered the group.
///
/// `OneForAll` covers every child, `RestForOne` the failed child and those
/// started after it. Every member with an occupant is committed to the
/// group's barrier at once, whatever state its stop is in: a running one, one
/// that is already stopping (by a `stop_child` or `terminate_child`
/// committed before this evaluation), and one whose restart attempt is in
/// flight. The evaluation stops none of them and asks none of them: the stop
/// queue does that, one member at a time, from the member's state when its
/// turn comes ([`stop_queue_next`]). A down member has nothing to wait for.
/// Every member but a `Temporary` one is started again, one whose manual stop
/// is in progress included.
pub(crate) fn group_commit(
    strategy: RestartStrategy,
    children: &[GroupMember],
    failed: &ActorId,
) -> Vec<(ActorId, u64, MemberAction)> {
    let start = match strategy {
        RestartStrategy::RestForOne => children
            .iter()
            .position(|child| &child.id == failed)
            .unwrap_or(0),
        _ => 0,
    };
    children[start..]
        .iter()
        .map(|child| {
            let held = &child.id != failed && !matches!(child.life, ChildLifecycle::Down { .. });
            (
                child.id.clone(),
                commit_token(child.life),
                MemberAction {
                    held,
                    cancel: held && matches!(child.life, ChildLifecycle::Restarting { .. }),
                    restarts: child.restart_type != RestartType::Temporary,
                },
            )
        })
        .collect()
}

/// What a retry of a group restart does: the commit, as a group evaluation
/// builds it, and the members the chain starts afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RetryPlan {
    /// Every member the retry covers, with what it does about its stop.
    pub commit: Vec<(ActorId, u64, MemberAction)>,
    /// The members the chain starts again, in start order.
    pub restart_order: Vec<ActorId>,
}

/// The plan of a retry after the chain's own start failed: the group is laid
/// out again from the registry as it is now. `registry` is every child in
/// start order, `overrides` the children a `terminate_child` released while
/// this group restart pended, and `failed` the member whose start failed.
///
/// The scope and the stops are the evaluation's, from one call of
/// [`group_commit`]: `OneForAll` covers every child, a child started since
/// the first evaluation included, and `RestForOne` the failed member and
/// every child after it. A member released by `terminate_child` stays down
/// through every retry: it is not started again and its end stays removed.
pub(crate) fn retry_plan(
    strategy: RestartStrategy,
    registry: &[GroupMember],
    overrides: &[ActorId],
    failed: &ActorId,
) -> RetryPlan {
    let mut commit = group_commit(strategy, registry, failed);
    for (id, _, action) in commit.iter_mut() {
        if overrides.contains(id) {
            action.restarts = false;
        }
    }
    let restart_order = commit
        .iter()
        .filter(|(_, _, action)| action.restarts)
        .map(|(id, _, _)| id.clone())
        .collect();
    RetryPlan {
        commit,
        restart_order,
    }
}

/// What the stop queue reads of its head when its turn is decided.
#[derive(Clone, Copy, Debug)]
pub(crate) struct QueueHead {
    /// The barrier still holds an entry for the head's awaited incarnation.
    pub awaited: bool,
    /// That incarnation carries the Unresponsive verdict.
    pub verdict: bool,
    /// The queue reached the head earlier and waits for its end.
    pub signalled: bool,
    /// A completed manual stop classified the head's end already: a snapshot
    /// of it waits for its report.
    pub handled: bool,
    /// The head's ledger entry; `None` when its spec is gone.
    pub life: Option<ChildLifecycle>,
    /// The occupant's slot already holds a stop order.
    pub has_order: bool,
    /// The occupant's policy is `Infinity`, its lane holds a request its
    /// `pre_stop` may still veto, and its task runs.
    pub ask_possible: bool,
    /// The one-more-ask record of the occupant's slot is set.
    pub reasked: bool,
    /// The restart attempt's incarnation finished its initialization: its
    /// status left `Initializing`.
    pub attempt_past_init: bool,
    /// The restart attempt spawned its actor: it published its lane. An
    /// attempt that has not was cancelled when the group was planned, and
    /// ends with nothing built.
    pub attempt_spawned: bool,
}

/// What the stop queue does with its head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueStep {
    /// The head is settled, or its end was classified by a completed manual
    /// stop: take it off the queue and read the next one. Its barrier entry
    /// stays until its report is delivered.
    Pop,
    /// The queue reached the head earlier: wait for its end.
    Wait,
    /// The head holds no incarnation the queue can stop (its spec is gone,
    /// or the key moved past the awaited incarnation, or the member is down
    /// with its report undelivered): nothing is ordered; mark it reached and
    /// wait for the report.
    Await,
    /// Order the running head's stop through its runner.
    Order,
    /// Order the head's spawned restart attempt to stop, with one more
    /// request when `ask`.
    OrderAttempt { ask: bool },
    /// The head's restart attempt has not published its lane: write its stop
    /// order unraised. An attempt that has not passed its check reads the
    /// order there and cancels with nothing built, exactly as the plan's flag
    /// makes it; one that passed it reads the order when its ladder starts
    /// and stops under the member's policy.
    OrderUnpublished,
    /// A stop of the head is in progress, from a caller's manual stop or an
    /// earlier call that was abandoned: join it, never giving it a second
    /// ladder, with one more request when `ask`.
    Join { ask: bool },
}

/// The step of a group's stop queue, which stops its members one at a time in
/// reverse start order and moves on only when the head is settled. Each step
/// reads the head's state when its turn comes, never the state the group was
/// evaluated in.
///
/// A head whose awaited end was delivered, whose Unresponsive verdict was
/// given, or whose end a completed manual stop classified before the queue
/// reached it is passed over. A head the queue reached waits for its end. A
/// running head with no order is ordered; one that already holds an order a
/// caller wrote is joined, as is one whose stop is in progress, each with the
/// one more request to a vetoing `Infinity` member that nobody asked since.
/// A restart attempt is ordered to stop at its turn, whatever its stage: one
/// with a published lane has the first signal raised, with one more request
/// when its stop was ordered already and its incarnation finished its
/// initialization; one without is given the order unraised.
pub(crate) fn stop_queue_next(head: &QueueHead) -> QueueStep {
    if !head.awaited || head.verdict || (!head.signalled && head.handled) {
        return QueueStep::Pop;
    }
    if head.signalled {
        return QueueStep::Wait;
    }
    let ask = head.ask_possible && !head.reasked;
    match head.life {
        Some(ChildLifecycle::Running(_)) if !head.has_order => QueueStep::Order,
        Some(ChildLifecycle::Running(_) | ChildLifecycle::Stopping { .. }) => {
            QueueStep::Join { ask }
        }
        Some(ChildLifecycle::Restarting { .. }) if !head.attempt_spawned => {
            QueueStep::OrderUnpublished
        }
        Some(ChildLifecycle::Restarting { .. }) => QueueStep::OrderAttempt {
            ask: head.has_order && head.attempt_past_init && ask,
        },
        Some(ChildLifecycle::Down { .. }) | None => QueueStep::Await,
    }
}

/// Whether the queue has reached its head after `step`, given whether it had
/// before: a head that was passed over leaves the next one unreached, a
/// waiting head stays as it was, and any other step reaches the head.
pub(crate) fn queue_head_reached_after(step: QueueStep, before: bool) -> bool {
    match step {
        QueueStep::Pop => false,
        QueueStep::Wait => before,
        _ => true,
    }
}

/// What a group does with the end of one of its members.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemberEnd {
    /// The member is started again in its slot; its end is reported as the
    /// group's restart of it.
    RestartInSlot,
    /// The member is not started again: its spec is removed and its end is
    /// reported removed.
    PruneAndRemove,
}

/// The one decision about a member's end in a group, wherever the end is
/// delivered: a `Temporary` member is removed with its spec, any other
/// member is started again in its slot.
pub(crate) fn group_member_end(restart_type: RestartType) -> MemberEnd {
    if restart_type == RestartType::Temporary {
        MemberEnd::PruneAndRemove
    } else {
        MemberEnd::RestartInSlot
    }
}

/// One child as the supervisor's teardown reads it when its turn comes.
#[derive(Clone, Debug)]
pub(crate) struct WalkChild {
    /// The child's ledger entry; `None` when no spec exists.
    pub life: Option<ChildLifecycle>,
    /// The occupant's slot already holds a stop order.
    pub has_order: bool,
    /// The occupant's policy is `Infinity`, its lane holds a request its
    /// `pre_stop` may still veto, and its task runs.
    pub ask_possible: bool,
    /// The one-more-ask record of the occupant's slot is set; a teardown asks
    /// whatever it says, so no decision reads it.
    #[allow(dead_code)]
    pub reasked: bool,
    /// The restart attempt's incarnation finished its initialization: its
    /// status left `Initializing`.
    pub attempt_past_init: bool,
}

/// What the teardown does about a child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TeardownAction {
    /// Nothing to stop.
    Skip,
    /// Order the stop through the occupant's runner, then wait for its end.
    Order,
    /// A stop of the occupant is in progress: wait for its end, never start
    /// a second ladder, with one more request when `ask`.
    Join { ask: bool },
    /// Order the restart attempt to stop (it cancels before its factory, or
    /// stops its starting incarnation), then wait for its end, with one more
    /// request when `ask`.
    OrderAttempt { ask: bool },
}

/// The teardown's action for each child, in the order the teardown visits
/// them: newest first. A child whose stop is in progress under `Infinity`
/// with a vetoable request is asked once more by this stop, whatever an
/// earlier stop asked.
///
/// A pool is asked about every child in one call and acts on all of them at
/// once; every other strategy is asked about one child when its turn comes,
/// after the child started later settled. The action for a child is the same
/// in both.
pub(crate) fn teardown_walk(
    _strategy: RestartStrategy,
    children: &[(ActorId, WalkChild)],
) -> Vec<(ActorId, TeardownAction)> {
    children
        .iter()
        .rev()
        .map(|(id, child)| {
            let action = match child.life {
                Some(ChildLifecycle::Running(_)) if !child.has_order => TeardownAction::Order,
                Some(ChildLifecycle::Running(_) | ChildLifecycle::Stopping { .. }) => {
                    TeardownAction::Join {
                        ask: child.ask_possible,
                    }
                }
                Some(ChildLifecycle::Restarting { .. }) => TeardownAction::OrderAttempt {
                    ask: child.has_order && child.attempt_past_init && child.ask_possible,
                },
                Some(ChildLifecycle::Down { .. }) | None => TeardownAction::Skip,
            };
            (id.clone(), action)
        })
        .collect()
}

/// Where the report of an owed evaluation is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReportPlace {
    /// Deferred behind the pending group restart.
    Queued,
    /// In the worklist of a completing group.
    Worklist,
    /// Not yet drained: still in the death plane.
    DeathPlane,
}

/// What a group evaluation does with a member's owed evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Absorption {
    /// Not absorbed: the evaluation stays owed.
    Keep,
    /// The report is removed from where it waits, the member is recorded
    /// down with nothing owed, and the report is delivered once as the
    /// group's restart of the member.
    Deliver,
    /// The member is recorded down with a snapshot of its end, which its
    /// report delivers once when it drains.
    Snapshot,
}

/// Whether a group evaluation absorbs a member's owed evaluation. A member
/// of the group whose ledger entry is down and owes its evaluation is
/// absorbed wherever its report is: a member's end belongs to its group, so
/// nothing is charged for it.
pub(crate) fn absorbable(
    place: ReportPlace,
    in_plan: bool,
    life: Option<ChildLifecycle>,
) -> Absorption {
    let owed = matches!(
        life,
        Some(ChildLifecycle::Down {
            pending: DownPending::Evaluate,
            ..
        })
    );
    if !in_plan || !owed {
        return Absorption::Keep;
    }
    match place {
        ReportPlace::Queued | ReportPlace::Worklist => Absorption::Deliver,
        ReportPlace::DeathPlane => Absorption::Snapshot,
    }
}

/// The member a pending group restart cannot wait for: `entries` are
/// the incarnations the group waits for, in reverse start order, each with
/// whether it carries the Unresponsive verdict. When there is at least one
/// and every one carries it, the group can make progress only through
/// unresponsive members, and the first of them is the one the supervisor's
/// failure names.
pub(crate) fn barrier_blocked_only_by_verdicts(entries: &[(ActorId, bool)]) -> Option<ActorId> {
    if entries.is_empty() || !entries.iter().all(|(_, verdict)| *verdict) {
        return None;
    }
    entries.first().map(|(id, _)| id.clone())
}

/// Whether the end of a child stopped by its supervisor's teardown is an
/// orderly one: the supervisor's own request, a `Graceful` end for a child
/// that is not `Permanent`, and the `Kill` of a child whose policy is
/// `Shutdown::Kill`. Any other end is logged.
pub(crate) fn teardown_end_is_orderly(
    reason: &StopReason,
    restart_type: RestartType,
    shutdown: Shutdown,
) -> bool {
    match reason {
        StopReason::ParentRequest => true,
        StopReason::Graceful => restart_type != RestartType::Permanent,
        StopReason::Kill => matches!(shutdown, Shutdown::Kill),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// The stop lane
// ---------------------------------------------------------------------------

/// How the stop lane answers a request: what it reports to the caller,
/// whether the request replaces the stored reason, and whether the lane's
/// generation moves (waking the actor to observe it).
///
/// The lane refuses everything once it is sealed, everything but `Kill` once
/// it is open to `Kill` only, and every request when `receiver_gone` (the
/// actor's task has dropped the receiving half). A request of strictly
/// lower severity than the stored one is superseded and changes nothing.
/// Anything else is recorded and bumps the generation; a request of strictly
/// higher severity, or of equal severity and at least the stored rank
/// (`ParentRequest` over `Graceful`, `Failure` over `Cancelled`, the newer of
/// two equals), also replaces the stored reason.
pub(crate) fn lane_accept(
    state: &LaneState,
    new: &StopReason,
    receiver_gone: bool,
) -> (RaiseResult, bool, bool) {
    let kill_only_refuses = state.kill_only && !matches!(new, StopReason::Kill);
    if receiver_gone || state.sealed || kill_only_refuses {
        return (RaiseResult::Refused, false, false);
    }
    let Some(stored) = state.reason.as_ref() else {
        return (RaiseResult::Recorded, true, true);
    };
    let (new_severity, stored_severity) = (StopSeverity::of(new), StopSeverity::of(stored));
    if new_severity < stored_severity {
        return (RaiseResult::Superseded, false, false);
    }
    let store =
        new_severity > stored_severity || StopSeverity::rank(new) >= StopSeverity::rank(stored);
    (RaiseResult::Recorded, store, true)
}

/// What an actor's start does when it reads its lane between two hooks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InitStep {
    /// Run the next hook.
    Continue,
    /// A `Kill` is pending: no further hook begins.
    KillNow,
}

/// The read of the lane before `pre_start` and after it returns: a pending
/// `Kill` skips every remaining start hook.
pub(crate) fn init_kill_check(lane_reason: Option<&StopReason>) -> InitStep {
    match lane_reason {
        Some(StopReason::Kill) => InitStep::KillNow,
        _ => InitStep::Continue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CommitEffect::{
        Answer, Classify, Dismiss, Join, LeaveRestartOrder, RaiseFirst, Reask, ReaskOnce,
        RecordOverride, RewriteSnapshot, WriteOrder,
    };

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn ladder_step_over_every_step_and_event() {
        let t0 = Instant::now();
        let grace = MS(120);
        let abort = ABORT_SETTLE;
        let now = at(t0, 1000);
        let order = |policy, issued_at, initial_raised| LadderEvent::Order {
            policy,
            issued_at,
            initial_raised,
        };

        // Idle + an order whose first signal was not raised: the ladder
        // raises it and the clock starts now.
        assert_eq!(
            ladder_step(
                LadderStep::Idle,
                order(Shutdown::Timeout(MS(200)), t0, false),
                now,
                grace
            ),
            (
                LadderStep::Graceful {
                    deadline: Some(at(now, 200))
                },
                LadderAction::Raise(Signal::ParentRequest)
            )
        );
        assert_eq!(
            ladder_step(
                LadderStep::Idle,
                order(Shutdown::Kill, t0, false),
                now,
                grace
            ),
            (
                LadderStep::KillGrace {
                    deadline: at(now, 120)
                },
                LadderAction::Raise(Signal::Kill)
            )
        );
        assert_eq!(
            ladder_step(
                LadderStep::Idle,
                order(Shutdown::Infinity, t0, false),
                now,
                grace
            ),
            (
                LadderStep::Graceful { deadline: None },
                LadderAction::Raise(Signal::ParentRequest)
            )
        );
        // Idle + an order whose first signal was raised: the clock started
        // when the order was issued, and nothing is raised again.
        assert_eq!(
            ladder_step(
                LadderStep::Idle,
                order(Shutdown::Timeout(MS(200)), t0, true),
                now,
                grace
            ),
            (
                LadderStep::Graceful {
                    deadline: Some(at(t0, 200))
                },
                LadderAction::None
            )
        );
        assert_eq!(
            ladder_step(
                LadderStep::Idle,
                order(Shutdown::Kill, t0, true),
                now,
                grace
            ),
            (
                LadderStep::KillGrace {
                    deadline: at(t0, 120)
                },
                LadderAction::None
            )
        );
        assert_eq!(
            ladder_step(
                LadderStep::Idle,
                order(Shutdown::Infinity, t0, true),
                now,
                grace
            ),
            (LadderStep::Graceful { deadline: None }, LadderAction::None)
        );
        // A zero grace puts the kill grace's deadline at the raise itself.
        assert_eq!(
            ladder_step(
                LadderStep::Idle,
                order(Shutdown::Kill, t0, false),
                now,
                Duration::ZERO
            ),
            (
                LadderStep::KillGrace { deadline: now },
                LadderAction::Raise(Signal::Kill)
            )
        );

        // The graceful window ending raises Kill and starts the kill grace.
        assert_eq!(
            ladder_step(
                LadderStep::Graceful {
                    deadline: Some(at(t0, 200))
                },
                LadderEvent::DeadlineReached,
                now,
                grace
            ),
            (
                LadderStep::KillGrace {
                    deadline: at(now, 120)
                },
                LadderAction::Raise(Signal::Kill)
            )
        );
        // An unbounded graceful wait never acts.
        assert_eq!(
            ladder_step(
                LadderStep::Graceful { deadline: None },
                LadderEvent::DeadlineReached,
                now,
                grace
            ),
            (LadderStep::Graceful { deadline: None }, LadderAction::None)
        );
        // The kill grace ending aborts and starts the abort wait.
        assert_eq!(
            ladder_step(
                LadderStep::KillGrace {
                    deadline: at(t0, 120)
                },
                LadderEvent::DeadlineReached,
                now,
                grace
            ),
            (
                LadderStep::AbortGrace {
                    deadline: now + abort
                },
                LadderAction::Abort
            )
        );
        // The abort wait ending writes the verdict and leaves nothing armed.
        assert_eq!(
            ladder_step(
                LadderStep::AbortGrace {
                    deadline: at(t0, 220)
                },
                LadderEvent::DeadlineReached,
                now,
                grace
            ),
            (LadderStep::Spent, LadderAction::WriteVerdict)
        );
        assert_eq!(
            ladder_step(LadderStep::Spent, LadderEvent::DeadlineReached, now, grace),
            (LadderStep::Spent, LadderAction::None)
        );
        // An armed ladder keeps its first order: a later one changes nothing.
        for armed in [
            LadderStep::Graceful {
                deadline: Some(at(t0, 5)),
            },
            LadderStep::Graceful { deadline: None },
            LadderStep::KillGrace {
                deadline: at(t0, 5),
            },
            LadderStep::AbortGrace {
                deadline: at(t0, 5),
            },
            LadderStep::Spent,
        ] {
            assert_eq!(
                ladder_step(armed, order(Shutdown::Kill, t0, false), now, grace),
                (armed, LadderAction::None)
            );
        }
        // Idle ignores a deadline.
        assert_eq!(
            ladder_step(LadderStep::Idle, LadderEvent::DeadlineReached, now, grace),
            (LadderStep::Idle, LadderAction::None)
        );
        // The grace given is the grace waited, whatever its size; an
        // unbounded one is capped to a deadline a timer accepts.
        for grace in [Duration::ZERO, MS(120), MS(1500), Duration::MAX] {
            let (next, _) = ladder_step(
                LadderStep::Idle,
                order(Shutdown::Kill, t0, false),
                now,
                grace,
            );
            assert_eq!(
                next,
                LadderStep::KillGrace {
                    deadline: now.checked_add(grace.min(FAR_FUTURE)).unwrap()
                }
            );
        }
    }

    #[test]
    fn ladder_deadlines_read_as_the_step_states() {
        let t0 = Instant::now();
        assert_eq!(LadderStep::Idle.deadline(), None);
        assert_eq!(LadderStep::Spent.deadline(), None);
        assert_eq!(LadderStep::Graceful { deadline: None }.deadline(), None);
        assert_eq!(
            LadderStep::Graceful { deadline: Some(t0) }.deadline(),
            Some(t0)
        );
        assert_eq!(LadderStep::KillGrace { deadline: t0 }.deadline(), Some(t0));
        assert_eq!(LadderStep::AbortGrace { deadline: t0 }.deadline(), Some(t0));
    }

    fn running(n: u64) -> Option<ChildLifecycle> {
        Some(ChildLifecycle::Running(n))
    }

    fn stopping(n: u64, kind: ManualStop) -> Option<ChildLifecycle> {
        Some(ChildLifecycle::Stopping {
            incarnation: n,
            kind,
        })
    }

    fn down(n: u64, pending: DownPending) -> Option<ChildLifecycle> {
        Some(ChildLifecycle::Down {
            incarnation: n,
            pending,
        })
    }

    fn restarting() -> Option<ChildLifecycle> {
        Some(ChildLifecycle::Restarting {
            incarnation: 1,
            next: 2,
        })
    }

    fn commit(to: Option<ChildLifecycle>, effects: Vec<CommitEffect>) -> Commit {
        Commit { to, effects }
    }

    /// The facts of a call on a `Permanent` child with no pending group, a
    /// `Timeout` policy and no request to veto.
    fn facts(
        life: Option<ChildLifecycle>,
        task: TaskState,
        membership: GroupMembership,
    ) -> ManualFacts {
        ManualFacts {
            life,
            task,
            membership,
            restart_type: RestartType::Permanent,
            pool: false,
            restarted: false,
            in_keep_scope: false,
            policy: Shutdown::Timeout(MS(100)),
            lane_vetoable: false,
            reasked: false,
            has_order: false,
        }
    }

    impl ManualFacts {
        fn ordered(mut self) -> Self {
            self.has_order = true;
            self
        }

        fn restart(mut self, restart_type: RestartType) -> Self {
            self.restart_type = restart_type;
            self
        }

        fn restarted(mut self) -> Self {
            self.restarted = true;
            self
        }

        fn keep(mut self) -> Self {
            self.in_keep_scope = true;
            self
        }

        fn asking(mut self) -> Self {
            self.policy = Shutdown::Infinity;
            self.lane_vetoable = true;
            self
        }

        fn asked(mut self) -> Self {
            self.reasked = true;
            self
        }
    }

    #[test]
    fn manual_commit_over_every_state_and_membership() {
        use GroupMembership::{Awaited, Queued, Released};
        use ManualStop::{Bounce, Terminate};
        use RestartType::{Temporary, Transient};
        use TaskState::{Ended, Live, RunnerLost};
        let none = GroupMembership::None;
        let ok = || vec![Answer(ManualReply::Ok)];
        let gone = || vec![Answer(ManualReply::ChildNotFound)];
        let busy = || vec![Answer(ManualReply::ChildRestarting)];
        let absorbed = || vec![Answer(ManualReply::Absorbed)];

        #[allow(clippy::type_complexity)]
        let rows: Vec<(ManualFacts, ManualStop, Commit)> = vec![
            // No spec under the key.
            (facts(None, Live, none), Bounce, commit(None, gone())),
            (facts(None, Live, none), Terminate, commit(None, gone())),
            (facts(None, Live, Queued), Terminate, commit(None, gone())),
            // A live running child: committed in one step.
            (
                facts(running(7), Live, none),
                Bounce,
                commit(stopping(7, Bounce), vec![RaiseFirst, WriteOrder, Join]),
            ),
            (
                facts(running(7), Live, none),
                Terminate,
                commit(stopping(7, Terminate), vec![RaiseFirst, WriteOrder, Join]),
            ),
            // The runner failed, the task still runs: the order is written
            // for whoever can read it, nothing is raised.
            (
                facts(running(7), RunnerLost, none),
                Bounce,
                commit(stopping(7, Bounce), vec![WriteOrder, Join]),
            ),
            (
                facts(running(7), RunnerLost, none),
                Terminate,
                commit(stopping(7, Terminate), vec![WriteOrder, Join]),
            ),
            // The task ended before the call: classified at once.
            (
                facts(running(7), Ended, none),
                Bounce,
                commit(None, vec![Classify, Answer(ManualReply::Ok)]),
            ),
            (
                facts(running(7), Ended, none),
                Terminate,
                commit(None, vec![Classify, Answer(ManualReply::Ok)]),
            ),
            // A stop in progress is joined.
            (
                facts(stopping(7, Bounce), Live, none),
                Bounce,
                commit(None, vec![Join]),
            ),
            (
                facts(stopping(7, Terminate), Live, none),
                Terminate,
                commit(None, vec![Join]),
            ),
            (
                facts(stopping(7, Terminate), Live, none),
                Bounce,
                commit(None, vec![Join]),
            ),
            // A terminate joining a bounce upgrades it.
            (
                facts(stopping(7, Bounce), Live, none),
                Terminate,
                commit(stopping(7, Terminate), vec![Join]),
            ),
            (
                facts(stopping(7, Bounce), Ended, none),
                Terminate,
                commit(stopping(7, Terminate), vec![Join]),
            ),
            // One more request when the child is a vetoing Infinity child,
            // by each stop that joins it.
            (
                facts(stopping(7, Bounce), Live, none).asking(),
                Bounce,
                commit(None, vec![Reask, Join]),
            ),
            (
                facts(stopping(7, Terminate), Live, none).asking(),
                Terminate,
                commit(None, vec![Reask, Join]),
            ),
            (
                facts(stopping(7, Terminate), Live, none).asking(),
                Bounce,
                commit(None, vec![Reask, Join]),
            ),
            (
                facts(stopping(7, Terminate), RunnerLost, none).asking(),
                Terminate,
                commit(None, vec![Reask, Join]),
            ),
            (
                facts(stopping(7, Bounce), Live, none).asking(),
                Terminate,
                commit(stopping(7, Terminate), vec![Reask, Join]),
            ),
            // No request to veto, or not Infinity: nothing to ask again.
            (
                facts(stopping(7, Bounce), Live, none),
                Bounce,
                commit(None, vec![Join]),
            ),
            (
                {
                    let mut f = facts(stopping(7, Bounce), Live, none).asking();
                    f.lane_vetoable = false;
                    f
                },
                Bounce,
                commit(None, vec![Join]),
            ),
            (
                {
                    let mut f = facts(stopping(7, Terminate), Live, none).asking();
                    f.policy = Shutdown::Kill;
                    f
                },
                Terminate,
                commit(None, vec![Join]),
            ),
            // An ended task being stopped completes at once: no ask.
            (
                facts(stopping(7, Bounce), Ended, none).asking(),
                Bounce,
                commit(None, vec![Join]),
            ),
            (
                facts(stopping(7, Terminate), Ended, none),
                Terminate,
                commit(None, vec![Join]),
            ),
            // A restart in flight is refused by both, whatever the type.
            (
                facts(restarting(), Live, none),
                Bounce,
                commit(None, busy()),
            ),
            (
                facts(restarting(), Live, none),
                Terminate,
                commit(None, busy()),
            ),
            (
                facts(restarting(), Live, none).restart(Transient),
                Bounce,
                commit(None, busy()),
            ),
            // A child that is down.
            (
                facts(down(7, DownPending::None), Ended, none),
                Bounce,
                commit(None, ok()),
            ),
            (
                facts(down(7, DownPending::Snapshot), Ended, none),
                Bounce,
                commit(None, ok()),
            ),
            (
                facts(down(7, DownPending::None), Ended, none),
                Terminate,
                commit(None, ok()),
            ),
            (
                facts(down(7, DownPending::Snapshot), Ended, none),
                Terminate,
                commit(None, ok()),
            ),
            // A death that owes its restart decision: a bounce is refused,
            // a terminate dismisses the decision.
            (
                facts(down(7, DownPending::Evaluate), Ended, none),
                Bounce,
                commit(None, busy()),
            ),
            (
                facts(down(7, DownPending::Evaluate), Ended, none),
                Terminate,
                commit(None, vec![Dismiss, Answer(ManualReply::Ok)]),
            ),
            // A `Permanent` member of a pending group restart that still has
            // to restart it absorbs a bounce, whatever its state.
            (
                facts(running(7), Live, Awaited { overridden: false }),
                Bounce,
                commit(None, absorbed()),
            ),
            (
                facts(stopping(7, Bounce), Live, Awaited { overridden: false }),
                Bounce,
                commit(None, absorbed()),
            ),
            (
                facts(restarting(), Live, Awaited { overridden: false }),
                Bounce,
                commit(None, absorbed()),
            ),
            (
                facts(down(7, DownPending::None), Ended, Queued),
                Bounce,
                commit(None, absorbed()),
            ),
            (
                facts(down(7, DownPending::Evaluate), Ended, Queued),
                Bounce,
                commit(None, absorbed()),
            ),
            // A member the group will not restart, or one a chain already
            // restarted, is refused.
            (
                facts(running(7), Live, Awaited { overridden: true }),
                Bounce,
                commit(None, busy()),
            ),
            (
                facts(stopping(7, Terminate), Live, Awaited { overridden: true }),
                Bounce,
                commit(None, busy()),
            ),
            (
                facts(down(7, DownPending::Snapshot), Ended, Released),
                Bounce,
                commit(None, busy()),
            ),
            (
                facts(stopping(7, Terminate), Live, Released),
                Bounce,
                commit(None, busy()),
            ),
            (
                facts(running(7), Live, none).restarted(),
                Bounce,
                commit(None, busy()),
            ),
            (
                facts(down(7, DownPending::None), Ended, none).restarted(),
                Bounce,
                commit(None, busy()),
            ),
            // The chain front's own attempt in flight is refused, whatever
            // its type.
            (
                facts(restarting(), Live, Queued),
                Bounce,
                commit(None, busy()),
            ),
            (
                facts(restarting(), Live, Queued).restart(Transient),
                Bounce,
                commit(None, busy()),
            ),
            // A `Permanent` key outside the group, or a spare one in its
            // scope, keeps the ordinary bounce.
            (
                facts(running(7), Live, none).keep(),
                Bounce,
                commit(stopping(7, Bounce), vec![RaiseFirst, WriteOrder, Join]),
            ),
            // A `Transient` or `Temporary` member follows its own policy: the
            // call is its `terminate_child`.
            (
                facts(running(7), Live, Awaited { overridden: false }).restart(Transient),
                Bounce,
                commit(
                    None,
                    vec![
                        RecordOverride,
                        LeaveRestartOrder,
                        RaiseFirst,
                        WriteOrder,
                        Join,
                    ],
                ),
            ),
            (
                facts(running(7), Live, Awaited { overridden: false })
                    .restart(Transient)
                    .ordered(),
                Bounce,
                commit(None, vec![RecordOverride, LeaveRestartOrder, Join]),
            ),
            (
                facts(stopping(7, Bounce), Live, Awaited { overridden: false }).restart(Transient),
                Bounce,
                commit(
                    stopping(7, Terminate),
                    vec![RecordOverride, LeaveRestartOrder, Join],
                ),
            ),
            (
                facts(restarting(), Live, Awaited { overridden: false }).restart(Temporary),
                Bounce,
                commit(None, busy()),
            ),
            (
                facts(down(7, DownPending::None), Ended, Queued).restart(Transient),
                Bounce,
                commit(
                    None,
                    vec![RecordOverride, LeaveRestartOrder, Answer(ManualReply::Ok)],
                ),
            ),
            (
                facts(stopping(7, Terminate), Live, Released).restart(Transient),
                Bounce,
                commit(None, vec![Join]),
            ),
            (
                facts(down(7, DownPending::None), Ended, Released).restart(Temporary),
                Bounce,
                commit(None, ok()),
            ),
            (
                facts(running(7), Live, none).restart(Transient).restarted(),
                Bounce,
                commit(stopping(7, Terminate), vec![RaiseFirst, WriteOrder, Join]),
            ),
            // A `Transient` or `Temporary` key of no membership in the
            // group's scope is stopped as its terminate, and kept down.
            (
                facts(running(7), Live, none).restart(Transient).keep(),
                Bounce,
                commit(
                    stopping(7, Terminate),
                    vec![RaiseFirst, WriteOrder, RecordOverride, Join],
                ),
            ),
            (
                facts(running(7), Ended, none).restart(Transient).keep(),
                Bounce,
                commit(
                    None,
                    vec![Classify, RecordOverride, Answer(ManualReply::Ok)],
                ),
            ),
            (
                facts(running(7), Ended, none).restart(Temporary).keep(),
                Bounce,
                commit(None, vec![Classify, Answer(ManualReply::Ok)]),
            ),
            (
                facts(down(7, DownPending::Evaluate), Ended, none)
                    .restart(Transient)
                    .keep(),
                Bounce,
                commit(None, vec![Dismiss, RecordOverride, Answer(ManualReply::Ok)]),
            ),
            // A terminate of a member whose end the group awaits records
            // that it stays down, and waits. A running member the stop queue
            // has not reached gets its order from the call; a member whose
            // stop is in progress is joined, with the one more ask.
            (
                facts(running(7), Live, Awaited { overridden: false }),
                Terminate,
                commit(
                    None,
                    vec![
                        RecordOverride,
                        LeaveRestartOrder,
                        RaiseFirst,
                        WriteOrder,
                        Join,
                    ],
                ),
            ),
            (
                facts(running(7), Live, Awaited { overridden: false }).asking(),
                Terminate,
                commit(
                    None,
                    vec![
                        RecordOverride,
                        LeaveRestartOrder,
                        RaiseFirst,
                        WriteOrder,
                        Join,
                    ],
                ),
            ),
            (
                facts(running(7), RunnerLost, Awaited { overridden: false }),
                Terminate,
                commit(
                    None,
                    vec![RecordOverride, LeaveRestartOrder, WriteOrder, Join],
                ),
            ),
            (
                facts(running(7), Live, Awaited { overridden: false }).ordered(),
                Terminate,
                commit(None, vec![RecordOverride, LeaveRestartOrder, Join]),
            ),
            (
                facts(running(7), Live, Awaited { overridden: false })
                    .ordered()
                    .asking(),
                Terminate,
                commit(
                    None,
                    vec![RecordOverride, LeaveRestartOrder, ReaskOnce, Join],
                ),
            ),
            (
                facts(running(7), Live, Awaited { overridden: true })
                    .ordered()
                    .asking(),
                Terminate,
                commit(
                    None,
                    vec![RecordOverride, LeaveRestartOrder, ReaskOnce, Join],
                ),
            ),
            (
                facts(running(7), RunnerLost, Awaited { overridden: false })
                    .ordered()
                    .asking(),
                Terminate,
                commit(
                    None,
                    vec![RecordOverride, LeaveRestartOrder, ReaskOnce, Join],
                ),
            ),
            (
                facts(running(7), Live, Awaited { overridden: false })
                    .ordered()
                    .asking()
                    .asked(),
                Terminate,
                commit(None, vec![RecordOverride, LeaveRestartOrder, Join]),
            ),
            (
                facts(running(7), Ended, Awaited { overridden: false }).asking(),
                Terminate,
                commit(None, vec![RecordOverride, LeaveRestartOrder, Join]),
            ),
            (
                facts(running(7), Ended, Awaited { overridden: false })
                    .ordered()
                    .asking(),
                Terminate,
                commit(None, vec![RecordOverride, LeaveRestartOrder, Join]),
            ),
            (
                facts(stopping(7, Terminate), Live, Awaited { overridden: false }),
                Terminate,
                commit(None, vec![RecordOverride, LeaveRestartOrder, Join]),
            ),
            (
                facts(stopping(7, Terminate), Live, Awaited { overridden: false }).asking(),
                Terminate,
                commit(
                    None,
                    vec![RecordOverride, LeaveRestartOrder, ReaskOnce, Join],
                ),
            ),
            (
                facts(stopping(7, Terminate), Live, Awaited { overridden: false })
                    .asking()
                    .asked(),
                Terminate,
                commit(None, vec![RecordOverride, LeaveRestartOrder, Join]),
            ),
            (
                facts(stopping(7, Bounce), Live, Awaited { overridden: false }),
                Terminate,
                commit(
                    stopping(7, Terminate),
                    vec![RecordOverride, LeaveRestartOrder, Join],
                ),
            ),
            (
                facts(stopping(7, Bounce), Live, Awaited { overridden: false }).asking(),
                Terminate,
                commit(
                    stopping(7, Terminate),
                    vec![RecordOverride, LeaveRestartOrder, ReaskOnce, Join],
                ),
            ),
            (
                facts(stopping(7, Bounce), Ended, Awaited { overridden: false }).asking(),
                Terminate,
                commit(
                    stopping(7, Terminate),
                    vec![RecordOverride, LeaveRestartOrder, Join],
                ),
            ),
            (
                facts(restarting(), Live, Awaited { overridden: false }),
                Terminate,
                commit(None, busy()),
            ),
            // A terminate of a member waiting in the restart set takes it
            // out of the set and dismisses what it owed.
            (
                facts(down(7, DownPending::None), Ended, Queued),
                Terminate,
                commit(
                    None,
                    vec![RecordOverride, LeaveRestartOrder, Answer(ManualReply::Ok)],
                ),
            ),
            (
                facts(down(7, DownPending::Snapshot), Ended, Queued),
                Terminate,
                commit(
                    None,
                    vec![
                        RecordOverride,
                        LeaveRestartOrder,
                        RewriteSnapshot,
                        Answer(ManualReply::Ok),
                    ],
                ),
            ),
            (
                facts(down(7, DownPending::Evaluate), Ended, Queued),
                Terminate,
                commit(
                    None,
                    vec![
                        Dismiss,
                        RecordOverride,
                        LeaveRestartOrder,
                        Answer(ManualReply::Ok),
                    ],
                ),
            ),
            (
                facts(restarting(), Live, Queued),
                Terminate,
                commit(None, busy()),
            ),
            // A terminate of a member an earlier terminate released.
            (
                facts(down(7, DownPending::Snapshot), Ended, Released),
                Terminate,
                commit(None, ok()),
            ),
            (
                facts(down(7, DownPending::None), Ended, Released),
                Terminate,
                commit(None, ok()),
            ),
            (
                facts(stopping(7, Terminate), Live, Released),
                Terminate,
                commit(None, vec![Join]),
            ),
            (
                facts(stopping(7, Terminate), Live, Released).asking(),
                Terminate,
                commit(None, vec![ReaskOnce, Join]),
            ),
            (
                facts(stopping(7, Terminate), Live, Released)
                    .asking()
                    .asked(),
                Terminate,
                commit(None, vec![Join]),
            ),
            (
                facts(stopping(7, Terminate), Ended, Released).asking(),
                Terminate,
                commit(None, vec![Join]),
            ),
            (
                facts(running(7), Live, Released),
                Terminate,
                commit(None, busy()),
            ),
            // A terminate of a child no group holds, while a group restart
            // is pending and covers its key, records that the group leaves
            // it down.
            (
                facts(running(7), Live, none).keep(),
                Terminate,
                commit(
                    stopping(7, Terminate),
                    vec![RaiseFirst, WriteOrder, RecordOverride, Join],
                ),
            ),
            (
                facts(running(7), RunnerLost, none).keep(),
                Terminate,
                commit(
                    stopping(7, Terminate),
                    vec![WriteOrder, RecordOverride, Join],
                ),
            ),
            (
                facts(running(7), Ended, none).keep(),
                Terminate,
                commit(
                    None,
                    vec![Classify, RecordOverride, Answer(ManualReply::Ok)],
                ),
            ),
            // A pruned spec records nothing.
            (
                facts(running(7), Ended, none).restart(Temporary).keep(),
                Terminate,
                commit(None, vec![Classify, Answer(ManualReply::Ok)]),
            ),
            (
                facts(down(7, DownPending::Evaluate), Ended, none)
                    .restart(Temporary)
                    .keep(),
                Terminate,
                commit(None, vec![Dismiss, Answer(ManualReply::Ok)]),
            ),
            // A spec that stays after the call records, whatever it is.
            (
                facts(down(7, DownPending::None), Ended, none).keep(),
                Terminate,
                commit(None, vec![RecordOverride, Answer(ManualReply::Ok)]),
            ),
            (
                facts(down(7, DownPending::Snapshot), Ended, none)
                    .restart(Temporary)
                    .keep(),
                Terminate,
                commit(None, vec![RecordOverride, Answer(ManualReply::Ok)]),
            ),
            (
                facts(down(7, DownPending::Evaluate), Ended, none).keep(),
                Terminate,
                commit(None, vec![Dismiss, RecordOverride, Answer(ManualReply::Ok)]),
            ),
            // The KEEP joins give the one more ask once, shared with the
            // group.
            (
                facts(stopping(7, Terminate), Live, none).keep().asking(),
                Terminate,
                commit(None, vec![ReaskOnce, RecordOverride, Join]),
            ),
            (
                facts(stopping(7, Bounce), Live, none).keep().asking(),
                Terminate,
                commit(
                    stopping(7, Terminate),
                    vec![ReaskOnce, RecordOverride, Join],
                ),
            ),
            (
                facts(stopping(7, Terminate), Live, none)
                    .keep()
                    .asking()
                    .asked(),
                Terminate,
                commit(None, vec![RecordOverride, Join]),
            ),
            (
                facts(stopping(7, Terminate), Ended, none).keep().asking(),
                Terminate,
                commit(None, vec![RecordOverride, Join]),
            ),
        ];
        for (facts, kind, want) in rows {
            let got = manual_commit(kind, &facts);
            assert_eq!(got, want, "{kind:?} {facts:?}");
        }
    }

    #[test]
    fn manual_commit_never_raises_without_an_order_or_orders_without_raising_twice() {
        let lives = [
            None,
            running(1),
            stopping(1, ManualStop::Bounce),
            stopping(1, ManualStop::Terminate),
            restarting(),
            down(1, DownPending::None),
            down(1, DownPending::Snapshot),
            down(1, DownPending::Evaluate),
        ];
        let tasks = [TaskState::Live, TaskState::RunnerLost, TaskState::Ended];
        let kinds = [ManualStop::Bounce, ManualStop::Terminate];
        let memberships = [
            GroupMembership::None,
            GroupMembership::Awaited { overridden: false },
            GroupMembership::Awaited { overridden: true },
            GroupMembership::Queued,
            GroupMembership::Released,
        ];
        let policies = [
            Shutdown::Kill,
            Shutdown::Timeout(MS(50)),
            Shutdown::Infinity,
        ];
        let types = [
            RestartType::Permanent,
            RestartType::Transient,
            RestartType::Temporary,
        ];
        for life in lives {
            for task in tasks {
                for kind in kinds {
                    for membership in memberships {
                        for policy in policies {
                            for restart_type in types {
                                for flags in 0..32u8 {
                                    let f = ManualFacts {
                                        life,
                                        task,
                                        membership,
                                        restart_type,
                                        pool: false,
                                        restarted: flags & 1 != 0,
                                        in_keep_scope: flags & 2 != 0,
                                        policy,
                                        lane_vetoable: flags & 4 != 0,
                                        reasked: flags & 8 != 0,
                                        has_order: flags & 16 != 0,
                                    };
                                    let got = manual_commit(kind, &f);
                                    let raises = got.effects.contains(&RaiseFirst);
                                    let orders = got.effects.contains(&WriteOrder);
                                    assert!(
                                        !raises || orders,
                                        "a first signal is always followed by the order: {got:?}"
                                    );
                                    if f.membership != GroupMembership::None {
                                        // Only a running member the stop queue has
                                        // not reached, with no order, is given one.
                                        let unreached =
                                            matches!(f.membership, GroupMembership::Awaited { .. })
                                                && matches!(
                                                    f.life,
                                                    Some(ChildLifecycle::Running(_))
                                                )
                                                && !f.has_order
                                                && f.task != TaskState::Ended;
                                        assert!(
                                            unreached || (!raises && !orders),
                                            "a member of a pending group is never given a \
                                             second first signal or order by a manual call: \
                                             {got:?} for {f:?}"
                                        );
                                    }
                                    let asks = got.effects.contains(&Reask)
                                        || got.effects.contains(&ReaskOnce);
                                    if asks {
                                        assert_eq!(policy, Shutdown::Infinity);
                                        assert!(f.lane_vetoable);
                                        assert_ne!(task, TaskState::Ended);
                                        assert!(got.effects.contains(&Join));
                                    }
                                    if got.effects.contains(&ReaskOnce) {
                                        assert!(!f.reasked, "the shared ask is given once");
                                    }
                                    // An answer ends the call: nothing follows it.
                                    if let Some(at) = got
                                        .effects
                                        .iter()
                                        .position(|e| matches!(e, CommitEffect::Answer(_)))
                                    {
                                        assert_eq!(at, got.effects.len() - 1, "{got:?}");
                                    }
                                    // The call goes on or answers, and a call
                                    // that waits joins last.
                                    if got.effects.contains(&Join) {
                                        assert_eq!(got.effects.last(), Some(&Join), "{got:?}");
                                    }
                                    // The reply a call gives without waiting is the
                                    // commit's bare answer, or `Ok`.
                                    let op = match kind {
                                        ManualStop::Bounce => ManualOp::StopChild,
                                        ManualStop::Terminate => ManualOp::TerminateChild,
                                    };
                                    let reply = manual_answer(op, &f);
                                    match got.effects.as_slice() {
                                        [Answer(r)] => assert_eq!(reply, *r),
                                        _ => assert_eq!(reply, ManualReply::Ok),
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn terminate_child_succeeds_for_every_known_child_but_an_attempt_in_flight() {
        let lives = [
            running(1),
            stopping(1, ManualStop::Bounce),
            stopping(1, ManualStop::Terminate),
            down(1, DownPending::None),
            down(1, DownPending::Snapshot),
            down(1, DownPending::Evaluate),
        ];
        for life in lives {
            for task in [TaskState::Live, TaskState::RunnerLost, TaskState::Ended] {
                for membership in [
                    GroupMembership::None,
                    GroupMembership::Awaited { overridden: false },
                    GroupMembership::Queued,
                    GroupMembership::Released,
                ] {
                    let f = facts(life, task, membership);
                    let reachable = match (membership, life) {
                        (GroupMembership::None, _) => true,
                        (GroupMembership::Awaited { .. }, Some(ChildLifecycle::Down { .. })) => {
                            false
                        }
                        (GroupMembership::Awaited { .. }, _) => true,
                        (GroupMembership::Queued, Some(ChildLifecycle::Down { .. })) => true,
                        (GroupMembership::Queued, _) => false,
                        (
                            GroupMembership::Released,
                            Some(
                                ChildLifecycle::Down { .. }
                                | ChildLifecycle::Stopping {
                                    kind: ManualStop::Terminate,
                                    ..
                                },
                            ),
                        ) => true,
                        (GroupMembership::Released, _) => false,
                    };
                    if !reachable {
                        continue;
                    }
                    assert_ne!(
                        manual_answer(ManualOp::TerminateChild, &f),
                        ManualReply::ChildRestarting,
                        "{f:?}"
                    );
                }
            }
        }
        let f = facts(restarting(), TaskState::Live, GroupMembership::None);
        assert_eq!(
            manual_answer(ManualOp::TerminateChild, &f),
            ManualReply::ChildRestarting
        );
    }

    // - the child ledger -------------------------------------------------------

    use crate::actor::supervision::{slot_pair, ChildSpec, ChildState};
    use crate::types::ActorId;

    const TOKENS: [u64; 4] = [0, 1, 2, u64::MAX];

    /// Every ledger entry over the token set: all four shapes, both stop
    /// kinds and all three pending values.
    fn every_life() -> Vec<ChildLifecycle> {
        let mut lives = Vec::new();
        for n in TOKENS {
            lives.push(ChildLifecycle::Running(n));
            for kind in [ManualStop::Bounce, ManualStop::Terminate] {
                lives.push(ChildLifecycle::Stopping {
                    incarnation: n,
                    kind,
                });
            }
            for next in TOKENS {
                lives.push(ChildLifecycle::Restarting {
                    incarnation: n,
                    next,
                });
            }
            for pending in [
                DownPending::None,
                DownPending::Snapshot,
                DownPending::Evaluate,
            ] {
                lives.push(ChildLifecycle::Down {
                    incarnation: n,
                    pending,
                });
            }
        }
        lives
    }

    fn run(n: u64) -> ChildLifecycle {
        ChildLifecycle::Running(n)
    }

    fn stop(n: u64, kind: ManualStop) -> ChildLifecycle {
        ChildLifecycle::Stopping {
            incarnation: n,
            kind,
        }
    }

    fn restart(n: u64, next: u64) -> ChildLifecycle {
        ChildLifecycle::Restarting {
            incarnation: n,
            next,
        }
    }

    fn dn(n: u64, pending: DownPending) -> ChildLifecycle {
        ChildLifecycle::Down {
            incarnation: n,
            pending,
        }
    }

    /// The four shapes in declaration order.
    fn shape(life: &ChildLifecycle) -> usize {
        match life {
            ChildLifecycle::Running(_) => 0,
            ChildLifecycle::Stopping { .. } => 1,
            ChildLifecycle::Restarting { .. } => 2,
            ChildLifecycle::Down { .. } => 3,
        }
    }

    /// The default-closed table by shape: rows are the "from" shape and
    /// columns the "to" shape, in the order Running, Stopping, Restarting,
    /// Down. A stopping entry moves to a stopping one only by the upgrade of
    /// a bounce to a terminate, which has its own pair.
    const SHAPE_TABLE: [[bool; 4]; 4] = [
        // from Running:    Running false, Stopping true,  Restarting false, Down true
        [false, true, false, true],
        // from Stopping:   Running false, Stopping false, Restarting false, Down true
        [false, false, false, true],
        // from Restarting: Running true,  Stopping false, Restarting false, Down true
        [true, false, false, true],
        // from Down:       Running false, Stopping false, Restarting true,  Down true
        [false, false, true, true],
    ];

    /// An entry of `from_shape` and one of `to_shape` whose payloads match the
    /// move between them (the same incarnation, the drawn token for an
    /// attempt, a later token for a new incarnation).
    fn matching_pair(from_shape: usize, to_shape: usize) -> (ChildLifecycle, ChildLifecycle) {
        let from = match from_shape {
            0 => run(1),
            1 => stop(1, ManualStop::Bounce),
            2 => restart(1, 2),
            _ => dn(1, DownPending::None),
        };
        let to = match (from_shape, to_shape) {
            (2, 0) => run(2),
            (2, 3) => dn(2, DownPending::None),
            (3, 2) => restart(1, 2),
            (_, 0) => run(1),
            (_, 1) => stop(1, ManualStop::Bounce),
            (_, 2) => restart(1, 2),
            _ => dn(1, DownPending::None),
        };
        (from, to)
    }

    #[test]
    fn the_shape_table_of_the_ledger_is_closed() {
        for (from_shape, row) in SHAPE_TABLE.iter().enumerate() {
            for (to_shape, &want) in row.iter().enumerate() {
                let (from, to) = matching_pair(from_shape, to_shape);
                assert_eq!(
                    (shape(&from), shape(&to)),
                    (from_shape, to_shape),
                    "the pair has the shapes it names"
                );
                assert_eq!(is_legal(&from, &to), want, "from {from:?} to {to:?}");
            }
        }
        // The upgrade of a bounce in progress to a terminate.
        assert!(is_legal(
            &stop(5, ManualStop::Bounce),
            &stop(5, ManualStop::Terminate)
        ));
    }

    #[test]
    fn each_legal_row_rejects_its_mismatched_field_twin() {
        // One negative per legal row: the same shapes, another incarnation.
        let negatives = [
            (run(5), stop(6, ManualStop::Bounce)),
            (run(5), dn(6, DownPending::None)),
            (stop(5, ManualStop::Bounce), dn(6, DownPending::None)),
            (stop(5, ManualStop::Bounce), stop(6, ManualStop::Terminate)),
            (restart(1, 2), run(1)),
            (restart(1, 2), dn(1, DownPending::None)),
            (dn(9, DownPending::None), restart(9, 9)),
            (dn(9, DownPending::None), restart(8, 10)),
            (dn(9, DownPending::None), dn(8, DownPending::None)),
            // The upgrade goes one way only.
            (stop(5, ManualStop::Terminate), stop(5, ManualStop::Bounce)),
        ];
        for (from, to) in negatives {
            assert!(!is_legal(&from, &to), "from {from:?} to {to:?}");
        }
    }

    /// The eight rows of the table, each written as the shapes it joins and
    /// the equalities between their payloads, independently of `is_legal`.
    fn the_table_allows(from: &ChildLifecycle, to: &ChildLifecycle) -> bool {
        use ChildLifecycle::{Down, Restarting, Running, Stopping};
        let token = |life: &ChildLifecycle| match life {
            Running(n) => *n,
            Stopping { incarnation, .. } | Down { incarnation, .. } => *incarnation,
            Restarting { incarnation, .. } => *incarnation,
        };
        match (shape(from), shape(to)) {
            // Running to stopping, running to down, stopping to down: the
            // same incarnation.
            (0, 1) | (0, 3) | (1, 3) => token(from) == token(to),
            // A bounce becomes a terminate: the same incarnation, nothing else
            // between stopping entries.
            (1, 1) => {
                token(from) == token(to)
                    && matches!(
                        (from, to),
                        (
                            Stopping {
                                kind: ManualStop::Bounce,
                                ..
                            },
                            Stopping {
                                kind: ManualStop::Terminate,
                                ..
                            }
                        )
                    )
            }
            // An attempt is adopted, or ends, as the incarnation it was
            // drawn for.
            (2, 0) => matches!(from, Restarting { next, .. } if *next == token(to)),
            (2, 3) => matches!(from, Restarting { next, .. } if *next == token(to)),
            // A new incarnation begins from a down entry, with a later token.
            (3, 2) => {
                matches!(to, Restarting { incarnation, next } if *incarnation == token(from) && *next > token(from))
            }
            // A down entry records what its end still owes.
            (3, 3) => token(from) == token(to),
            _ => false,
        }
    }

    #[test]
    fn legal_rows_accept_exactly_their_matching_fields() {
        let lives = every_life();
        let mut legal = 0;
        for from in &lives {
            for to in &lives {
                let want = the_table_allows(from, to);
                assert_eq!(is_legal(from, to), want, "from {from:?} to {to:?}");
                legal += usize::from(want);
            }
        }
        assert!(legal > 0);
        // No arrow starts a new incarnation from a running or stopping entry.
        for n in TOKENS {
            for next in [3, u64::MAX] {
                if next > n {
                    assert!(!is_legal(&run(n), &restart(n, next)));
                    for kind in [ManualStop::Bounce, ManualStop::Terminate] {
                        assert!(!is_legal(&stop(n, kind), &restart(n, next)));
                    }
                }
            }
        }
    }

    #[test]
    fn rejected_transitions_leave_the_state_untouched() {
        let lives = every_life();
        let mut rejected = 0;
        for from in &lives {
            for to in &lives {
                if is_legal(from, to) {
                    continue;
                }
                let (ctl, _io) = slot_pair(Default::default());
                let mut child = ChildState {
                    id: ActorId::from("rejected"),
                    name: None,
                    spec: ChildSpec {
                        restart_type: RestartType::Permanent,
                        shutdown: Shutdown::default(),
                        start_timeout: None,
                        kill_grace: crate::actor::supervision::DEFAULT_KILL_GRACE,
                        is_supervisor: false,
                    },
                    ctl,
                    lifecycle: *from,
                    origin: Origin::Initial,
                    predecessor: None,
                };
                assert_eq!(child.transition(*to), Err(*to), "from {from:?} to {to:?}");
                assert_eq!(child.lifecycle, *from);
                rejected += 1;
            }
        }
        assert!(rejected > 0);
        // One legal pair, to confirm an accepted move applies.
        let (ctl, _io) = slot_pair(Default::default());
        let mut child = ChildState {
            id: ActorId::from("accepted"),
            name: None,
            spec: ChildSpec {
                restart_type: RestartType::Permanent,
                shutdown: Shutdown::default(),
                start_timeout: None,
                kill_grace: crate::actor::supervision::DEFAULT_KILL_GRACE,
                is_supervisor: false,
            },
            ctl,
            lifecycle: run(1),
            origin: Origin::Initial,
            predecessor: None,
        };
        let target = stop(1, ManualStop::Bounce);
        assert_eq!(child.transition(target), Ok(()));
        assert_eq!(child.lifecycle, target);
    }

    #[test]
    fn report_class_over_every_state() {
        use DownPending::{Evaluate, None as Nothing, Snapshot};
        use ReportClass::{Occupant, OwedEvaluation, Stale};
        let rows = [
            // The occupant of a running or stopping entry.
            (run(4), 4, Occupant),
            (run(4), 3, Stale),
            (stop(4, ManualStop::Bounce), 4, Occupant),
            (stop(4, ManualStop::Terminate), 4, Occupant),
            (stop(4, ManualStop::Bounce), 5, Stale),
            // An attempt in flight: its own token is the occupant, the
            // retired one and any other are stale.
            (restart(4, 9), 9, Occupant),
            (restart(4, 9), 4, Stale),
            (restart(4, 9), 7, Stale),
            // A down entry has no occupant; only an owed end is evaluated.
            (dn(4, Nothing), 4, Stale),
            (dn(4, Snapshot), 4, Stale),
            (dn(4, Evaluate), 4, OwedEvaluation),
            (dn(4, Evaluate), 5, Stale),
            (dn(4, Nothing), 5, Stale),
        ];
        for (life, n, want) in rows {
            assert_eq!(report_class(life, n), want, "{life:?} {n}");
        }
    }

    #[test]
    fn child_info_flags_over_every_state() {
        use DownPending::{Evaluate, None as Nothing, Snapshot};
        // (life, fate_present, in_restart_set) -> (is_alive, restart_pending)
        let rows = [
            (run(1), false, false, (true, false)),
            (run(1), true, false, (false, true)),
            (run(1), false, true, (true, false)),
            (stop(1, ManualStop::Bounce), false, false, (true, false)),
            (stop(1, ManualStop::Terminate), false, true, (true, false)),
            // A stopping child whose task ended reads neither alive nor
            // pending until its death is handled.
            (stop(1, ManualStop::Bounce), true, false, (false, false)),
            (restart(1, 2), false, false, (false, true)),
            (restart(1, 2), true, true, (false, true)),
            (dn(1, Nothing), false, false, (false, false)),
            (dn(1, Nothing), false, true, (false, true)),
            (dn(1, Snapshot), false, false, (false, false)),
            (dn(1, Snapshot), false, true, (false, true)),
            (dn(1, Evaluate), false, false, (false, true)),
            (dn(1, Evaluate), true, true, (false, true)),
        ];
        for (life, fate_present, in_restart_set, want) in rows {
            assert_eq!(
                child_info_flags(life, fate_present, in_restart_set),
                want,
                "{life:?} fate {fate_present} restart set {in_restart_set}"
            );
        }
    }

    #[test]
    fn manual_answer_over_every_state() {
        use DownPending::{Evaluate, None as Nothing, Snapshot};
        use GroupMembership::{Awaited, Queued, Released};
        use ManualOp::{DeleteChild, RestartChild};
        use ManualReply::{ChildNotFound, ChildRestarting, ChildRunning, Ok as Done};
        use TaskState::{Ended, Live, RunnerLost};
        let none = GroupMembership::None;
        let rows = [
            // No spec.
            (RestartChild, None, Live, none, ChildNotFound),
            (DeleteChild, None, Live, none, ChildNotFound),
            // A running child: its task decides.
            (RestartChild, Some(run(1)), Live, none, ChildRunning),
            (DeleteChild, Some(run(1)), Live, none, ChildRunning),
            (RestartChild, Some(run(1)), RunnerLost, none, ChildRunning),
            (DeleteChild, Some(run(1)), RunnerLost, none, ChildRunning),
            (RestartChild, Some(run(1)), Ended, none, ChildRestarting),
            (DeleteChild, Some(run(1)), Ended, none, ChildRestarting),
            // A child being stopped, whether or not its task ended: a death
            // is never classified to answer.
            (
                RestartChild,
                Some(stop(1, ManualStop::Bounce)),
                Live,
                none,
                ChildRestarting,
            ),
            (
                DeleteChild,
                Some(stop(1, ManualStop::Terminate)),
                Ended,
                none,
                ChildRestarting,
            ),
            // An attempt in flight.
            (
                RestartChild,
                Some(restart(1, 2)),
                Live,
                none,
                ChildRestarting,
            ),
            (
                DeleteChild,
                Some(restart(1, 2)),
                Live,
                none,
                ChildRestarting,
            ),
            // A down child: only an owed decision keeps the call out.
            (RestartChild, Some(dn(1, Nothing)), Ended, none, Done),
            (DeleteChild, Some(dn(1, Nothing)), Ended, none, Done),
            (RestartChild, Some(dn(1, Snapshot)), Ended, none, Done),
            (DeleteChild, Some(dn(1, Snapshot)), Ended, none, Done),
            (
                RestartChild,
                Some(dn(1, Evaluate)),
                Ended,
                none,
                ChildRestarting,
            ),
            (
                DeleteChild,
                Some(dn(1, Evaluate)),
                Ended,
                none,
                ChildRestarting,
            ),
            // A member of a pending group is the group's until it completes.
            (
                RestartChild,
                Some(run(1)),
                Live,
                Awaited { overridden: false },
                ChildRestarting,
            ),
            (
                DeleteChild,
                Some(run(1)),
                Live,
                Awaited { overridden: true },
                ChildRestarting,
            ),
            (
                RestartChild,
                Some(dn(1, Nothing)),
                Ended,
                Queued,
                ChildRestarting,
            ),
            (
                DeleteChild,
                Some(dn(1, Nothing)),
                Ended,
                Queued,
                ChildRestarting,
            ),
            // A member a terminate released: restart_child is refused for
            // every state, delete_child is answered by the state.
            (
                RestartChild,
                Some(dn(1, Snapshot)),
                Ended,
                Released,
                ChildRestarting,
            ),
            (
                RestartChild,
                Some(dn(1, Nothing)),
                Ended,
                Released,
                ChildRestarting,
            ),
            (DeleteChild, Some(dn(1, Snapshot)), Ended, Released, Done),
            (DeleteChild, Some(dn(1, Nothing)), Ended, Released, Done),
            (
                DeleteChild,
                Some(stop(1, ManualStop::Terminate)),
                Live,
                Released,
                ChildRestarting,
            ),
            (DeleteChild, Some(run(1)), Live, Released, ChildRunning),
            (RestartChild, Some(run(1)), Live, Released, ChildRestarting),
        ];
        for (op, life, task, membership, want) in rows {
            assert_eq!(
                manual_answer(op, &facts(life, task, membership)),
                want,
                "{op:?} {life:?} {task:?} {membership:?}"
            );
        }
        // A key a chain of the pending group already restarted is refused
        // by `restart_child` while its task runs; `delete_child` answers by
        // the state.
        let restarted = facts(Some(run(1)), Live, none).restarted();
        assert_eq!(
            manual_answer(RestartChild, &restarted),
            ChildRestarting,
            "restart_child of a key a chain restarted"
        );
        assert_eq!(manual_answer(DeleteChild, &restarted), ChildRunning);
        assert_eq!(
            manual_answer(RestartChild, &facts(Some(run(1)), Live, none)),
            ChildRunning
        );
        // `stop_child` and `terminate_child` are answered by their commit.
        let member = facts(Some(run(1)), Live, Awaited { overridden: false });
        assert_eq!(
            manual_answer(ManualOp::StopChild, &member),
            ManualReply::Absorbed
        );
        assert_eq!(manual_answer(ManualOp::TerminateChild, &member), Done);
        assert_eq!(
            manual_answer(ManualOp::StopChild, &facts(None, Live, none)),
            ChildNotFound
        );
    }

    #[test]
    fn chain_front_over_every_state() {
        use FrontAction::{Fresh, Hold, Initiate, StopThenInitiate};
        let rows = [
            (dn(1, DownPending::None), None, Initiate),
            (dn(1, DownPending::Evaluate), Some(1), Initiate),
            (run(7), Some(7), Fresh),
            // An incarnation the chain did not start: another token, or none.
            (run(7), Some(6), StopThenInitiate),
            (run(7), None, StopThenInitiate),
            (stop(7, ManualStop::Bounce), Some(7), Hold),
            (stop(7, ManualStop::Terminate), None, Hold),
            (restart(7, 8), Some(8), Hold),
            (restart(7, 8), None, Hold),
        ];
        for (life, started, want) in rows {
            assert_eq!(chain_front(life, started), want, "{life:?} {started:?}");
        }
    }

    #[test]
    fn classify_manual_over_every_input() {
        use ManualStop::{Bounce, Terminate};
        use RestartType::{Permanent, Temporary, Transient};
        let crash = StopReason::Failure(crate::error::ActorError::user("crash"));
        let outcome = |action, pending, remove, initiate| ManualOutcome {
            action,
            pending,
            remove,
            initiate,
        };
        let removed = Some(SupervisionAction::Removed);
        let restarted = Some(SupervisionAction::RestartInitiated);
        // A bounce that met a crash is the crash: no snapshot, the decision
        // owed, whatever the restart type or the supervisor.
        for restart_type in [Permanent, Transient, Temporary] {
            for pool in [false, true] {
                assert_eq!(
                    classify_manual(Bounce, &crash, restart_type, pool, false),
                    outcome(None, DownPending::Evaluate, false, false),
                    "{restart_type:?} {pool}"
                );
            }
        }
        // A terminate met by a crash is still a terminate.
        assert_eq!(
            classify_manual(Terminate, &crash, Permanent, false, false),
            outcome(removed, DownPending::Snapshot, false, false)
        );
        for reason in [StopReason::Graceful, StopReason::Kill, crash.clone()] {
            // A terminate keeps the child down, a temporary child or a pool
            // child is removed.
            assert_eq!(
                classify_manual(Terminate, &reason, Permanent, false, false),
                outcome(removed, DownPending::Snapshot, false, false)
            );
            assert_eq!(
                classify_manual(Terminate, &reason, Transient, false, false),
                outcome(removed, DownPending::Snapshot, false, false)
            );
            assert_eq!(
                classify_manual(Terminate, &reason, Temporary, false, false),
                outcome(removed, DownPending::None, true, false)
            );
            assert_eq!(
                classify_manual(Terminate, &reason, Permanent, true, false),
                outcome(removed, DownPending::None, true, false)
            );
        }
        for reason in [
            StopReason::Graceful,
            StopReason::Kill,
            StopReason::Cancelled,
        ] {
            // A bounce restarts a permanent child without charge, removes a
            // pool child and a temporary one, and leaves a transient one down.
            assert_eq!(
                classify_manual(Bounce, &reason, Permanent, false, false),
                outcome(restarted, DownPending::Snapshot, false, true)
            );
            assert_eq!(
                classify_manual(Bounce, &reason, Permanent, true, false),
                outcome(removed, DownPending::None, true, false)
            );
            assert_eq!(
                classify_manual(Bounce, &reason, Transient, false, false),
                outcome(removed, DownPending::Snapshot, false, false)
            );
            assert_eq!(
                classify_manual(Bounce, &reason, Temporary, false, false),
                outcome(removed, DownPending::None, true, false)
            );
        }
        // A stop of either kind that ended a member of the pending group's
        // restart set is that group's restart: kept for its report, nothing
        // started, nothing removed, a crash a bounce met excepted.
        for kind in [Bounce, Terminate] {
            for reason in [
                StopReason::Graceful,
                StopReason::Kill,
                StopReason::Cancelled,
            ] {
                for restart_type in [Permanent, Transient, Temporary] {
                    for pool in [false, true] {
                        assert_eq!(
                            classify_manual(kind, &reason, restart_type, pool, true),
                            outcome(restarted, DownPending::Snapshot, false, false),
                            "{kind:?} {reason:?} {restart_type:?} {pool}"
                        );
                    }
                }
            }
        }
        assert_eq!(
            classify_manual(Terminate, &crash, Permanent, false, true),
            outcome(restarted, DownPending::Snapshot, false, false)
        );
        assert_eq!(
            classify_manual(Bounce, &crash, Permanent, false, true),
            outcome(None, DownPending::Evaluate, false, false)
        );
    }

    #[test]
    fn attempt_failure_route_over_every_input() {
        use GroupMembership::{Awaited, Queued, Released};
        let memberships = [
            GroupMembership::None,
            Awaited { overridden: false },
            Awaited { overridden: true },
            Queued,
            Released,
        ];
        let origins = [
            Origin::Initial,
            Origin::Strategy,
            Origin::Bounce,
            Origin::Manual,
            Origin::Chain,
        ];
        for membership in memberships {
            for origin in origins {
                // The chain front's own attempt is tested first.
                assert_eq!(
                    attempt_failure_route(true, membership, origin),
                    Route::ChainRetry
                );
                let want = if matches!(membership, Awaited { .. } | Queued) {
                    // A member the group committed: the group restarts it, a
                    // manual attempt's origin included.
                    Route::Member
                } else if origin == Origin::Manual {
                    Route::Removed
                } else {
                    Route::Evaluate
                };
                assert_eq!(
                    attempt_failure_route(false, membership, origin),
                    want,
                    "{membership:?} {origin:?}"
                );
            }
        }
    }

    #[test]
    fn attempt_step_over_every_stage_and_event() {
        use AttemptAction as A;
        use AttemptLadder as L;
        let ladders = [L::Off, L::Window, L::KillGrace, L::AbortGrace, L::Spent];
        for stage in [
            AttemptStage::Factory,
            AttemptStage::Init,
            AttemptStage::InTransit,
        ] {
            for ladder in ladders {
                for kill_stored in [false, true] {
                    for acked in [false, true] {
                        let view = AttemptView {
                            ladder,
                            kill_stored,
                            acked,
                        };
                        let case = format!("{stage:?} {view:?}");
                        let step = |event| attempt_step(stage, event, view);
                        let init = stage == AttemptStage::Init;
                        let factory = stage == AttemptStage::Factory;
                        // Before the factory, an order cancels and no order
                        // spawns.
                        assert_eq!(
                            step(AttemptEvent::Begin { ordered: true }),
                            if factory { A::Cancel } else { A::None },
                            "{case}"
                        );
                        assert_eq!(
                            step(AttemptEvent::Begin { ordered: false }),
                            if factory { A::Spawn } else { A::None },
                            "{case}"
                        );
                        // During the init an order starts the ladder at once,
                        // whether or not the init acknowledged.
                        assert_eq!(
                            step(AttemptEvent::Order),
                            if init { A::ArmLadder } else { A::None },
                            "{case}"
                        );
                        // An acknowledgment read under no order, before the
                        // ladder acted, hands the incarnation over, whether or
                        // not its task already ended.
                        for ended in [false, true] {
                            for ordered in [false, true] {
                                assert_eq!(
                                    step(AttemptEvent::Ack { ordered, ended }),
                                    if init && !ordered && ladder == L::Off {
                                        A::Adopt
                                    } else {
                                        A::None
                                    },
                                    "{case} ordered={ordered} ended={ended}"
                                );
                            }
                        }
                        // The start timeout acts whenever the init has not
                        // acknowledged and the ladder is not past its window,
                        // and raises `Kill` only when none is stored.
                        assert_eq!(
                            step(AttemptEvent::StartTimeout),
                            if init && !acked && matches!(ladder, L::Off | L::Window) {
                                A::KillPath {
                                    raise: !kill_stored,
                                }
                            } else {
                                A::None
                            },
                            "{case}"
                        );
                        assert_eq!(
                            step(AttemptEvent::InitEnded),
                            if init && ladder == L::Off {
                                A::KillPath {
                                    raise: !kill_stored,
                                }
                            } else {
                                A::None
                            },
                            "{case}"
                        );
                        // A task still running after the ladder's two waits
                        // gets the verdict, never a fate.
                        assert_eq!(
                            step(AttemptEvent::LadderSpent),
                            if init { A::WriteVerdict } else { A::None },
                            "{case}"
                        );
                        // The end is a start failure only when the start
                        // timeout raised the first `Kill`, stopped when it was
                        // under an order, failed otherwise.
                        for ordered in [false, true] {
                            for timeout_raised in [false, true] {
                                let want = if timeout_raised {
                                    AttemptReport::StartTimeout
                                } else if ordered {
                                    AttemptReport::Stopped
                                } else {
                                    AttemptReport::Failed
                                };
                                assert_eq!(
                                    step(AttemptEvent::Ended {
                                        ordered,
                                        timeout_raised
                                    }),
                                    if init { A::Report(want) } else { A::None },
                                    "{case} ordered={ordered} timeout_raised={timeout_raised}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn teardown_end_is_orderly_over_every_reason_restart_type_and_policy() {
        use crate::error::ActorError;
        let failure = StopReason::Failure(ActorError::user("boom"));
        for restart_type in [
            RestartType::Permanent,
            RestartType::Transient,
            RestartType::Temporary,
        ] {
            for shutdown in [
                Shutdown::Kill,
                Shutdown::Timeout(MS(100)),
                Shutdown::Infinity,
            ] {
                for reason in [
                    StopReason::Graceful,
                    StopReason::ParentRequest,
                    failure.clone(),
                    StopReason::Cancelled,
                    StopReason::Kill,
                ] {
                    let want = match reason {
                        StopReason::ParentRequest => true,
                        StopReason::Graceful => restart_type != RestartType::Permanent,
                        StopReason::Kill => shutdown == Shutdown::Kill,
                        _ => false,
                    };
                    assert_eq!(
                        teardown_end_is_orderly(&reason, restart_type, shutdown),
                        want,
                        "{reason:?} {restart_type:?} {shutdown:?}"
                    );
                }
            }
        }
    }

    fn member(id: &str, life: ChildLifecycle) -> GroupMember {
        GroupMember {
            id: ActorId::from(id),
            life,
            restart_type: RestartType::Permanent,
        }
    }

    #[test]
    fn group_commit_holds_every_occupant_and_names_every_member_that_restarts() {
        use ChildLifecycle::{Down, Restarting, Running, Stopping};
        let down = |n| Down {
            incarnation: n,
            pending: DownPending::None,
        };
        let mut temporary = member("temporary", Running(7));
        temporary.restart_type = RestartType::Temporary;
        let children = vec![
            member("failed", down(1)),
            member("running", Running(2)),
            member(
                "stopping",
                Stopping {
                    incarnation: 4,
                    kind: ManualStop::Terminate,
                },
            ),
            temporary,
            member(
                "attempt",
                Restarting {
                    incarnation: 8,
                    next: 9,
                },
            ),
            member("down", down(10)),
        ];
        let commit = group_commit(
            RestartStrategy::OneForAll,
            &children,
            &ActorId::from("failed"),
        );
        let summary: Vec<(String, u64, bool, bool, bool)> = commit
            .iter()
            .map(|(id, n, action)| {
                (
                    id.as_str().to_string(),
                    *n,
                    action.held,
                    action.cancel,
                    action.restarts,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("failed".into(), 1, false, false, true),
                ("running".into(), 2, true, false, true),
                ("stopping".into(), 4, true, false, true),
                ("temporary".into(), 7, true, false, false),
                ("attempt".into(), 9, true, true, true),
                ("down".into(), 10, false, false, true),
            ]
        );
        // `RestForOne` covers the failed child and those started after it.
        let rest = group_commit(
            RestartStrategy::RestForOne,
            &children,
            &ActorId::from("stopping"),
        );
        let ids: Vec<&str> = rest.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["stopping", "temporary", "attempt", "down"]);
        assert!(!rest[0].2.held, "the failed child is not waited for");
    }

    #[test]
    fn retry_plan_covers_the_registry_as_it_is_and_keeps_released_members_down() {
        use ChildLifecycle::{Down, Running};
        let down = |n| Down {
            incarnation: n,
            pending: DownPending::None,
        };
        let mut temporary = member("temporary", Running(7));
        temporary.restart_type = RestartType::Temporary;
        let registry = vec![
            member("a", Running(1)),
            member("b", down(5)),
            member("released", down(3)),
            temporary,
            member("late", Running(9)),
        ];
        let b = ActorId::from("b");
        let released = [ActorId::from("released")];
        let ids = |plan: &[ActorId]| -> Vec<String> {
            plan.iter().map(|id| id.as_str().to_string()).collect()
        };

        // `OneForAll` covers every child, the one started since included; the
        // failed member and the released one have no occupant to wait for,
        // the others have.
        let plan = retry_plan(RestartStrategy::OneForAll, &registry, &released, &b);
        assert_eq!(ids(&plan.restart_order), ["a", "b", "late"]);
        let held: Vec<(String, bool)> = plan
            .commit
            .iter()
            .map(|(id, _, action)| (id.as_str().to_string(), action.held))
            .collect();
        assert_eq!(
            held,
            vec![
                ("a".to_string(), true),
                ("b".to_string(), false),
                ("released".to_string(), false),
                ("temporary".to_string(), true),
                ("late".to_string(), true),
            ]
        );
        // The released member is in the commit and does not restart.
        let restarts: Vec<bool> = plan.commit.iter().map(|(_, _, a)| a.restarts).collect();
        assert_eq!(restarts, [true, true, false, false, true]);

        // `RestForOne` covers the failed member and every child after it.
        let plan = retry_plan(RestartStrategy::RestForOne, &registry, &released, &b);
        assert_eq!(ids(&plan.restart_order), ["b", "late"]);
        let covered: Vec<&str> = plan.commit.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(covered, ["b", "released", "temporary", "late"]);

        // Without overrides the released child restarts too, and the plan
        // reads the scope from the one commit.
        let plan = retry_plan(RestartStrategy::OneForAll, &registry, &[], &b);
        assert_eq!(ids(&plan.restart_order), ["a", "b", "released", "late"]);
        assert_eq!(
            plan.commit,
            group_commit(RestartStrategy::OneForAll, &registry, &b)
        );
    }

    #[test]
    fn what_a_down_member_still_owes_changes_neither_its_stop_nor_its_restart() {
        use ChildLifecycle::{Down, Running};
        let down = |pending| Down {
            incarnation: 5,
            pending,
        };
        for restart_type in [
            RestartType::Permanent,
            RestartType::Transient,
            RestartType::Temporary,
        ] {
            for strategy in [RestartStrategy::OneForAll, RestartStrategy::RestForOne] {
                let plan = |pending| {
                    let mut owing = member("owing", down(pending));
                    owing.restart_type = restart_type;
                    let registry = vec![member("a", Running(1)), owing, member("z", Running(3))];
                    (
                        group_commit(strategy, &registry, &ActorId::from("a")),
                        retry_plan(strategy, &registry, &[], &ActorId::from("a")),
                    )
                };
                let none = plan(DownPending::None);
                assert_eq!(
                    plan(DownPending::Evaluate),
                    none,
                    "{strategy:?} {restart_type:?}"
                );
                assert_eq!(
                    plan(DownPending::Snapshot),
                    none,
                    "{strategy:?} {restart_type:?}"
                );
            }
        }
    }

    #[test]
    fn a_temporary_member_that_owes_its_end_is_planned_like_one_that_is_gone() {
        use ChildLifecycle::{Down, Running};
        // Absorbing a `Temporary` member removes its spec, so a plan made
        // after the absorption does not see the entry the first plan saw.
        // Both plans stop the same children and restart the same members.
        for strategy in [RestartStrategy::OneForAll, RestartStrategy::RestForOne] {
            for pending in [
                DownPending::None,
                DownPending::Evaluate,
                DownPending::Snapshot,
            ] {
                let mut owing = member(
                    "owing",
                    Down {
                        incarnation: 5,
                        pending,
                    },
                );
                owing.restart_type = RestartType::Temporary;
                let present = vec![member("a", Running(1)), owing, member("z", Running(3))];
                let gone = vec![member("a", Running(1)), member("z", Running(3))];
                let summary = |registry: &[GroupMember]| {
                    let plan = retry_plan(strategy, registry, &[], &ActorId::from("a"));
                    let stops: Vec<(ActorId, bool)> = plan
                        .commit
                        .iter()
                        .filter(|(_, _, action)| action.held)
                        .map(|(id, _, action)| (id.clone(), action.cancel))
                        .collect();
                    (stops, plan.restart_order)
                };
                assert_eq!(
                    summary(&present),
                    summary(&gone),
                    "{strategy:?} {pending:?}"
                );
            }
        }
    }

    #[test]
    fn a_member_end_is_decided_by_its_restart_type() {
        assert_eq!(
            group_member_end(RestartType::Permanent),
            MemberEnd::RestartInSlot
        );
        assert_eq!(
            group_member_end(RestartType::Transient),
            MemberEnd::RestartInSlot
        );
        assert_eq!(
            group_member_end(RestartType::Temporary),
            MemberEnd::PruneAndRemove
        );
    }

    #[test]
    fn teardown_walk_visits_newest_first_and_takes_every_occupant() {
        use ChildLifecycle::{Down, Restarting, Running, Stopping};
        let child = |life: Option<ChildLifecycle>, has_order, ask_possible, past_init| WalkChild {
            life,
            has_order,
            ask_possible,
            reasked: true,
            attempt_past_init: past_init,
        };
        let down = Down {
            incarnation: 1,
            pending: DownPending::None,
        };
        let children = vec![
            (
                ActorId::from("down"),
                child(Some(down), false, false, false),
            ),
            (
                ActorId::from("running"),
                child(Some(Running(2)), false, true, false),
            ),
            (
                ActorId::from("ordered"),
                child(Some(Running(3)), true, true, false),
            ),
            (
                ActorId::from("stopping"),
                child(
                    Some(Stopping {
                        incarnation: 4,
                        kind: ManualStop::Bounce,
                    }),
                    true,
                    false,
                    false,
                ),
            ),
            (
                ActorId::from("attempt"),
                child(
                    Some(Restarting {
                        incarnation: 5,
                        next: 6,
                    }),
                    true,
                    true,
                    true,
                ),
            ),
            (
                ActorId::from("fresh-attempt"),
                child(
                    Some(Restarting {
                        incarnation: 7,
                        next: 8,
                    }),
                    false,
                    true,
                    true,
                ),
            ),
            (
                ActorId::from("starting-attempt"),
                child(
                    Some(Restarting {
                        incarnation: 9,
                        next: 10,
                    }),
                    true,
                    true,
                    false,
                ),
            ),
            (ActorId::from("gone"), child(None, false, false, false)),
        ];
        let walk = teardown_walk(RestartStrategy::OneForOne, &children);
        let summary: Vec<(&str, TeardownAction)> = walk
            .iter()
            .map(|(id, action)| (id.as_str(), *action))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("gone", TeardownAction::Skip),
                (
                    "starting-attempt",
                    TeardownAction::OrderAttempt { ask: false }
                ),
                ("fresh-attempt", TeardownAction::OrderAttempt { ask: false }),
                ("attempt", TeardownAction::OrderAttempt { ask: true }),
                ("stopping", TeardownAction::Join { ask: false }),
                ("ordered", TeardownAction::Join { ask: true }),
                ("running", TeardownAction::Order),
                ("down", TeardownAction::Skip),
            ]
        );
    }

    #[test]
    fn a_pool_is_walked_in_one_call_with_the_action_of_every_child() {
        use ChildLifecycle::{Down, Running, Stopping};
        let child = |life: Option<ChildLifecycle>, has_order| WalkChild {
            life,
            has_order,
            ask_possible: true,
            reasked: false,
            attempt_past_init: false,
        };
        let children = vec![
            (ActorId::from("a"), child(Some(Running(1)), false)),
            (
                ActorId::from("b"),
                child(
                    Some(Stopping {
                        incarnation: 2,
                        kind: ManualStop::Bounce,
                    }),
                    true,
                ),
            ),
            (ActorId::from("c"), child(Some(Running(3)), true)),
            (
                ActorId::from("d"),
                child(
                    Some(Down {
                        incarnation: 4,
                        pending: DownPending::None,
                    }),
                    false,
                ),
            ),
        ];
        let pool = teardown_walk(RestartStrategy::SimpleOneForOne, &children);
        let serial = teardown_walk(RestartStrategy::OneForOne, &children);
        assert_eq!(
            pool, serial,
            "a pool decides each child as any strategy does"
        );
        let summary: Vec<(&str, TeardownAction)> = pool
            .iter()
            .map(|(id, action)| (id.as_str(), *action))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("d", TeardownAction::Skip),
                ("c", TeardownAction::Join { ask: true }),
                ("b", TeardownAction::Join { ask: true }),
                ("a", TeardownAction::Order),
            ],
            "every child with an occupant has an action in the one call"
        );
    }

    #[test]
    fn stop_queue_next_over_every_state_of_the_head() {
        use ChildLifecycle::{Down, Restarting, Running, Stopping};
        let lives = [
            None,
            Some(Running(1)),
            Some(Stopping {
                incarnation: 1,
                kind: ManualStop::Bounce,
            }),
            Some(Stopping {
                incarnation: 1,
                kind: ManualStop::Terminate,
            }),
            Some(Restarting {
                incarnation: 1,
                next: 2,
            }),
            Some(Down {
                incarnation: 1,
                pending: DownPending::None,
            }),
            Some(Down {
                incarnation: 1,
                pending: DownPending::Evaluate,
            }),
        ];
        for life in lives {
            for flags in 0..512u32 {
                let bit = |n: u32| flags & (1 << n) != 0;
                let head = QueueHead {
                    awaited: bit(0),
                    verdict: bit(1),
                    signalled: bit(2),
                    handled: bit(3),
                    life,
                    has_order: bit(4),
                    ask_possible: bit(5),
                    reasked: bit(6),
                    attempt_past_init: bit(7),
                    attempt_spawned: bit(8),
                };
                let ask = head.ask_possible && !head.reasked;
                let want = if !head.awaited || head.verdict || (head.handled && !head.signalled) {
                    QueueStep::Pop
                } else if head.signalled {
                    QueueStep::Wait
                } else {
                    match life {
                        Some(Running(_)) if !head.has_order => QueueStep::Order,
                        Some(Running(_) | Stopping { .. }) => QueueStep::Join { ask },
                        Some(Restarting { .. }) if !head.attempt_spawned => {
                            QueueStep::OrderUnpublished
                        }
                        Some(Restarting { .. }) => QueueStep::OrderAttempt {
                            ask: head.has_order && head.attempt_past_init && ask,
                        },
                        Some(Down { .. }) | None => QueueStep::Await,
                    }
                };
                assert_eq!(stop_queue_next(&head), want, "{head:?}");
            }
        }
    }

    #[test]
    fn absorbable_over_every_place_membership_and_ledger_entry() {
        let owed = Some(ChildLifecycle::Down {
            incarnation: 3,
            pending: DownPending::Evaluate,
        });
        for place in [
            ReportPlace::Queued,
            ReportPlace::Worklist,
            ReportPlace::DeathPlane,
        ] {
            let want = match place {
                ReportPlace::DeathPlane => Absorption::Snapshot,
                _ => Absorption::Deliver,
            };
            assert_eq!(absorbable(place, true, owed), want);
            assert_eq!(absorbable(place, false, owed), Absorption::Keep);
            for other in [
                None,
                Some(ChildLifecycle::Running(3)),
                Some(ChildLifecycle::Down {
                    incarnation: 3,
                    pending: DownPending::None,
                }),
                Some(ChildLifecycle::Down {
                    incarnation: 3,
                    pending: DownPending::Snapshot,
                }),
            ] {
                assert_eq!(
                    absorbable(place, true, other),
                    Absorption::Keep,
                    "{other:?}"
                );
            }
        }
    }

    #[test]
    fn barrier_blocked_only_by_verdicts_over_every_set_of_flags() {
        let id = |s: &str| ActorId::from(s);
        assert_eq!(barrier_blocked_only_by_verdicts(&[]), None);
        for a in [false, true] {
            for b in [false, true] {
                for c in [false, true] {
                    let entries = [(id("c"), c), (id("b"), b), (id("a"), a)];
                    let want = (a && b && c).then(|| id("c"));
                    assert_eq!(barrier_blocked_only_by_verdicts(&entries), want);
                    assert_eq!(
                        barrier_blocked_only_by_verdicts(&entries[..2]),
                        (c && b).then(|| id("c"))
                    );
                }
            }
        }
        assert_eq!(
            barrier_blocked_only_by_verdicts(&[(id("front"), true)]),
            Some(id("front"))
        );
    }
}
