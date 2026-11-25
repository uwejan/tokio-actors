//! Restarts: the worklist, the retry of a group, the outcome of an attempt and its adoption.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;

use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::{
    absorb_owed, absorb_records, advance_and_initiate, advance_group_stop, advance_stop_queue,
    claim_death, deliver_absorbed, deliver_child_event, drop_removed, emit_restart_budget,
    fail_with, handle_child_stopped_with, handle_released, initiate_restart, keep_first_of,
    owe_deliveries, owe_report, plan_group_stop, prune_spec_if_needed, report_unexpected_end,
    route_report, settle_barrier, spawn_runner, ChainStart, Decision, GroupSteps, Owner, Routed,
};
use crate::actor::context::ActorContext;
use crate::actor::incarnation::StopLane;
#[cfg(test)]
use crate::actor::runtime::end_hooks;
use crate::actor::supervision::{
    advance_restart_chain, evaluate_strategy_charged, rules, ChainStep, ChildLifecycle,
    ChildLinkGuard, DownPending, GroupPhase, GroupRestart, GroupScope, IgnoredEnd, Origin,
    ReportClass, RestartFn, RestartOutcome, SlotIo, StrategyOutcome, SupervisionState, TaskPlane,
    DEFAULT_KILL_GRACE,
};
use crate::actor::Actor;
use crate::error::{ActorError, SupervisionError};
use crate::telemetry::{reports, AttemptOutcome};
use crate::types::{ActorId, ChildEvent, ChildStoppedInternal, StopReason, SupervisionAction};

/// Runs the worklist of [`handle_child_stopped`] until it is empty: `first`
/// is handled before anything in `work`, and a report pushed back to the
/// front of `work` is handled next.
pub(super) async fn handle_worklist<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    first: Option<ChildStoppedInternal>,
    work: &mut VecDeque<ChildStoppedInternal>,
) -> ControlFlow<StopReason> {
    handle_worklist_with(actor, ctx, first, work, true).await
}

/// [`handle_worklist`]; `first_terminated` says whether the supervisor's
/// report `child_terminated` covers `first`.
pub(super) async fn handle_worklist_with<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    mut first: Option<ChildStoppedInternal>,
    work: &mut VecDeque<ChildStoppedInternal>,
    first_terminated: bool,
) -> ControlFlow<StopReason> {
    let mut terminated = first_terminated;
    while let Some(ev) = first.take().or_else(|| work.pop_front()) {
        // Only the first report of the call can be an attempt's: the rest
        // came from the worklist and are deaths of incarnations.
        let reports_death = std::mem::replace(&mut terminated, true);
        // The death in hand is owed a decision until this pass over it ends
        // (the restart it started has its ticket by then), whichever way it
        // ends; a pass that parks or re-queues the report keeps the debt.
        let mut decision = Decision::new(
            ctx.supervision_ref()
                .and_then(|sup| sup.registry.owner_record()),
            ev.incarnation,
        );
        // The end of a child whose awaited start failed: it kept no spec, so
        // its report changes nothing and reaches no callback.
        if ctx.supervision_mut().is_some_and(|sup| {
            !sup.silenced.is_empty() && sup.take_silenced(&ev.child_id, ev.incarnation)
        }) {
            continue;
        }
        // Manual-completion delivery: `stop_child`/`terminate_child` already
        // classified this exact incarnation's disposition the moment its real
        // fate was observed (see `ActorContext::manual_stop_child`) and
        // stored the resulting snapshot here, independent of whatever the
        // registry entry looks like by now - a same-handler `delete_child`
        // and respawn of the same name may already have replaced or removed
        // it. This is the ONLY place `on_child_stopped` fires for a manual
        // stop, and it fires from the snapshot, exactly once.
        //
        // A snapshot belongs to an incarnation the ledger entry has moved
        // past: the report of the entry's occupant, or of an end it owes its
        // decision, is never delivered from one.
        let snapshot = ctx.supervision_mut().and_then(|sup| {
            let class = sup
                .registry
                .get(&ev.child_id)
                .map(|child| rules::report_class(child.lifecycle, ev.incarnation));
            if matches!(
                class,
                Some(ReportClass::Occupant | ReportClass::OwedEvaluation)
            ) {
                return None;
            }
            sup.pending_manual_events
                .remove(&(ev.child_id.clone(), ev.incarnation))
        });
        if let Some(child_event) = snapshot {
            // This death may ALSO be the individual death an in-flight
            // group's `Stopping` phase is awaiting (a `terminate_child`
            // override recorded while that phase was in flight - see
            // `ActorContext::manual_stop_child` - already settled the
            // ledger and this snapshot synchronously; the run loop still
            // owns `awaiting` removal and starting the chain once it
            // empties). Neither duty short-circuits the other: the group
            // bookkeeping below always runs first, and the snapshot always
            // fires exactly once, from here, regardless of group
            // membership. Delivering a snapshot never moves the ledger
            // beyond clearing the marker it left: the entry may already
            // hold a newer incarnation.
            let mut begin_chain = None;
            if let Some(sup) = ctx.supervision_mut() {
                if let Some(child) = sup.registry.get_mut(&ev.child_id) {
                    if child.lifecycle
                        == (ChildLifecycle::Down {
                            incarnation: ev.incarnation,
                            pending: DownPending::Snapshot,
                        })
                    {
                        child.transition_expected(ChildLifecycle::Down {
                            incarnation: ev.incarnation,
                            pending: DownPending::None,
                        });
                    }
                }
                if settle_barrier(sup, &ev.child_id, ev.incarnation) {
                    begin_chain = advance_group_stop(sup);
                }
            }
            if let Some(start) = begin_chain {
                start_restart_chain(ctx, start, work);
            } else {
                // A front member whose manual stop just completed does not
                // hold the chain any more.
                let is_front =
                    ctx.supervision_ref()
                        .is_some_and(|sup| match sup.pending_group.as_ref() {
                            Some(GroupPhase::Restarting { queue, .. }) => {
                                queue.front() == Some(&ev.child_id)
                            }
                            _ => false,
                        });
                if is_front {
                    advance_and_initiate(ctx, work);
                }
            }
            // The stop landed after the incarnation had already ended by its
            // ignored start: the bookkeeping above ran, and the end is no
            // death to report.
            if !ev.ignored {
                deliver_child_event(actor, ctx, &child_event, ev.incarnation).await?;
            }
            continue;
        }

        // Cancel-safety fallback: the ledger is still `Stopping{kind}` for
        // this exact incarnation, meaning a caller committed a manual stop
        // and then dropped its own future before ever observing the fate (no
        // snapshot was ever recorded above). The commit already happened
        // (synchronously, before any await) and this death IS that commit's
        // own result, so the run loop finishes the classification itself
        // instead of falling through to the strategy-evaluated crash path.
        let uncommitted_manual = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&ev.child_id))
            .and_then(|child| match child.lifecycle {
                ChildLifecycle::Stopping { incarnation, kind } if incarnation == ev.incarnation => {
                    Some(kind)
                }
                _ => None,
            });
        if let Some(kind) = uncommitted_manual {
            let removed = ctx.classify_manual_completion(
                &ev.child_id,
                ev.incarnation,
                kind,
                ev.reason.clone(),
            );
            decision.keep();
            work.push_front(ev);
            // The report goes on through the worklist; on a panic of the
            // factory's drop it stays there, first, for the final deliveries.
            if let Some(err) = drop_removed(ctx, removed) {
                return ControlFlow::Break(StopReason::Failure(err));
            }
            continue;
        }

        // Stale-report filter: a report of an incarnation the entry has
        // moved past is not a death of the child the registry tracks.
        let stale = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&ev.child_id))
            .is_some_and(|child| {
                rules::report_class(child.lifecycle, ev.incarnation) == ReportClass::Stale
            });
        if stale {
            continue;
        }

        let mut steps = GroupSteps::default();
        let routed = match ctx.supervision_mut() {
            Some(sup) => route_report(sup, &ev, &mut steps),
            None => Routed::Evaluate,
        };

        match routed {
            Routed::Queued => {
                decision.keep();
                continue;
            }
            Routed::Deliver(mut action) => {
                // A caller's `terminate_child` on this member during the
                // group's Stopping phase wins over the group's default
                // disposition: it is reported and left down instead of being
                // folded into the restart chain (the member left the restart
                // order when the call was made).
                if let Some(start) = steps.begin_chain.take() {
                    start_restart_chain(ctx, start, work);
                } else if steps.advance_chain {
                    advance_and_initiate(ctx, work);
                }
                let child_name = ctx
                    .supervision_ref()
                    .and_then(|sup| sup.registry.get(&ev.child_id))
                    .and_then(|child| child.name.as_deref().map(str::to_string));
                let mut removed = None;
                if steps.manual_override {
                    removed = prune_spec_if_needed(ctx, &ev.child_id);
                    action = SupervisionAction::Removed;
                }
                if let Some(err) = drop_removed(ctx, removed) {
                    // An ignored end owes no report.
                    if !ev.ignored {
                        owe_report(ctx, ev, child_name);
                    }
                    return ControlFlow::Break(StopReason::Failure(err));
                }
                // An ignored end is bookkeeping only: the group's chain and
                // the spec's removal above ran, and no report is made.
                if ev.ignored {
                    continue;
                }
                let child_event = ChildEvent {
                    child_id: ev.child_id,
                    child_name,
                    reason: ev.reason,
                    action,
                };
                deliver_child_event(actor, ctx, &child_event, ev.incarnation).await?;
                continue;
            }
            Routed::ChainRetry => {
                // The in-flight member's restart failed (factory panic,
                // spawn error, or the fresh incarnation died before
                // adoption): the whole group is laid out again, charging the
                // budget once (OTP try_again_restart parity).
                let now = Instant::now();
                let charge = ctx
                    .supervision_mut()
                    .map(|sup| sup.budget.check_and_record(now));
                if let Some(charge) = &charge {
                    emit_restart_budget(ctx, &ev.child_id, ev.incarnation, charge);
                }
                let budget_ok = charge.is_some_and(|charge| charge.accepted);
                if budget_ok {
                    retry_group(actor, ctx, work, &ev).await?;
                    continue;
                }
                let child_name = ctx
                    .supervision_ref()
                    .and_then(|sup| sup.registry.get(&ev.child_id))
                    .and_then(|child| child.name.as_deref().map(str::to_string));
                let child_event = ChildEvent {
                    child_id: ev.child_id,
                    child_name,
                    reason: ev.reason,
                    action: SupervisionAction::BudgetExhausted,
                };
                deliver_child_event(actor, ctx, &child_event, ev.incarnation).await?;
                ctx.record_failure(SupervisionError::BudgetExhausted.into());
                return ControlFlow::Break(StopReason::ParentRequest);
            }
            Routed::Ignored(settle) => {
                // A lost attempt's incarnation ended by its ignored start:
                // no charge, no retry of the group, no report.
                match carry_out_ignored(ctx, *settle) {
                    Ok(released) => {
                        // The reports the chain released join the worklist
                        // behind what it holds, as on every other route that
                        // ends a group.
                        work.extend(released);
                        continue;
                    }
                    Err(reason) => return ControlFlow::Break(reason),
                }
            }
            Routed::Evaluate => {}
        }

        // The end of an incarnation whose start was ignored is no death:
        // nothing is restarted, charged, reported or delivered, and the spec
        // is kept down or removed by one rule. The ledger already holds the
        // end as `Down` with nothing owed.
        if let Some(end) = ignored_start_end(ctx, &ev) {
            let removed = settle_ignored_end(ctx, &ev, end);
            if let Some(err) = drop_removed(ctx, removed) {
                return ControlFlow::Break(StopReason::Failure(err));
            }
            continue;
        }

        // Normal path: the ledger holds the end; evaluate the strategy, act.
        let child_name = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&ev.child_id))
            .and_then(|child| child.name.as_deref().map(str::to_string));

        // The supervisor's report of the death, before the decision it leads
        // to, as Erlang/OTP's `do_restart` writes it.
        if cfg!(feature = "tracing") && reports_death {
            if let Some((restart_type, sensitive)) = ctx
                .supervision_ref()
                .and_then(|sup| sup.registry.get(&ev.child_id))
                .map(|child| (child.spec.restart_type, child.spec.sys.sensitive))
            {
                reports::child_terminated(
                    ctx.actor_id(),
                    &ev.child_id,
                    ev.incarnation,
                    restart_type,
                    &ev.reason,
                    sensitive,
                );
            }
        }

        let mut charged = None;
        let now = Instant::now();
        let outcome = ctx.supervision_mut().map(|sup| {
            evaluate_strategy_charged(
                sup,
                &ev.child_id,
                &ev.reason,
                now,
                &mut |incarnation, charge| {
                    charged = Some((incarnation, charge));
                },
            )
        });
        if let Some((incarnation, charge)) = &charged {
            emit_restart_budget(ctx, &ev.child_id, *incarnation, charge);
        }

        let mut removed = None;
        let action = match outcome {
            None => SupervisionAction::NotSupervised,
            Some(StrategyOutcome::RestartOne(id)) => {
                initiate_restart(ctx, &id, Origin::Strategy);
                SupervisionAction::RestartInitiated
            }
            Some(StrategyOutcome::RestartGroup { commit }) => {
                // Owed evaluations of the group's members are absorbed into
                // the group first, then the group's stops are carried out.
                if let ControlFlow::Break(reason) = absorb_owed(actor, ctx, work, &commit).await {
                    // The trigger's own report was taken, charged and
                    // recorded, and its delivery has not run: it stays owed
                    // ahead of the absorbed reports the failing delivery
                    // left, for the final collection.
                    if let Some(sup) = ctx.supervision_mut() {
                        sup.owed.push_front(ev);
                    }
                    return ControlFlow::Break(reason);
                }
                let restart_order: Vec<ActorId> = commit
                    .iter()
                    .filter(|(_, _, action)| action.restarts)
                    .map(|(id, _, _)| id.clone())
                    .collect();
                // The scope the group keeps through its retries: under
                // `RestForOne`, the keys before the first failed member stay
                // outside it.
                let scope = GroupScope {
                    restarted: HashSet::new(),
                    outside: ctx
                        .supervision_ref()
                        .map(|sup| sup.first_plan_outside(&ev.child_id))
                        .unwrap_or_default(),
                };
                let (awaiting, stop_queue) = match ctx.supervision_ref() {
                    Some(sup) => plan_group_stop(sup, &commit),
                    None => (HashMap::new(), VecDeque::new()),
                };
                if awaiting.is_empty() {
                    // No live members to stop: go straight to the sequential
                    // restart chain.
                    start_restart_chain(
                        ctx,
                        ChainStart {
                            order: restart_order,
                            overrides: HashMap::new(),
                            scope,
                        },
                        work,
                    );
                } else if let Some(sup) = ctx.supervision_mut() {
                    sup.pending_group = Some(GroupPhase::Stopping(GroupRestart {
                        awaiting,
                        stop_queue,
                        head_reached: false,
                        restart_order,
                        manual_overrides: HashMap::new(),
                        scope,
                    }));
                    sup.note_group();
                    advance_stop_queue(sup);
                }
                SupervisionAction::RestartInitiated
            }
            Some(StrategyOutcome::Remove) => {
                // Read before the spec is pruned: the end is one the
                // supervisor did not order, so it may shut the supervisor
                // down.
                let due = ctx
                    .supervision_ref()
                    .is_some_and(|sup| sup.auto_shutdown_due(&ev.child_id));
                removed = prune_spec_if_needed(ctx, &ev.child_id);
                if due {
                    SupervisionAction::AutoShutdown
                } else {
                    SupervisionAction::Removed
                }
            }
            Some(StrategyOutcome::BudgetExhausted) => SupervisionAction::BudgetExhausted,
        };
        if let Some(err) = drop_removed(ctx, removed) {
            owe_report(ctx, ev, child_name);
            return ControlFlow::Break(StopReason::Failure(err));
        }

        let child_event = ChildEvent {
            child_id: ev.child_id,
            child_name,
            reason: ev.reason,
            action,
        };
        deliver_child_event(actor, ctx, &child_event, ev.incarnation).await?;

        if matches!(action, SupervisionAction::BudgetExhausted) {
            ctx.record_failure(SupervisionError::BudgetExhausted.into());
            // OTP parity: an intensity-exceeded supervisor exits with
            // `shutdown` (our ParentRequest), so its own supervisor's
            // Transient policy does not restart it. Direct break: not
            // vetoable by pre_stop.
            return ControlFlow::Break(StopReason::ParentRequest);
        }

        if matches!(action, SupervisionAction::AutoShutdown) {
            // The supervisor shuts down as OTP's `shutdown` exit (our
            // ParentRequest), with nothing recorded as a failure: its own
            // supervisor's Transient policy does not restart it. Direct
            // break, not vetoable by pre_stop; the deaths still pending are
            // delivered as `SupervisorStopping` before `on_stopped`.
            return ControlFlow::Break(StopReason::ParentRequest);
        }
    }

    ControlFlow::Continue(())
}

/// The retry of a group restart after the chain's own start of a member
/// failed, once the budget took its one charge.
///
/// The group is planned again from the registry as it is now
/// ([`SupervisionState::plan_retry`]): `OneForAll` covers every child, one
/// spawned since included, and `RestForOne` the failed member and every child
/// after it. The owed evaluations of the members are absorbed, every member
/// with an occupant is committed (a member whose restart attempt is in flight
/// is ordered and cancelled, one already stopping is joined), and the group
/// returns to the stop phase with the overrides it kept, so a member a
/// `terminate_child` released stays down. When no member has an occupant, the
/// chain starts in the same step. The failed attempt's own end is delivered
/// once with `RestartInitiated`, then the absorbed reports; the ledger and
/// the group hold the whole plan before any callback runs.
async fn retry_group<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    work: &mut VecDeque<ChildStoppedInternal>,
    failed: &ChildStoppedInternal,
) -> ControlFlow<StopReason> {
    let mut deliveries: VecDeque<(ChildEvent, u64)> = VecDeque::new();
    let mut removed: Vec<RestartFn> = Vec::new();
    let mut awaiting: HashMap<ActorId, u64> = HashMap::new();
    let mut stop_queue: VecDeque<ActorId> = VecDeque::new();
    let mut restart_order: Vec<ActorId> = Vec::new();
    let mut overrides: HashMap<ActorId, u64> = HashMap::new();
    let mut scope = GroupScope::default();
    if let Some(sup) = ctx.supervision_mut() {
        deliveries.push_back((
            ChildEvent {
                child_id: failed.child_id.clone(),
                child_name: sup
                    .registry
                    .get(&failed.child_id)
                    .and_then(|child| child.name.as_deref().map(str::to_string)),
                reason: failed.reason.clone(),
                action: SupervisionAction::RestartInitiated,
            },
            failed.incarnation,
        ));
        // The members the retry covers are absorbed first, and the group is
        // planned from the ledger they leave. A down member's stop and
        // restart do not depend on what its end still owes, so the two plans
        // cover the same members.
        let covered = sup.plan_retry(&failed.child_id);
        let (absorbed, factories) = absorb_records(sup, work, &covered.commit);
        deliveries.extend(absorbed);
        removed = factories;
        let plan = sup.plan_retry(&failed.child_id);
        (overrides, scope) = match sup.pending_group.as_ref() {
            Some(GroupPhase::Restarting {
                overrides, scope, ..
            }) => (overrides.clone(), scope.clone()),
            _ => (HashMap::new(), GroupScope::default()),
        };
        (awaiting, stop_queue) = plan_group_stop(sup, &plan.commit);
        restart_order = plan.restart_order;
    }
    if awaiting.is_empty() {
        start_restart_chain(
            ctx,
            ChainStart {
                order: restart_order,
                overrides,
                scope,
            },
            work,
        );
    } else if let Some(sup) = ctx.supervision_mut() {
        sup.pending_group = Some(GroupPhase::Stopping(GroupRestart {
            awaiting,
            stop_queue,
            head_reached: false,
            restart_order,
            manual_overrides: overrides,
            scope,
        }));
        sup.note_group();
        advance_stop_queue(sup);
    }
    // The ledger and the group hold the whole plan: the factories the
    // absorption removed are dropped now, before any callback. On a panic,
    // the failed attempt's end and the absorbed reports stay owed, in order.
    if let Some(err) = drop_removed(ctx, removed) {
        owe_deliveries(ctx, deliveries);
        return ControlFlow::Break(StopReason::Failure(err));
    }
    deliver_absorbed(actor, ctx, deliveries).await
}

/// Starts the sequential restart chain for a completed group: sets the
/// Restarting phase and hands the front of the queue to
/// [`advance_restart_chain`], which skips any member already fresher-`Running`
/// before this ever initiates a first attempt (sequential registration, OTP
/// left-to-right). An empty order (or one where every member turns out to
/// already be fresh) just drains the queued triggers.
fn start_restart_chain<A: Actor>(
    ctx: &mut ActorContext<A>,
    start: ChainStart,
    work: &mut VecDeque<ChildStoppedInternal>,
) {
    let queue: VecDeque<ActorId> = start.order.into();
    let step = if let Some(sup) = ctx.supervision_mut() {
        sup.pending_group = Some(GroupPhase::Restarting {
            queue,
            overrides: start.overrides,
            started: HashMap::new(),
            scope: start.scope,
        });
        sup.note_group();
        advance_restart_chain(sup, work)
    } else {
        ChainStep::Done
    };
    if let ChainStep::Initiate(front) = step {
        initiate_restart(ctx, &front, Origin::Chain);
    }
}

/// What the supervisor does with the spec of the child `ev` reports, when the
/// report is of an incarnation whose start was ignored: `None` for any other
/// end. The fact is read from the report itself, set from the incarnation's
/// own end, never from a ledger entry that may have moved past it; an entry
/// that is gone has nothing left to settle.
fn ignored_start_end<A: Actor>(
    ctx: &ActorContext<A>,
    ev: &ChildStoppedInternal,
) -> Option<IgnoredEnd> {
    if !ev.ignored {
        return None;
    }
    Some(
        ctx.supervision_ref()
            .and_then(|sup| {
                let child = sup.registry.get(&ev.child_id)?;
                Some(rules::ignored_end(
                    sup.config.strategy,
                    child.spec.restart_type,
                ))
            })
            .unwrap_or(IgnoredEnd::KeepDown),
    )
}

/// Settles the end of an ignored start in the ledger: the entry stays down
/// with nothing owed (`KeepDown`) or its spec is removed (`Remove`).
/// Returns the removed factory, for the caller to drop after its last write.
#[must_use = "the factory is dropped by the caller, after its last write"]
fn settle_ignored_end<A: Actor>(
    ctx: &mut ActorContext<A>,
    ev: &ChildStoppedInternal,
    end: IgnoredEnd,
) -> Option<RestartFn> {
    let sup = ctx.supervision_mut()?;
    match end {
        IgnoredEnd::KeepDown => {
            if let Some(child) = sup.registry.get_mut(&ev.child_id) {
                child.transition_expected(ChildLifecycle::Down {
                    incarnation: ev.incarnation,
                    pending: DownPending::None,
                });
            }
            None
        }
        IgnoredEnd::Remove => sup.remove_spec(&ev.child_id),
    }
}

/// What the ledger part of an ignored attempt's end leaves for the run loop
/// to carry out.
pub(super) struct IgnoredSettle {
    /// The removed spec's factory, for the caller to drop after its last
    /// write.
    removed: Option<RestartFn>,
    /// The group's stop phase emptied: the chain starts from this.
    begin_chain: Option<ChainStart>,
    /// The chain's next step when the member was its front.
    step: ChainStep,
    /// The reports the chain released.
    released: VecDeque<ChildStoppedInternal>,
}

/// The ledger part of the end of an attempt whose incarnation's start was
/// ignored: the entry reaches `Down` with nothing owed (kept) or the spec is
/// removed by `ignored_end`, the group's barrier settles its wait on the
/// member, and a chain whose front it was moves past it. Nothing is charged,
/// restarted, reported or delivered. An entry that does not hold the
/// attempt is left as it is.
pub(super) fn settle_ignored_attempt(
    sup: &mut SupervisionState,
    child_id: &ActorId,
    incarnation: u64,
) -> IgnoredSettle {
    let mut settle = IgnoredSettle {
        removed: None,
        begin_chain: None,
        step: ChainStep::Hold,
        released: VecDeque::new(),
    };
    sup.settle_attempt(child_id, incarnation, || AttemptOutcome::Abandoned);
    let end = sup.registry.get(child_id).and_then(|child| {
        matches!(
            child.lifecycle,
            ChildLifecycle::Restarting { next, .. } if next == incarnation
        )
        .then(|| rules::ignored_end(sup.config.strategy, child.spec.restart_type))
    });
    let Some(end) = end else {
        return settle;
    };
    // The incarnation was spawned: the next start for the child waits for
    // its subtree.
    if let Some(child) = sup.registry.get_mut(child_id) {
        child.note_subtree();
    }
    let was_front = matches!(
        sup.pending_group.as_ref(),
        Some(GroupPhase::Restarting { queue, started, .. })
            if queue.front() == Some(child_id)
                && started.get(child_id) == Some(&incarnation)
    );
    record_cancelled(sup, child_id, incarnation);
    match end {
        IgnoredEnd::KeepDown => {
            if was_front {
                if let Some(GroupPhase::Restarting { queue, .. }) = sup.pending_group.as_mut() {
                    queue.pop_front();
                }
            }
        }
        IgnoredEnd::Remove => settle.removed = sup.remove_spec(child_id),
    }
    if settle_barrier(sup, child_id, incarnation) {
        settle.begin_chain = advance_group_stop(sup);
    } else if was_front {
        settle.step = advance_restart_chain(sup, &mut settle.released);
    }
    settle
}

/// Carries out an [`IgnoredSettle`]: starts the chain or initiates its next
/// member and drops the removed factory. Returns the reports the chain
/// released, for the caller to handle; a factory drop panic ends the turn,
/// with those reports left owed.
fn carry_out_ignored<A: Actor>(
    ctx: &mut ActorContext<A>,
    settle: IgnoredSettle,
) -> Result<VecDeque<ChildStoppedInternal>, StopReason> {
    let IgnoredSettle {
        removed,
        begin_chain,
        step,
        mut released,
    } = settle;
    if let Some(start) = begin_chain {
        start_restart_chain(ctx, start, &mut released);
    } else if let ChainStep::Initiate(next) = step {
        initiate_restart(ctx, &next, Origin::Chain);
    }
    if let Some(err) = drop_removed(ctx, removed) {
        if let Some(sup) = ctx.supervision_mut() {
            sup.owed.extend(released);
        }
        return Err(StopReason::Failure(err));
    }
    Ok(released)
}

/// Handles an attempt whose incarnation's start was ignored: no charge, no
/// restart, no report, and a group's chain goes on to its next member, as
/// after an adoption.
async fn handle_ignored_attempt<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    child_id: ActorId,
    incarnation: u64,
) -> ControlFlow<StopReason> {
    let Some(sup) = ctx.supervision_mut() else {
        return ControlFlow::Continue(());
    };
    let settle = settle_ignored_attempt(sup, &child_id, incarnation);
    match carry_out_ignored(ctx, settle) {
        Ok(released) => handle_released(actor, ctx, released).await,
        Err(reason) => ControlFlow::Break(reason),
    }
}

/// Dispatches one restart attempt's outcome: an [`RestartOutcome::Adopted`]
/// value is handed to [`adopt_restart`]; a [`RestartOutcome::Failed`] one is
/// reported through the ordinary child-death path - budget-charged strategy
/// evaluation, exactly like any other failure - by synthesizing the same
/// [`ChildStoppedInternal`] event a runner completion would have produced.
pub(super) async fn handle_restart_outcome<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    outcome: RestartOutcome,
) -> ControlFlow<StopReason> {
    let stopped = matches!(outcome, RestartOutcome::Stopped { .. });
    match outcome {
        RestartOutcome::Adopted {
            child_id,
            incarnation,
            new_stop_lane,
            new_join,
            guard,
            io,
            ended,
        } => {
            adopt_restart(
                actor,
                ctx,
                child_id,
                incarnation,
                new_stop_lane,
                new_join,
                ended,
                guard,
                io,
            )
            .await
        }
        RestartOutcome::Failed {
            child_id,
            incarnation,
            reason,
        }
        | RestartOutcome::Stopped {
            child_id,
            incarnation,
            reason,
        } => {
            if let Some(sup) = ctx.supervision_mut() {
                sup.settle_attempt(&child_id, incarnation, || {
                    if stopped {
                        AttemptOutcome::Abandoned
                    } else {
                        AttemptOutcome::Failed {
                            reason: reason.clone(),
                        }
                    }
                });
            }
            // An attempt that failed to start is a `start_error` of the
            // supervisor; one that ended under a stop order is not.
            if cfg!(feature = "tracing") && !stopped {
                let sensitive = ctx
                    .supervision_ref()
                    .and_then(|sup| sup.registry.get(&child_id))
                    .is_some_and(|child| child.spec.sys.sensitive);
                reports::start_error(
                    ctx.actor_id(),
                    &child_id,
                    Some(incarnation),
                    || reason.clone(),
                    sensitive,
                );
            }
            // An attempt's incarnation that ended under a stop order is
            // reported like a failed one: the end of the incarnation the
            // ledger holds as the attempt's.
            // An attempt that ended by an ignored start is reported as
            // ignored, never as a failure, so this end is none.
            let event = ChildStoppedInternal {
                child_id,
                reason,
                incarnation,
                ignored: false,
            };
            handle_child_stopped_with(actor, ctx, event, false).await
        }
        RestartOutcome::Ignored {
            child_id,
            incarnation,
        } => handle_ignored_attempt(actor, ctx, child_id, incarnation).await,
        RestartOutcome::Cancelled {
            child_id,
            incarnation,
            drop_panic,
        } => {
            // The attempt honored a stop order or the group's cancel before its
            // factory ran (or while it waited for the previous subtree), or
            // found the spec removed: no incarnation existed, and nothing is
            // delivered.
            let mut begin_chain = None;
            if let Some(sup) = ctx.supervision_mut() {
                sup.settle_attempt(&child_id, incarnation, || AttemptOutcome::Abandoned);
                record_cancelled(sup, &child_id, incarnation);
                if settle_barrier(sup, &child_id, incarnation) {
                    begin_chain = advance_group_stop(sup);
                }
            }
            let chained = begin_chain.is_some();
            let mut released = VecDeque::new();
            if let Some(start) = begin_chain {
                start_restart_chain(ctx, start, &mut released);
            }
            // The factory's last reference dropped in the attempt, and its
            // drop panicked: a panic of the turn, recorded as a handler's is,
            // once the outcome is recorded. The reports the chain released
            // stay owed, in order.
            if let Some(payload) = drop_panic {
                let err = ActorError::Panic(payload);
                ctx.record_failure(err.clone());
                if let Some(sup) = ctx.supervision_mut() {
                    sup.owed.extend(released);
                }
                return ControlFlow::Break(StopReason::Failure(err));
            }
            if !chained {
                return ControlFlow::Continue(());
            }
            handle_released(actor, ctx, released).await
        }
        RestartOutcome::PredecessorUnresponsive {
            child_id,
            incarnation,
        } => {
            // The attempt's wait for the previous incarnation's subtree
            // outlived its bound: no incarnation existed, so the ledger entry
            // is down with nothing owed, and nothing is charged, delivered or
            // evaluated. The supervisor leaves its loop with the failure, so
            // its own supervisor decides.
            let err: ActorError = SupervisionError::ChildUnresponsive(child_id.clone()).into();
            if let Some(sup) = ctx.supervision_mut() {
                sup.settle_attempt(&child_id, incarnation, || AttemptOutcome::Failed {
                    reason: StopReason::Failure(err.clone()),
                });
                record_cancelled(sup, &child_id, incarnation);
            }
            fail_with(ctx, err)
        }
    }
}

/// Records the end of an attempt that never produced an incarnation: the
/// ledger entry the attempt held reaches `Down` with nothing owed.
fn record_cancelled(sup: &mut SupervisionState, child_id: &ActorId, incarnation: u64) {
    if let Some(child) = sup.registry.get_mut(child_id) {
        if matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == incarnation)
        {
            child.transition_expected(ChildLifecycle::Down {
                incarnation,
                pending: DownPending::None,
            });
        }
    }
}

/// Adopts a restart attempt's [`RestartOutcome::Adopted`] value: validates
/// the incarnation token, starts the fresh instance's runner on the slot the
/// attempt hands over (the guard itself was already armed inside the restart
/// task, the instant its `spawn_actor` call succeeded; a stop order written
/// while the outcome was on its way is honored by the runner at once),
/// advances a sequential group-restart
/// chain, and - when the chain completes - processes the failure events that
/// were queued while the group was in flight.
///
/// A member adopted while the group is still in its `Stopping` phase is one
/// whose attempt the group committed when it was evaluated. Its barrier entry
/// names the attempt's incarnation already, and the adoption writes no order:
/// when the stop queue reached the member the slot holds the queue's order
/// and the runner honors it at once, and otherwise the queue orders the
/// adopted incarnation when its turn comes, so it is stopped and started
/// again in its slot, never kept.
#[allow(clippy::too_many_arguments)]
async fn adopt_restart<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    child_id: ActorId,
    seq: u64,
    new_stop_lane: StopLane,
    new_join: JoinHandle<StopReason>,
    ended: Option<StopReason>,
    guard: ChildLinkGuard,
    io: SlotIo,
) -> ControlFlow<StopReason> {
    let accepted = ctx
        .supervision_ref()
        .and_then(|sup| sup.registry.get(&child_id))
        .map(|child| matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == seq))
        .unwrap_or(false);
    // A slot leaves `Restarting` only through its own attempt's outcome, and
    // no report about the attempt's incarnation exists before its adoption,
    // so an outcome that finds another entry is not expected.
    debug_assert!(
        accepted,
        "an adoption concerns the attempt the ledger holds"
    );
    if let Some(sup) = ctx.supervision_mut() {
        // The runner's claim comes before the attempt settles: an adopted
        // incarnation that already ended is owed from here, so the attempt
        // in flight and the death owed are never both absent.
        if accepted {
            claim_death(
                sup.registry.owner_record().as_ref(),
                &child_id,
                seq,
                new_stop_lane.abort_record(),
            );
        }
        sup.settle_attempt(&child_id, seq, || {
            if accepted {
                AttemptOutcome::Adopted
            } else {
                AttemptOutcome::Abandoned
            }
        });
        #[cfg(test)]
        end_hooks::run(end_hooks::SETTLED, &child_id);
    }
    if !accepted {
        // Superseded restart (a newer seq is pending or the spec is gone):
        // dropping the guard kills the stray instance through the ordinary
        // Kill-raise/reaper ladder instead of leaking a
        // duplicate. Its death event would carry a stale incarnation and be
        // ignored anyway.
        drop(guard);
        drop(new_join);
        return ControlFlow::Continue(());
    }
    // A restart the supervisor decided is reported at info, one the caller
    // asked for with `restart_child` at debug.
    if cfg!(feature = "tracing") {
        let by_hand = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&child_id))
            .is_some_and(|child| child.origin == Origin::Manual);
        reports::progress(
            ctx.actor_id(),
            &child_id,
            seq,
            if by_hand {
                reports::Progress::Started
            } else {
                reports::Progress::Restarted
            },
        );
    }

    let mut step = ChainStep::Hold;
    let mut drained: VecDeque<ChildStoppedInternal> = VecDeque::new();
    if let Some(sup) = ctx.supervision_mut() {
        // The kill grace of the child's spec: the one its link guard was
        // built with.
        let kill_grace = sup
            .registry
            .get(&child_id)
            .map_or(DEFAULT_KILL_GRACE, |child| child.spec.kill_grace);
        let owner = sup.registry.owner_record();
        let task = spawn_runner(
            &mut sup.death_set,
            child_id.clone(),
            seq,
            new_join,
            ended,
            new_stop_lane,
            io,
            guard,
            kill_grace,
            owner,
        );
        sup.task_owner
            .insert(task.id(), (child_id.clone(), seq, TaskPlane::Death));
        sup.registry.update_restarted(&child_id, seq);
        if let Some(child) = sup.registry.get_mut(&child_id) {
            child.note_subtree();
        }

        let occupant = sup
            .registry
            .get(&child_id)
            .and_then(|child| child.lifecycle.occupant());
        let folds = matches!(
            sup.pending_group.as_ref(),
            Some(GroupPhase::Stopping(group)) if group.restart_order.contains(&child_id)
        );
        let is_front = matches!(
            sup.pending_group.as_ref(),
            Some(GroupPhase::Restarting { queue, .. }) if queue.front() == Some(&child_id)
        );
        if folds {
            if let (Some(GroupPhase::Stopping(group)), Some(occupant)) =
                (sup.pending_group.as_mut(), occupant)
            {
                // The group committed the attempt when it was evaluated, so
                // its barrier entry names this very incarnation already.
                debug_assert!(
                    group.awaiting.get(&child_id) == Some(&occupant),
                    "an attempt adopted during the stop phase was committed to the group"
                );
                group.awaiting.insert(child_id.clone(), occupant);
            }
        } else if is_front {
            step = advance_restart_chain(sup, &mut drained);
        }
    }

    if let ChainStep::Initiate(next) = step {
        initiate_restart(ctx, &next, Origin::Chain);
    }
    handle_released(actor, ctx, drained).await
}

/// Records one restart-plane outcome during the supervisor's teardown or its
/// final collection: nothing is delivered, charged or restarted. An adopted
/// attempt becomes the child's running incarnation (its runner honors the
/// order the cell holds, if any); every other outcome puts the ledger entry
/// the attempt held down. A cancelled attempt's factory drop panic is kept in
/// `drop_panic`, after any earlier one, as a drop before the reason is final.
/// Returns the child and incarnation the outcome concerned, whether the
/// attempt was adopted, and the end it reported, if any.
pub(super) fn record_restart_outcome(
    sup: &mut SupervisionState,
    supervisor: &Owner,
    outcome: RestartOutcome,
    warn: bool,
    drop_panic: &mut Option<String>,
) -> (ActorId, u64, Option<AdoptedRunner>, Option<StopReason>) {
    let stopped = matches!(outcome, RestartOutcome::Stopped { .. });
    match outcome {
        RestartOutcome::Adopted {
            child_id,
            incarnation,
            new_stop_lane,
            new_join,
            guard,
            io,
            ended,
        } => {
            let accepted = sup.registry.get(&child_id).is_some_and(|child| {
                matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == incarnation)
            });
            // Claimed before the settle, as an adoption in the loop is.
            if accepted {
                claim_death(
                    sup.registry.owner_record().as_ref(),
                    &child_id,
                    incarnation,
                    new_stop_lane.abort_record(),
                );
            }
            sup.settle_attempt(&child_id, incarnation, || {
                if accepted {
                    AttemptOutcome::Adopted
                } else {
                    AttemptOutcome::Abandoned
                }
            });
            #[cfg(test)]
            end_hooks::run(end_hooks::SETTLED, &child_id);
            if !accepted {
                // Superseded: dropping the guard kills the stray
                // incarnation through the ordinary ladder.
                drop(guard);
                drop(new_join);
                return (child_id, incarnation, None, None);
            }
            let kill_grace = sup
                .registry
                .get(&child_id)
                .map_or(DEFAULT_KILL_GRACE, |child| child.spec.kill_grace);
            let owner = sup.registry.owner_record();
            let task = spawn_runner(
                &mut sup.death_set,
                child_id.clone(),
                incarnation,
                new_join,
                ended,
                new_stop_lane,
                io,
                guard,
                kill_grace,
                owner,
            );
            sup.task_owner
                .insert(task.id(), (child_id.clone(), incarnation, TaskPlane::Death));
            sup.registry.update_restarted(&child_id, incarnation);
            if let Some(child) = sup.registry.get_mut(&child_id) {
                child.note_subtree();
            }
            let ended = sup
                .registry
                .get(&child_id)
                .is_some_and(|child| child.ctl.status_ended());
            (
                child_id,
                incarnation,
                Some(AdoptedRunner {
                    task: task.id(),
                    ended,
                }),
                None,
            )
        }
        RestartOutcome::Failed {
            child_id,
            incarnation,
            reason,
        }
        | RestartOutcome::Stopped {
            child_id,
            incarnation,
            reason,
        } => {
            sup.settle_attempt(&child_id, incarnation, || {
                if stopped {
                    AttemptOutcome::Abandoned
                } else {
                    AttemptOutcome::Failed {
                        reason: reason.clone(),
                    }
                }
            });
            if let (true, Some(child)) = (warn, sup.registry.get(&child_id)) {
                report_unexpected_end(
                    supervisor,
                    &child_id,
                    incarnation,
                    &reason,
                    child.spec.restart_type,
                    child.spec.shutdown,
                    child.spec.sys.sensitive,
                );
            }
            (child_id, incarnation, None, Some(reason))
        }
        RestartOutcome::Ignored {
            child_id,
            incarnation,
        } => {
            // An ignored start is no end to report: the entry is down with
            // nothing owed, as for an attempt that spawned nothing.
            sup.settle_attempt(&child_id, incarnation, || AttemptOutcome::Abandoned);
            record_cancelled(sup, &child_id, incarnation);
            (child_id, incarnation, None, None)
        }
        RestartOutcome::Cancelled {
            child_id,
            incarnation,
            drop_panic: panic,
        } => {
            sup.settle_attempt(&child_id, incarnation, || AttemptOutcome::Abandoned);
            record_cancelled(sup, &child_id, incarnation);
            keep_first_of(drop_panic, panic, supervisor);
            (child_id, incarnation, None, None)
        }
        RestartOutcome::PredecessorUnresponsive {
            child_id,
            incarnation,
        } => {
            sup.settle_attempt(&child_id, incarnation, || AttemptOutcome::Failed {
                reason: StopReason::Failure(
                    SupervisionError::ChildUnresponsive(child_id.clone()).into(),
                ),
            });
            record_cancelled(sup, &child_id, incarnation);
            (child_id, incarnation, None, None)
        }
    }
}

/// The runner an adoption spawned: its task, and whether the incarnation's
/// task had already ended when it was adopted.
pub(super) struct AdoptedRunner {
    pub(super) task: tokio::task::Id,
    pub(super) ended: bool,
}

/// Records, with no delivery, that an incarnation's end was collected: the
/// ledger entry that held it reaches `Down` with nothing owed, a pending
/// snapshot of the end is dropped (the collected report keeps its own
/// reason), and a barrier entry for it is released.
pub(super) fn record_collected_end(
    sup: &mut SupervisionState,
    child_id: &ActorId,
    incarnation: u64,
) {
    if let Some(child) = sup.registry.get_mut(child_id) {
        let held = match child.lifecycle {
            ChildLifecycle::Running(n) | ChildLifecycle::Stopping { incarnation: n, .. } => n,
            ChildLifecycle::Restarting { next, .. } => next,
            ChildLifecycle::Down { incarnation: n, .. } => n,
        };
        if held == incarnation {
            child.transition_expected(ChildLifecycle::Down {
                incarnation,
                pending: DownPending::None,
            });
        }
    }
    sup.pending_manual_events
        .remove(&(child_id.clone(), incarnation));
    if let Some(GroupPhase::Stopping(group)) = sup.pending_group.as_mut() {
        if group.awaiting.get(child_id) == Some(&incarnation) {
            group.awaiting.remove(child_id);
        }
    }
}
