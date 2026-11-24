//! Group stops and owed records: the stop queue, barriers, absorbed records and verdict checks.

use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;

use super::{
    deliver_child_event, end_value, handle_worklist, keep_first_of, take_chain_order, ChainStart,
};
use crate::actor::context::ActorContext;
use crate::actor::supervision::{
    rules, ChildLifecycle, ChildState, DownPending, GroupPhase, GroupRestart, Origin, RestartFn,
    SupervisionState, TaskState,
};
use crate::actor::Actor;
use crate::error::{ActorError, SupervisionError};
use crate::types::{
    ActorId, ActorStatus, ChildEvent, ChildStoppedInternal, RestartType, Shutdown, StopReason,
    SupervisionAction,
};

/// Handles the reports a completing chain released as one worklist: an
/// evaluation among them finds the others still waiting in it. When a
/// delivery stops the supervisor, the reports not yet handled stay owed for
/// its final collection.
pub(super) async fn handle_released<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    mut released: VecDeque<ChildStoppedInternal>,
) -> ControlFlow<StopReason> {
    let flow = handle_worklist(actor, ctx, None, &mut released).await;
    if flow.is_break() {
        if let Some(sup) = ctx.supervision_mut() {
            sup.owed.extend(released);
        }
    }
    flow
}

/// Commits a group restart's members to its stop phase, from the commit list
/// of the evaluation or of a retry's plan: the barrier holds every member
/// with an occupant at that occupant's incarnation, the stop queue holds the
/// same members in reverse start order, and the restart attempt of a member
/// that has not spawned its actor yet is cancelled at once. Nothing is
/// stopped here: the queue stops one member at a time, each in its turn.
pub(super) fn plan_group_stop(
    sup: &SupervisionState,
    commit: &[(ActorId, u64, rules::MemberAction)],
) -> (HashMap<ActorId, u64>, VecDeque<ActorId>) {
    let awaiting = commit
        .iter()
        .filter(|(_, _, action)| action.held)
        .map(|(id, token, _)| (id.clone(), *token))
        .collect();
    let stop_queue = commit
        .iter()
        .rev()
        .filter(|(_, _, action)| action.held)
        .map(|(id, _, _)| id.clone())
        .collect();
    for (id, _, action) in commit {
        if action.cancel {
            if let Some(child) = sup.registry.get(id) {
                child.ctl.cancel_unspawned();
            }
        }
    }
    (awaiting, stop_queue)
}

/// What the stop queue reads of its head: the barrier's entry, the slot's
/// order and request state, and whether a completed manual stop classified
/// the head's end already. The ledger entry and the slot count only when they
/// hold the incarnation the barrier names: the end of an incarnation the key
/// has moved past is waited for, and the key's new occupant is no member.
pub(crate) fn queue_head(
    sup: &SupervisionState,
    group: &GroupRestart,
    id: &ActorId,
) -> rules::QueueHead {
    let awaited = group.awaiting.get(id).copied();
    let child = sup.registry.get(id);
    let held = awaited.zip(child);
    let slot = held.and_then(|(token, child)| child.slot_of(token).map(|ctl| (child, ctl)));
    rules::QueueHead {
        awaited: awaited.is_some(),
        verdict: slot.is_some_and(|(_, ctl)| ctl.is_unresponsive()),
        signalled: group.head_reached,
        handled: awaited
            .is_some_and(|token| sup.pending_manual_events.contains_key(&(id.clone(), token))),
        life: slot.map(|(child, _)| child.lifecycle),
        has_order: slot.is_some_and(|(_, ctl)| ctl.has_order()),
        ask_possible: slot.is_some_and(|(child, ctl)| {
            child.spec.shutdown == Shutdown::Infinity
                && ctl.lane_vetoable()
                && ctl.task_state() != TaskState::Ended
        }),
        reasked: slot.is_some_and(|(_, ctl)| ctl.reasked()),
        attempt_past_init: slot.is_some_and(|(child, ctl)| {
            matches!(child.lifecycle, ChildLifecycle::Restarting { .. })
                && ctl
                    .status()
                    .is_some_and(|status| status.borrow().status != ActorStatus::Initializing)
        }),
        attempt_spawned: slot.is_none_or(|(child, ctl)| {
            !matches!(child.lifecycle, ChildLifecycle::Restarting { .. }) || ctl.lane().is_some()
        }),
    }
}

/// The group restart's stop queue, one member at a time. The head is decided
/// from its state now ([`rules::stop_queue_next`]): a member whose end was
/// delivered, who carries the Unresponsive verdict or whose end a completed
/// manual stop classified is passed over; a running member is ordered to stop
/// under its own `Shutdown` policy, a member whose stop is in progress is
/// joined (never given a second ladder, and asked once more when it is a
/// vetoing `Infinity` member that nobody asked since), and a member whose
/// restart attempt is in flight has the attempt ordered to stop. Each
/// member's runner raises the first signal, escalates at the policy's
/// deadline to `Kill` and then to an abort, and records the member's end; the
/// queue reads the next member only when that end, or a verdict, is recorded.
/// Nothing here waits or arms a timer.
pub(super) fn advance_stop_queue(sup: &mut SupervisionState) {
    loop {
        let Some(GroupPhase::Stopping(group)) = sup.pending_group.as_ref() else {
            return;
        };
        let Some(head) = group.stop_queue.front().cloned() else {
            return;
        };
        let step = rules::stop_queue_next(&queue_head(sup, group, &head));
        if step == rules::QueueStep::Wait {
            return;
        }
        if let Some(child) = sup.registry.get(&head) {
            match step {
                rules::QueueStep::Order => child.ctl.order_stop(child.spec.shutdown, true),
                rules::QueueStep::OrderUnpublished => {
                    child.ctl.order_stop(child.spec.shutdown, false);
                }
                rules::QueueStep::OrderAttempt { ask } => {
                    child.ctl.order_stop(child.spec.shutdown, true);
                    if ask {
                        child.ctl.ask_again();
                        child.ctl.mark_reasked();
                    }
                }
                rules::QueueStep::Join { ask } => {
                    if ask {
                        child.ctl.ask_again();
                        child.ctl.mark_reasked();
                    }
                }
                rules::QueueStep::Pop | rules::QueueStep::Wait | rules::QueueStep::Await => {}
            }
        }
        if let Some(GroupPhase::Stopping(group)) = sup.pending_group.as_mut() {
            let reached = rules::queue_head_reached_after(step, group.head_reached);
            if step == rules::QueueStep::Pop {
                group.stop_queue.pop_front();
            }
            // A signalled head's next read says to wait.
            group.head_reached = reached;
        }
    }
}

/// Marks the incarnation of `child_id` the stop phase awaits as ended.
/// Returns `false` when no stop phase is pending or it awaits another
/// incarnation.
pub(super) fn settle_barrier(
    sup: &mut SupervisionState,
    child_id: &ActorId,
    incarnation: u64,
) -> bool {
    match sup.pending_group.as_mut() {
        Some(GroupPhase::Stopping(group)) => group.settle_awaited(child_id, incarnation),
        _ => false,
    }
}

/// The stop phase's step after a barrier entry was settled or a verdict was
/// given: the queue goes on to the next member, and the chain starts, with
/// the restart order the group kept, once the queue and the barrier are both
/// empty.
pub(super) fn advance_group_stop(sup: &mut SupervisionState) -> Option<ChainStart> {
    advance_stop_queue(sup);
    let finished = matches!(
        sup.pending_group.as_ref(),
        Some(GroupPhase::Stopping(group))
            if group.stop_queue.is_empty() && group.awaiting.is_empty()
    );
    if finished {
        take_chain_order(sup)
    } else {
        None
    }
}

/// Absorbs the owed evaluations of a group's members into the group, before
/// the group's stops are carried out: a member's end belongs to its group, so
/// it is neither evaluated nor charged. See [`absorb_records`] for what is
/// recorded; the deliveries it names are made here, in order.
pub(super) async fn absorb_owed<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    work: &mut VecDeque<ChildStoppedInternal>,
    commit: &[(ActorId, u64, rules::MemberAction)],
) -> ControlFlow<StopReason> {
    let (deliveries, removed) = match ctx.supervision_mut() {
        Some(sup) => absorb_records(sup, work, commit),
        None => (VecDeque::new(), Vec::new()),
    };
    // The factories the absorption removed are dropped once it is recorded,
    // before any delivery. On a panic, the absorbed reports stay owed.
    if let Some(err) = drop_removed(ctx, removed) {
        owe_deliveries(ctx, deliveries);
        return ControlFlow::Break(StopReason::Failure(err));
    }
    deliver_absorbed(actor, ctx, deliveries).await
}

/// Records the absorption of every owed evaluation of a group's members, and
/// returns the deliveries it owes, each with the incarnation it concerns. A
/// report waiting in the deferred triggers or in the worklist is removed, the
/// member is recorded down, and the report is delivered once as the group's
/// restart of the member. A report that has not reached the loop yet (a
/// bounce a `stop_child` met a crash on completed inline) leaves a snapshot of
/// the end that the report delivers once when it drains. A completed manual
/// stop's snapshot that says the member stays down is rewritten, for a member
/// the group restarts, to say the group restarts it. A `Temporary` member is
/// not restarted whatever its report says: its spec is removed and its end is
/// reported removed ([`rules::group_member_end`]). The factories of the
/// removed specs are returned, for the caller to drop after its last write.
pub(super) fn absorb_records(
    sup: &mut SupervisionState,
    work: &mut VecDeque<ChildStoppedInternal>,
    commit: &[(ActorId, u64, rules::MemberAction)],
) -> (VecDeque<(ChildEvent, u64)>, Vec<RestartFn>) {
    let mut deliveries: VecDeque<(ChildEvent, u64)> = VecDeque::new();
    let mut removed: Vec<RestartFn> = Vec::new();
    for (id, _, action) in commit {
        let Some(child) = sup.registry.get(id) else {
            continue;
        };
        let life = child.lifecycle;
        let name = child.name.as_deref().map(str::to_string);
        let prune =
            rules::group_member_end(child.spec.restart_type) == rules::MemberEnd::PruneAndRemove;
        let ChildLifecycle::Down { incarnation: n, .. } = life else {
            continue;
        };
        let queued = sup
            .queued_triggers
            .iter()
            .position(|ev| &ev.child_id == id && ev.incarnation == n);
        let worklist = work
            .iter()
            .position(|ev| &ev.child_id == id && ev.incarnation == n);
        let place = if queued.is_some() {
            rules::ReportPlace::Queued
        } else if worklist.is_some() {
            rules::ReportPlace::Worklist
        } else {
            rules::ReportPlace::DeathPlane
        };
        match rules::absorbable(place, true, Some(life)) {
            rules::Absorption::Keep => {
                // A completed manual stop that kept the member down rejoins
                // the restart the group makes.
                if action.restarts {
                    if let Some(snapshot) = sup.pending_manual_events.get_mut(&(id.clone(), n)) {
                        if snapshot.action == SupervisionAction::Removed {
                            snapshot.action = SupervisionAction::RestartInitiated;
                        }
                    }
                }
            }
            rules::Absorption::Deliver => {
                let report = match (queued, worklist) {
                    (Some(i), _) => sup.queued_triggers.remove(i),
                    (None, Some(i)) => work.remove(i),
                    (None, None) => None,
                };
                if let Some(report) = report {
                    // Absorbed into the group's restart: no decision of its
                    // own is owed.
                    sup.death_decided(report.incarnation);
                    if let Some(child) = sup.registry.get_mut(id) {
                        child.transition_expected(ChildLifecycle::Down {
                            incarnation: n,
                            pending: DownPending::None,
                        });
                    }
                    if prune {
                        removed.extend(sup.remove_spec(id));
                    }
                    #[cfg(feature = "tracing")]
                    caught_log!(debug, target: "tokio_actors::actor::runtime",
                        child = %id,
                        incarnation = n,
                        waited_in = report_place_words(place),
                        "an owed report of a group member was absorbed into the group restart"
                    );
                    deliveries.push_back((
                        ChildEvent {
                            child_id: id.clone(),
                            child_name: name,
                            reason: report.reason,
                            action: if prune {
                                SupervisionAction::Removed
                            } else {
                                SupervisionAction::RestartInitiated
                            },
                        },
                        n,
                    ));
                }
            }
            rules::Absorption::Snapshot => {
                let reason = sup
                    .registry
                    .get(id)
                    .map_or(StopReason::Kill, |child| child.ctl.end_reason());
                if let Some(child) = sup.registry.get_mut(id) {
                    child.transition_expected(ChildLifecycle::Down {
                        incarnation: n,
                        pending: DownPending::Snapshot,
                    });
                }
                if prune {
                    removed.extend(sup.remove_spec(id));
                }
                #[cfg(feature = "tracing")]
                caught_log!(debug, target: "tokio_actors::actor::runtime",
                    child = %id,
                    incarnation = n,
                    waited_in = report_place_words(place),
                    "the end of a group member that owes a report was recorded, to be delivered when the report arrives"
                );
                sup.pending_manual_events.insert(
                    (id.clone(), n),
                    ChildEvent {
                        child_id: id.clone(),
                        child_name: name,
                        reason,
                        action: if prune {
                            SupervisionAction::Removed
                        } else {
                            SupervisionAction::RestartInitiated
                        },
                    },
                );
            }
        }
    }
    (deliveries, removed)
}

/// Where an absorbed report waited, in words for a log line.
#[cfg(feature = "tracing")]
fn report_place_words(place: rules::ReportPlace) -> &'static str {
    match place {
        rules::ReportPlace::Queued => "deferred behind the group",
        rules::ReportPlace::Worklist => "worklist",
        rules::ReportPlace::DeathPlane => "not yet drained",
    }
}

/// Makes the deliveries [`absorb_records`] owes, in order. When a delivery
/// stops the supervisor, the ones that did not run stay owed: its final
/// collection delivers them.
pub(super) async fn deliver_absorbed<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    mut deliveries: VecDeque<(ChildEvent, u64)>,
) -> ControlFlow<StopReason> {
    while let Some((event, incarnation)) = deliveries.pop_front() {
        if let ControlFlow::Break(reason) =
            deliver_child_event(actor, ctx, &event, incarnation).await
        {
            owe_deliveries(ctx, deliveries);
            return ControlFlow::Break(reason);
        }
    }
    ControlFlow::Continue(())
}

/// Prunes a child's spec after a non-restart outcome when OTP says so:
/// Temporary specs are deleted as soon as the process terminates, and
/// SimpleOneForOne removes dynamic specs likewise. A transient child stopped
/// cleanly keeps its spec (`Down`, the pid=undefined analog) for
/// restart_child. Returns the removed spec's factory, for the caller to drop
/// after its last write.
#[must_use = "the factory is dropped by the caller, after its last write"]
pub(crate) fn prune_spec_if_needed<A: Actor>(
    ctx: &mut ActorContext<A>,
    child_id: &ActorId,
) -> Option<RestartFn> {
    let sup = ctx.supervision_mut()?;
    let strategy = sup.config.strategy;
    let prune = sup
        .registry
        .get(child_id)
        .map_or(rules::is_pool(strategy), |c| {
            rules::spec_prunable(strategy, c.spec.restart_type)
        });
    if prune {
        sup.remove_spec(child_id)
    } else {
        None
    }
}

/// Drops the factories an operation of the run loop removed, once it has
/// made its last write. A factory is user code: each drop is caught, and a
/// panic is a panic of the turn, recorded as a handler's is and returned for
/// the turn to stop the supervisor with.
pub(super) fn drop_removed<A: Actor>(
    ctx: &mut ActorContext<A>,
    removed: impl IntoIterator<Item = RestartFn>,
) -> Option<ActorError> {
    let owner = ctx.owner();
    let mut first = None;
    for factory in removed {
        keep_first_of(&mut first, end_value(factory), &owner);
    }
    let err = ActorError::Panic(first?);
    ctx.record_failure(err.clone());
    Some(err)
}

/// Leaves a report the turn took and did not deliver for the final
/// deliveries, with the child's name.
pub(super) fn owe_report<A: Actor>(
    ctx: &mut ActorContext<A>,
    report: ChildStoppedInternal,
    name: Option<String>,
) {
    if let Some(sup) = ctx.supervision_mut() {
        sup.owe(report, name);
    }
}

/// Leaves undelivered deliveries for the final deliveries, in order, each
/// with the child's name.
pub(super) fn owe_deliveries<A: Actor>(
    ctx: &mut ActorContext<A>,
    deliveries: VecDeque<(ChildEvent, u64)>,
) {
    if let Some(sup) = ctx.supervision_mut() {
        for (event, incarnation) in deliveries {
            sup.owe(
                ChildStoppedInternal {
                    child_id: event.child_id,
                    reason: event.reason,
                    incarnation,
                },
                event.child_name,
            );
        }
    }
}

/// Initiates a non-blocking restart for a child using its stored restart
/// closure (which captured the child's id, name, and config by value).
pub(super) fn initiate_restart<A: Actor>(
    ctx: &mut ActorContext<A>,
    child_id: &ActorId,
    origin: Origin,
) {
    let seq = ctx.resolved_system().next_incarnation();
    ctx.initiate_attempt(child_id, seq, origin);
}

/// A pending group restart that can make progress only through members that
/// carry the Unresponsive verdict ends the supervisor: it records and stops
/// with `ChildUnresponsive` for the first such member in reverse start order,
/// so its own parent decides. So does a stop in progress whose occupant
/// carries the verdict when no caller waits on it or was told (`orphans`,
/// read when a verdict woke the loop): a `begin_stop_child`, a
/// `begin_terminate_child`, or an awaited stop whose caller dropped its
/// future. A verdict a caller waits for, or was told, is that caller's to
/// decide.
pub(super) fn check_verdicts<A: Actor>(
    ctx: &mut ActorContext<A>,
    orphans: bool,
) -> Option<StopReason> {
    let sup = ctx.supervision_ref()?;
    let id = group_blocked_verdict(sup).or_else(|| {
        if orphans {
            unreported_stop_verdict(sup)
        } else {
            None
        }
    })?;
    let err: ActorError = SupervisionError::ChildUnresponsive(id).into();
    ctx.record_failure(err.clone());
    Some(StopReason::Failure(err))
}

/// The member the pending group restart is blocked on, when it can make
/// progress only through members that carry the verdict.
///
/// In the stop phase the members are those the barrier still waits for; in
/// the chain phase it is the front member's own restart attempt.
fn group_blocked_verdict(sup: &SupervisionState) -> Option<ActorId> {
    let entries: Vec<(ActorId, bool)> = match sup.pending_group.as_ref()? {
        GroupPhase::Stopping(group) => {
            let mut entries: Vec<(ActorId, bool)> = sup
                .registry
                .iter()
                .filter_map(|child| {
                    let awaited = *group.awaiting.get(&child.id)?;
                    let verdict = child
                        .slot_of(awaited)
                        .is_some_and(|ctl| ctl.is_unresponsive());
                    Some((child.id.clone(), verdict))
                })
                .collect();
            entries.reverse();
            entries
        }
        GroupPhase::Restarting { queue, started, .. } => {
            let front = queue.front()?;
            let child = sup.registry.get(front)?;
            match child.lifecycle {
                ChildLifecycle::Restarting { next, .. } if started.get(front) == Some(&next) => {
                    vec![(front.clone(), child.ctl.is_unresponsive())]
                }
                _ => Vec::new(),
            }
        }
    };
    rules::barrier_blocked_only_by_verdicts(&entries)
}

/// The child, newest first, whose stop in progress carries the Unresponsive
/// verdict with no caller to report it.
fn unreported_stop_verdict(sup: &SupervisionState) -> Option<ActorId> {
    // Newest first, each entry read only when the scan reaches it.
    rules::unreported_verdict(sup.registry.iter().rev().map(|child| {
        let stopping = matches!(child.lifecycle, ChildLifecycle::Stopping { .. });
        (
            &child.id,
            rules::StopVerdictFact {
                stopping,
                verdict: stopping && child.ctl.is_unresponsive(),
                ended: child.ctl.fate().is_some() || child.ctl.status_ended(),
                audience: child.ctl.verdict_has_audience(),
            },
        )
    }))
}

/// What the teardown reads of a child when its turn comes.
pub(super) fn walk_child(child: &ChildState) -> rules::WalkChild {
    let ctl = &child.ctl;
    let ask_possible = child.spec.shutdown == Shutdown::Infinity
        && ctl.lane_vetoable()
        && ctl.task_state() != TaskState::Ended;
    rules::WalkChild {
        life: Some(child.lifecycle),
        has_order: ctl.has_order(),
        ask_possible,
        reasked: ctl.reasked(),
        attempt_past_init: ctl
            .status()
            .is_some_and(|status| status.borrow().status != ActorStatus::Initializing),
    }
}

/// Logs, at error level on the `tokio_actors::supervisor` target, the end of a
/// child its supervisor's teardown stopped, when the end is not an orderly
/// one. Changes no state and runs no callback.
#[allow(unused_variables)]
pub(super) fn report_unexpected_end(
    supervisor: &ActorId,
    child: &ActorId,
    incarnation: u64,
    reason: &StopReason,
    restart_type: RestartType,
    shutdown: Shutdown,
    sensitive: bool,
) {
    if !rules::teardown_end_is_orderly(reason, restart_type, shutdown) {
        #[cfg(feature = "tracing")]
        caught_log!(error,
            target: "tokio_actors::supervisor",
            report = "shutdown_error",
            supervisor = %supervisor,
            child = %child,
            incarnation,
            reason = %crate::telemetry::LogFields::of_sensitive(sensitive).reason_text(reason),
            "child ended unexpectedly while its supervisor stopped it"
        );
    }
}
