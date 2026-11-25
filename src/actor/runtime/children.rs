//! Child deaths as the supervisor sees them: routing a report, the events and the callback.

use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;

use super::{
    advance_group_stop, fail_with, handle_worklist_with, initiate_restart, settle_barrier,
    settle_ignored_attempt, HookOutcome, IgnoredSettle,
};
use crate::actor::context::ActorContext;
use crate::actor::incarnation::turn_clock::push_turn;
use crate::actor::incarnation::IncarnationRef;
use crate::actor::panic::{catch_hook_t, payload_into_string};
use crate::actor::supervision::{
    advance_restart_chain, rules, BudgetCharge, ChainStep, ChildLifecycle, DownPending,
    GroupMembership, GroupPhase, GroupScope, Origin, Route, SupervisionState,
};
use crate::actor::Actor;
use crate::error::ActorError;
use crate::telemetry::{reports, ActorRef, EventKind, HookKind, TelemetryEvent};
use crate::types::{ActorId, ChildEvent, ChildStoppedInternal, StopReason, SupervisionAction};

// ---------------------------------------------------------------------------
// Child supervision
// ---------------------------------------------------------------------------

/// What the run loop does with one report of a child's end, once the ledger
/// has recorded it.
pub(super) enum Routed {
    /// The report waits behind the pending group restart; the ledger records
    /// the decision it owes. Nothing is delivered now.
    Queued,
    /// The report is delivered with this action and decides nothing else.
    Deliver(SupervisionAction),
    /// The restart chain's own start failed: the whole group is retried,
    /// charged to the budget once.
    ChainRetry,
    /// The end of an attempt's incarnation whose start was ignored: the
    /// ledger is settled and nothing is charged, retried or delivered.
    Ignored(Box<IgnoredSettle>),
    /// The ordinary death path: the strategy decides.
    Evaluate,
}

/// What the run loop does after it recorded a report as a member's end.
#[derive(Default)]
pub(super) struct GroupSteps {
    /// The stop phase's barrier emptied: the chain starts with this restart
    /// order, these overrides and the group's scope.
    pub(super) begin_chain: Option<ChainStart>,
    /// The chain's front member is down: the chain advances.
    pub(super) advance_chain: bool,
    /// The member leaves the group removed instead of restarted: a
    /// `terminate_child` released it, or it is a `Temporary` member. Its
    /// spec is pruned when it is prunable and its end is delivered as
    /// removed.
    pub(super) manual_override: bool,
}

/// Records one report of a child's end in the ledger and decides its route.
///
/// The report is of a child's occupant or of its owed end (a stale report
/// was dropped before). Membership is read before any change, and every
/// ledger write names the reported incarnation: a failed restart attempt
/// `Restarting { n, m }` is reported as `m` and recorded as `Down { m, .. }`.
pub(super) fn route_report(
    sup: &mut SupervisionState,
    ev: &ChildStoppedInternal,
    steps: &mut GroupSteps,
) -> Routed {
    let id = &ev.child_id;
    let incarnation = ev.incarnation;
    let Some((life, origin)) = sup
        .registry
        .get(id)
        .map(|child| (child.lifecycle, child.origin))
    else {
        return Routed::Evaluate;
    };
    let attempt = matches!(life, ChildLifecycle::Restarting { next, .. } if next == incarnation);
    let membership = sup.group_membership(id);
    // What a group does with this member's end, wherever it is delivered.
    let pruned = sup.registry.get(id).is_some_and(|child| {
        rules::group_member_end(child.spec.restart_type) == rules::MemberEnd::PruneAndRemove
    });
    let chain_front = match sup.pending_group.as_ref() {
        Some(GroupPhase::Restarting { queue, started, .. }) if queue.front() == Some(id) => {
            Some(started.get(id).copied())
        }
        _ => None,
    };
    let record = |sup: &mut SupervisionState, pending: DownPending| {
        if let Some(child) = sup.registry.get_mut(id) {
            child.transition_expected(ChildLifecycle::Down {
                incarnation,
                pending,
            });
        }
    };

    if attempt {
        // An attempt whose task was lost is followed to its incarnation's
        // end through the death plane: an ignored end takes one path for
        // every route below.
        if ev.ignored {
            return Routed::Ignored(Box::new(settle_ignored_attempt(sup, id, incarnation)));
        }
        // The attempt spawned an incarnation if its cell holds the subtree:
        // the next start for the child waits for it, whether or not the
        // incarnation was adopted.
        if let Some(child) = sup.registry.get_mut(id) {
            child.note_subtree();
        }
        let chain_front_own = chain_front == Some(Some(incarnation));
        return match rules::attempt_failure_route(chain_front_own, membership, origin) {
            Route::ChainRetry => {
                record(sup, DownPending::None);
                Routed::ChainRetry
            }
            Route::Member => {
                record(sup, DownPending::None);
                if settle_barrier(sup, id, incarnation) {
                    steps.begin_chain = advance_group_stop(sup);
                }
                steps.advance_chain = chain_front.is_some();
                steps.manual_override = pruned;
                Routed::Deliver(SupervisionAction::RestartInitiated)
            }
            Route::Removed => {
                record(sup, DownPending::None);
                Routed::Deliver(SupervisionAction::Removed)
            }
            Route::Evaluate if sup.pending_group.is_some() => {
                record(sup, DownPending::Evaluate);
                sup.queued_triggers.push_back(ev.clone());
                Routed::Queued
            }
            Route::Evaluate => {
                record(sup, DownPending::None);
                Routed::Evaluate
            }
        };
    }

    if sup.pending_group.is_none() {
        record(sup, DownPending::None);
        return Routed::Evaluate;
    }
    // The stop phase awaits this very incarnation: its end settles the
    // barrier entry, and a member's end belongs to its group.
    let settled = match sup.pending_group.as_mut() {
        Some(GroupPhase::Stopping(group)) => group
            .settle_awaited(id, incarnation)
            .then(|| group.manual_overrides.get(id) == Some(&incarnation)),
        _ => None,
    };
    if let Some(overridden) = settled {
        steps.manual_override = overridden || pruned;
        record(sup, DownPending::None);
        steps.begin_chain = advance_group_stop(sup);
        return Routed::Deliver(SupervisionAction::RestartInitiated);
    }
    if membership == GroupMembership::Queued {
        // A member's own end belongs to its group, whether it owed its
        // decision or died as the occupant (one the chain stopped): the
        // group restarts the member, nothing is charged, and the chain goes
        // on when the member is its front.
        record(sup, DownPending::None);
        steps.advance_chain = chain_front.is_some();
        steps.manual_override = pruned;
        return Routed::Deliver(SupervisionAction::RestartInitiated);
    }
    // A child the group does not restart: its decision waits for the group,
    // and the ledger says so now.
    record(sup, DownPending::Evaluate);
    sup.queued_triggers.push_back(ev.clone());
    Routed::Queued
}

/// What the restart chain of a group starts with: the restart order, the
/// members a `terminate_child` released, and the sets the group keeps through
/// its retries.
pub(super) struct ChainStart {
    pub(super) order: Vec<ActorId>,
    pub(super) overrides: HashMap<ActorId, u64>,
    pub(super) scope: GroupScope,
}

/// Takes the stop phase's group out of the pending state when its barrier
/// emptied, as the restart order and overrides the chain starts with. A
/// member a `terminate_child` released left the restart order when the call
/// was made; the filter by `overrides` is a second line of defense for it.
pub(super) fn take_chain_order(sup: &mut SupervisionState) -> Option<ChainStart> {
    match sup.pending_group.take() {
        Some(GroupPhase::Stopping(group)) => {
            let overrides = group.manual_overrides;
            Some(ChainStart {
                order: group
                    .restart_order
                    .into_iter()
                    .filter(|id| !overrides.contains_key(id))
                    .collect(),
                overrides,
                scope: group.scope,
            })
        }
        other => {
            sup.pending_group = other;
            None
        }
    }
}

/// Queues the death event of the child incarnation `incarnation`, with the
/// decision the event carries: the supervisor's own identity names the
/// supervisor (its id read as a root with incarnation 0 when it was spawned
/// with none), and the child's reference is rebuilt from it. Called before
/// the callback, with the decision already made, so the budget event of the
/// same death comes first.
pub(super) fn emit_child_died<A: Actor>(
    ctx: &ActorContext<A>,
    event: &ChildEvent,
    incarnation: u64,
) {
    crate::telemetry::with_mode(
        ctx.stop_lane().abort_record(),
        ctx.system_in_hand(),
        |mode| {
            if !mode.armed(EventKind::ChildDied) {
                return;
            }
            let (supervisor, owner) = crate::telemetry::supervisor_of(
                mode,
                ctx.stop_lane().abort_record(),
                ctx.actor_id(),
            );
            crate::telemetry::push_for(mode, owner.as_ref(), EventKind::ChildDied, || {
                TelemetryEvent::ChildDied {
                    child: ActorRef::for_child_of(&supervisor, event.child_id.clone(), incarnation),
                    supervisor,
                    reason: event.reason.clone(),
                    decision: event.action,
                }
            });
        },
    );
}

/// Queues the budget event of one charge against the restart budget of the
/// supervisor behind `ctx`: the death of `child` (the incarnation
/// `incarnation`) was charged, and the charge stands as `charge` says.
pub(super) fn emit_restart_budget<A: Actor>(
    ctx: &ActorContext<A>,
    child: &ActorId,
    incarnation: u64,
    charge: &BudgetCharge,
) {
    // The charge that exhausts the budget is the one the supervisor does not
    // accept: it stops there, so this is the only report of it.
    if !charge.accepted {
        reports::reached_max_restart_intensity(
            ctx.actor_id(),
            child,
            incarnation,
            charge.max,
            charge.window,
        );
    }
    crate::telemetry::with_mode(
        ctx.stop_lane().abort_record(),
        ctx.system_in_hand(),
        |mode| {
            if !mode.armed(EventKind::RestartBudget) {
                return;
            }
            let (supervisor, owner) = crate::telemetry::supervisor_of(
                mode,
                ctx.stop_lane().abort_record(),
                ctx.actor_id(),
            );
            crate::telemetry::push_for(mode, owner.as_ref(), EventKind::RestartBudget, || {
                TelemetryEvent::RestartBudget {
                    child: ActorRef::for_child_of(&supervisor, child.clone(), incarnation),
                    supervisor,
                    used: charge.used,
                    max: charge.max,
                    window: charge.window,
                    accepted: charge.accepted,
                }
            });
        },
    );
}

/// Delivers one `on_child_stopped`; a failing callback stops the supervisor.
pub(super) async fn deliver_child_event<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    event: &ChildEvent,
    incarnation: u64,
) -> ControlFlow<StopReason> {
    emit_child_died(ctx, event, incarnation);
    let delivered = hook_turn!(ctx, HookKind::OnChildStopped, always, |token| catch_hook_t(
        token,
        || actor.on_child_stopped(event, ctx)
    ));
    // The chain does not run after this callback: a message it asked for is
    // dropped here, not carried to a later event.
    ctx.drop_continuation();
    match delivered {
        Ok(Ok(())) => ControlFlow::Continue(()),
        Ok(Err(err)) => fail_with(ctx, err),
        Err(payload) => fail_with(ctx, ActorError::Panic(payload_into_string(payload))),
    }
}

/// Advances the restart chain and starts its front member when that member
/// is down.
pub(super) fn advance_and_initiate<A: Actor>(
    ctx: &mut ActorContext<A>,
    work: &mut VecDeque<ChildStoppedInternal>,
) {
    let step = ctx
        .supervision_mut()
        .map(|sup| advance_restart_chain(sup, work));
    if let Some(ChainStep::Initiate(front)) = step {
        initiate_restart(ctx, &front, Origin::Chain);
    }
}

/// Handles child-death events: manual-completion delivery, stale-report
/// filtering, group-restart bookkeeping, strategy evaluation, restart
/// initiation, and the `on_child_stopped` callback.
///
/// Every report is matched exactly: by the child and the incarnation it is
/// of. A report of the ledger entry's occupant, or of an end the entry owes
/// its restart decision, is handled; a report of any other incarnation
/// changes nothing but the pending snapshot of a completed manual stop.
///
/// Runs as a worklist so that a completing group restart can drain the
/// failure events that were queued while the group was pending.
pub(super) async fn handle_child_stopped<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    event: ChildStoppedInternal,
) -> ControlFlow<StopReason> {
    handle_child_stopped_with(actor, ctx, event, true).await
}

/// [`handle_child_stopped`]. `terminated` says whether the supervisor's
/// report `child_terminated` covers the first report: false for the end of a
/// restart attempt that never started an incarnation, which is reported as a
/// `start_error` where the attempt's outcome is handled.
pub(super) async fn handle_child_stopped_with<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    event: ChildStoppedInternal,
    terminated: bool,
) -> ControlFlow<StopReason> {
    // The report in hand is held apart from the worklist, so the worklist
    // allocates only when a second report queues behind it.
    let mut work: VecDeque<ChildStoppedInternal> = VecDeque::new();
    let flow = handle_worklist_with(actor, ctx, Some(event), &mut work, terminated).await;
    if flow.is_break() {
        // The loop ends with reports still in the worklist: they are
        // collected for their own deliveries.
        if let Some(sup) = ctx.supervision_mut() {
            sup.owed.extend(work);
        }
    }
    flow
}

/// The debt of one reported death: owed to the supervision state's decision
/// until the pass over it ends, which clears it on every path out, an early
/// return or an unwind included, unless the pass keeps it.
pub(super) struct Decision {
    owner: Option<IncarnationRef>,
    incarnation: u64,
    kept: bool,
}

impl Decision {
    pub(super) fn new(owner: Option<IncarnationRef>, incarnation: u64) -> Self {
        Self {
            owner,
            incarnation,
            kept: false,
        }
    }

    /// The report is parked or goes through the worklist again: the debt
    /// stays until the pass that decides it.
    pub(super) fn keep(&mut self) {
        self.kept = true;
    }
}

impl Drop for Decision {
    fn drop(&mut self) {
        if !self.kept {
            if let Some(owner) = &self.owner {
                crate::telemetry::death_decided(owner, self.incarnation);
            }
        }
    }
}
