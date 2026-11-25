//! The last deliveries of a stopping actor: collected child ends and the notices still pending.

use std::collections::HashSet;
use std::ops::ControlFlow;

use tokio::sync::mpsc;

use super::{
    dispatch, emit_child_died, end_value, fail_with, kill_stored, log_late_drop,
    record_collected_end, record_restart_outcome, CellWait, Collected, FinalOutcome, HeldAnswer,
    HookOutcome, Owner, ReplyDrain, SettleSet,
};
use crate::actor::context::ActorContext;
use crate::actor::envelope::Envelope;
use crate::actor::incarnation::turn_clock::push_turn;
use crate::actor::incarnation::{LaneRx, TurnClock};
use crate::actor::link::InboxItem;
use crate::actor::mailbox::{MailRx, QueueRx};
use crate::actor::panic::{catch_hook_t, payload_into_string};
#[cfg(test)]
use crate::actor::runtime::end_hooks;
use crate::actor::supervision::{await_cell, RestartOutcome, SupervisionState};
use crate::actor::{Actor, ActorEnvelope};
use crate::error::ActorError;
use crate::telemetry::HookKind;
use crate::types::{
    ActorId, ChildEvent, ChildStoppedInternal, DownReason, LinkExit, MonitorId, StopReason,
    SupervisionAction,
};

/// Pushes one collected report, with the child's name as the ledger (or a
/// pending snapshot of the end) holds it, and records the end in the ledger.
fn collect_report(
    sup: &mut SupervisionState,
    out: &mut Vec<Collected>,
    report: ChildStoppedInternal,
) {
    // Collected for the final deliveries: no restart is owed for it.
    sup.death_decided(report.incarnation);
    if !sup.silenced.is_empty() && sup.take_silenced(&report.child_id, report.incarnation) {
        return;
    }
    // The end of an incarnation whose start was ignored is no death: the
    // ledger records it as collected and nothing is delivered for it.
    if report.ignored {
        record_collected_end(sup, &report.child_id, report.incarnation);
        return;
    }
    let name = sup
        .registry
        .get(&report.child_id)
        .and_then(|child| child.name.as_deref().map(str::to_string))
        .or_else(|| {
            sup.pending_manual_events
                .get(&(report.child_id.clone(), report.incarnation))
                .and_then(|event| event.child_name.clone())
        })
        .or_else(|| {
            sup.owed_names
                .remove(&(report.child_id.clone(), report.incarnation))
        });
    record_collected_end(sup, &report.child_id, report.incarnation);
    out.push(Collected { report, name });
}

/// As [`collect_report`], for the report a runner returned to the final
/// collection: dropped when the collection already took that incarnation's
/// death from its status plane (`early`), so each death is collected once.
fn collect_runner(
    sup: &mut SupervisionState,
    out: &mut Vec<Collected>,
    early: &HashSet<(ActorId, u64)>,
    report: ChildStoppedInternal,
) {
    if early.contains(&(report.child_id.clone(), report.incarnation)) {
        return;
    }
    collect_report(sup, out, report);
}

/// Collects, delivering nothing yet, every report of an incarnation's end
/// that was still pending when the supervisor's message loop ended.
///
/// The runners of the incarnations whose tasks had already ended, and the
/// attempts whose incarnations had, are awaited first: a runner writes its
/// fate and returns after the task published `Stopped`, so reading the
/// planes at once would miss a child that ended just before the loop did.
/// The set collected is fixed by this collection: an incarnation whose end
/// it does not see is the teardown's. An incarnation that ended on its own,
/// its final reason written, while its task has not published its end (it
/// may still be dropping what it holds) is collected as it would be once
/// the task ended, without waiting for the task, which the teardown then
/// waits for: an attempt whose start failed, was cut or was ordered to stop
/// is reported from that reason with the outcome the attempt reports
/// itself; an attempt whose start completed hands the incarnation over, the
/// collection awaiting the attempt, which returns at its next poll; and a
/// runner's incarnation has its death collected with that reason. Then, in
/// this order: the worklist a
/// completing group left, the deferred triggers, the restart plane (an
/// adopted attempt is adopted, nothing is delivered for it), the attempts
/// reported from their status planes, and the death
/// plane, each read until empty, the death plane then read from the status
/// planes of the runners it still holds. Each collected report is recorded as the
/// teardown records a drain: the ledger reaches `Down` with nothing owed, a
/// snapshot of the end is dropped, a barrier entry is released. No strategy,
/// charge, restart or group step runs.
async fn collect_final<A: Actor>(
    ctx: &mut ActorContext<A>,
    drop_panic: &mut Option<String>,
) -> Vec<Collected> {
    let supervisor = ctx.owner();
    let mut out: Vec<Collected> = Vec::new();
    let Some(sup) = ctx.supervision_ref() else {
        return out;
    };
    let crate::actor::supervision::FinalSets {
        mut runners,
        mut attempts,
        adopts,
        mut reported,
    } = sup.final_await_sets();
    // Runners of adopted attempts whose incarnation ended in transit.
    let mut adopted: std::collections::HashSet<tokio::task::Id> = Default::default();
    // The attempts that hand their incarnation over at their next poll: the
    // wait on one ends early when its cell settles first.
    let mut adopting: Vec<(tokio::task::Id, ActorId, u64)> = Vec::with_capacity(adopts.len());
    let mut cells: Vec<CellWait> = Vec::with_capacity(adopts.len());
    for (task, child_id, incarnation, mut watch) in adopts {
        adopting.push((task, child_id, incarnation));
        cells.push(Box::pin(async move {
            await_cell(&mut watch, incarnation).await
        }));
    }
    let mut cells = SettleSet::new(cells);

    // The runners of the incarnations that ended.
    while !runners.is_empty() {
        let Some(sup) = ctx.supervision_mut() else {
            break;
        };
        match sup.death_set.join_next_with_id().await {
            None => break,
            Some(Ok((task, (child_id, end, incarnation)))) => {
                let (reason, ignored) = end.into_parts();
                sup.task_owner.remove(&task);
                runners.remove(&task);
                collect_report(
                    sup,
                    &mut out,
                    ChildStoppedInternal {
                        child_id,
                        reason,
                        incarnation,
                        ignored,
                    },
                );
            }
            Some(Err(err)) => {
                runners.remove(&err.id());
                if let Some(replacement) = ctx.lost_death_task(err) {
                    runners.insert(replacement);
                }
            }
        }
    }

    // The attempts whose incarnations ended, and those that hand their
    // incarnation over.
    #[cfg(test)]
    if !attempts.is_empty() {
        end_hooks::run(end_hooks::ATTEMPT_WAIT, &supervisor);
    }
    while !attempts.is_empty() {
        let Some(sup) = ctx.supervision_mut() else {
            break;
        };
        let watching = cells.pending() > 0;
        tokio::select! {
            biased;
            joined = sup.restart_set.join_next_with_id() => match joined {
                None => break,
                Some(Ok((task, outcome))) => {
                    sup.task_owner.remove(&task);
                    attempts.remove(&task);
                    collect_restart_outcome(
                        sup,
                        &supervisor,
                        &mut out,
                        &mut adopted,
                        outcome,
                        drop_panic,
                    );
                }
                Some(Err(err)) => {
                    attempts.remove(&err.id());
                    let (outcome, replacement) = ctx.lost_restart_task(err);
                    if let Some(replacement) = replacement {
                        adopted.insert(replacement);
                    }
                    if let (Some(outcome), Some(sup)) = (outcome, ctx.supervision_mut()) {
                        collect_restart_outcome(
                            sup,
                            &supervisor,
                            &mut out,
                            &mut adopted,
                            outcome,
                            drop_panic,
                        );
                    }
                }
            },
            // The cell of an attempt read as handing its incarnation over
            // settled before the attempt returned: its ladder left its idle
            // step meanwhile, so it hands nothing over, and it is reported
            // from its incarnation's final reason, as it reports itself.
            (index, _) = cells.next_settled(), if watching => {
                let (task, child_id, incarnation) = &adopting[index];
                if attempts.remove(task) {
                    let outcome = sup
                        .registry
                        .get(child_id)
                        .and_then(|child| child.slot_of(*incarnation))
                        .and_then(|ctl| crate::actor::supervision::slot::settled_attempt(
                            ctl,
                            child_id,
                            *incarnation,
                        ));
                    if let Some(outcome) = outcome {
                        reported.push((*task, outcome));
                    }
                }
            }
        }
    }

    // The worklist a completing group left, then the deferred triggers.
    if let Some(sup) = ctx.supervision_mut() {
        let owed: Vec<ChildStoppedInternal> = sup.owed.drain(..).collect();
        for report in owed {
            collect_report(sup, &mut out, report);
        }
        let queued: Vec<ChildStoppedInternal> = sup.queued_triggers.drain(..).collect();
        for report in queued {
            collect_report(sup, &mut out, report);
        }
    }

    // The restart plane.
    while let Some(sup) = ctx.supervision_mut() {
        match sup.restart_set.try_join_next_with_id() {
            None => break,
            Some(Ok((task, outcome))) => {
                sup.task_owner.remove(&task);
                collect_restart_outcome(
                    sup,
                    &supervisor,
                    &mut out,
                    &mut adopted,
                    outcome,
                    drop_panic,
                );
            }
            Some(Err(err)) => {
                let (outcome, replacement) = ctx.lost_restart_task(err);
                if let Some(replacement) = replacement {
                    adopted.insert(replacement);
                }
                if let (Some(outcome), Some(sup)) = (outcome, ctx.supervision_mut()) {
                    collect_restart_outcome(
                        sup,
                        &supervisor,
                        &mut out,
                        &mut adopted,
                        outcome,
                        drop_panic,
                    );
                }
            }
        }
    }

    // The attempts whose incarnation's start failed with its final reason
    // written and whose task had not published its end: each is reported
    // with the outcome the attempt reports itself, unless a join above
    // already took it. Its task stays registered, so the teardown waits for
    // it; nothing here waits for the incarnation's drops.
    if let Some(sup) = ctx.supervision_mut() {
        for (task, outcome) in reported {
            if sup.task_owner.contains_key(&task) {
                collect_restart_outcome(
                    sup,
                    &supervisor,
                    &mut out,
                    &mut adopted,
                    outcome,
                    drop_panic,
                );
            }
        }
    }

    // The death plane: the runners the adoptions above spawned for
    // incarnations that ended in transit first, then everything it holds.
    // Once it holds no runner that has returned, the runners left are read
    // from their status planes: a runner whose incarnation published its
    // end is awaited, and an incarnation that ended on its own with its task
    // still dropping has its death collected with its final reason, without
    // waiting for the task. Each pass that collects or awaits anything is
    // followed by another. A death collected from its plane is recorded in
    // `early`, keyed by child and incarnation, and its runner's own report,
    // when the collection joins it, is dropped.
    let mut early: HashSet<(ActorId, u64)> = HashSet::new();
    loop {
        loop {
            while !adopted.is_empty() {
                let Some(sup) = ctx.supervision_mut() else {
                    break;
                };
                match sup.death_set.join_next_with_id().await {
                    None => {
                        adopted.clear();
                    }
                    Some(Ok((task, (child_id, end, incarnation)))) => {
                        let (reason, ignored) = end.into_parts();
                        sup.task_owner.remove(&task);
                        adopted.remove(&task);
                        collect_runner(
                            sup,
                            &mut out,
                            &early,
                            ChildStoppedInternal {
                                child_id,
                                reason,
                                incarnation,
                                ignored,
                            },
                        );
                    }
                    Some(Err(err)) => {
                        adopted.remove(&err.id());
                        if let Some(replacement) = ctx.lost_death_task(err) {
                            adopted.insert(replacement);
                        }
                    }
                }
            }
            let Some(sup) = ctx.supervision_mut() else {
                break;
            };
            match sup.death_set.try_join_next_with_id() {
                None => break,
                Some(Ok((task, (child_id, end, incarnation)))) => {
                    let (reason, ignored) = end.into_parts();
                    sup.task_owner.remove(&task);
                    collect_runner(
                        sup,
                        &mut out,
                        &early,
                        ChildStoppedInternal {
                            child_id,
                            reason,
                            incarnation,
                            ignored,
                        },
                    );
                }
                Some(Err(err)) => {
                    if let Some(replacement) = ctx.lost_death_task(err) {
                        adopted.insert(replacement);
                    }
                }
            }
        }
        let Some(sup) = ctx.supervision_mut() else {
            break;
        };
        let (ended, deaths) = sup.final_deaths(&early);
        if ended.is_empty() && deaths.is_empty() {
            break;
        }
        adopted.extend(ended);
        for report in deaths {
            let key = (report.child_id.clone(), report.incarnation);
            collect_report(sup, &mut out, report);
            #[cfg(test)]
            end_hooks::run(end_hooks::PLANE_REPORT, &key.0);
            early.insert(key);
        }
    }

    // Nothing of the group is left to do, and `children()` inside a final
    // delivery reads no member as awaiting a restart that will not be made.
    if let Some(sup) = ctx.supervision_mut() {
        sup.pending_group = None;
        sup.note_group();
        // An attempt that reached no outcome ends with the supervisor: it
        // reports before the supervisor's own end event.
        sup.verdicts.settle_unsettled();
    }
    out
}

/// Records one restart-plane outcome of the final collection: a failed or
/// stopped attempt is collected for its own delivery, an adopted one becomes
/// the child's running incarnation (its runner is awaited with the death
/// plane when its incarnation had ended in transit), a cancelled one is only
/// recorded, and a factory drop panic it carries is kept in `drop_panic`
/// with the teardown's, for the end to apply before the reason is final.
fn collect_restart_outcome(
    sup: &mut SupervisionState,
    supervisor: &Owner,
    out: &mut Vec<Collected>,
    adopted: &mut std::collections::HashSet<tokio::task::Id>,
    outcome: RestartOutcome,
    drop_panic: &mut Option<String>,
) {
    let (child_id, incarnation, runner, reason) =
        record_restart_outcome(sup, supervisor, outcome, false, drop_panic);
    if let Some(runner) = runner {
        if runner.ended {
            adopted.insert(runner.task);
        }
    }
    if let Some(reason) = reason {
        collect_report(
            sup,
            out,
            ChildStoppedInternal {
                child_id,
                reason,
                incarnation,
                // An attempt's end is never an ignore: that outcome has its
                // own report.
                ignored: false,
            },
        );
    }
}

/// The final deliveries of a supervisor whose message loop ended for any
/// reason but `Kill`, between the loop and `on_stopped`: every report still
/// pending is delivered once, with `SupervisorStopping`, the supervisor's
/// status reading `Stopping`, and no restart or charge. The stop lane is
/// read before each delivery; a `Kill` that arrived ends the drain at once.
/// A delivery that returns an `Err` or panics makes the supervisor's exit
/// reason a `Failure` once `on_stopped` returns, and the remaining
/// deliveries still run. A factory drop panic the collection meets is kept
/// in `drop_panic`, the teardown's.
pub(super) async fn final_drain<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    stop_rx: &LaneRx,
    accepted: &StopReason,
    drop_panic: &mut Option<String>,
) -> FinalOutcome {
    let collected = collect_final(ctx, drop_panic).await;
    // The waiters on the status plane run before the first delivery.
    tokio::task::yield_now().await;
    let mut pending: Option<StopReason> = None;
    for Collected { report, name } in collected {
        if kill_stored(stop_rx) {
            return FinalOutcome::Killed;
        }
        let event = ChildEvent {
            child_id: report.child_id,
            child_name: name,
            reason: report.reason,
            action: SupervisionAction::SupervisorStopping,
        };
        emit_child_died(ctx, &event, report.incarnation);
        if let Some(sup) = ctx.supervision_mut() {
            sup.final_delivery = true;
        }
        let result = hook_turn!(ctx, HookKind::OnChildStopped, always, |token| catch_hook_t(
            token,
            || actor.on_child_stopped(&event, ctx)
        ));
        if let Some(sup) = ctx.supervision_mut() {
            sup.final_delivery = false;
        }
        // The chain does not run after this callback: a message it asked for
        // is dropped here, not carried to a later delivery or to the end.
        ctx.drop_continuation();
        // An earlier failure stands, whether the delivery returns an error or
        // panics.
        let failed = matches!(accepted, StopReason::Failure(_))
            || matches!(pending, Some(StopReason::Failure(_)));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                ctx.record_failure(err.clone());
                if !failed {
                    pending = Some(StopReason::Failure(err));
                }
            }
            Err(payload) => {
                let err = ActorError::Panic(payload_into_string(payload));
                ctx.record_failure(err.clone());
                if !failed {
                    pending = Some(StopReason::Failure(err));
                }
            }
        }
    }
    FinalOutcome::Done { pending }
}

/// The stop-time stage for monitors, for any actor whose message loop ended
/// for a reason but `Kill`, between the loop (and a supervisor's final child
/// reports) and `on_stopped`: every notice the actor is still owed is
/// delivered once through `handle`, the status reading `Stopping`, the stop
/// lane read before each delivery, none once a `Kill` is stored. A delivery
/// that returns an `Err` or panics makes the exit reason a `Failure` once
/// `on_stopped` returns unless one already stands, and the remaining
/// deliveries still run.
///
/// Owed are the notices queued in the mailbox or the priority lane, in queue
/// order, then the monitors whose watched actor ended and whose task has not
/// queued a notice, in monitor order. The stage first closes every monitor
/// nobody has taken, so no task queues another notice from then on; a task
/// that took its notice before is waited for, a few instructions, because it
/// queues no user code. The messages the stage takes from the queues while
/// it looks are kept in the monitor state and dropped with the context.
#[allow(clippy::too_many_arguments)]
pub(super) async fn monitor_drain<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    stop_rx: &LaneRx,
    accepted: &StopReason,
    turn_clock: &TurnClock,
    mailbox: &mut MailRx<ActorEnvelope<A>>,
    mut priority: Option<&mut mpsc::Receiver<ActorEnvelope<A>>>,
    drain: &mut ReplyDrain,
    held: &mut HeldAnswer<A::Response>,
) -> FinalOutcome {
    let mut seen: std::collections::HashSet<MonitorId> = std::collections::HashSet::new();
    let mut queued: Vec<ActorEnvelope<A>> = Vec::new();
    let mut owed: Vec<(MonitorId, DownReason)> = Vec::new();
    let mut link_owed: Vec<(MonitorId, LinkExit)> = Vec::new();
    loop {
        owed.extend(
            ctx.monitors_mut()
                .map(|state| state.close_pass())
                .unwrap_or_default(),
        );
        link_owed.extend(ctx.close_links());
        if !notice_in_flight(ctx, &seen) {
            break;
        }
        // Take what is queued now: at most the queued count, so a producer
        // that refills the queue cannot hold the stage.
        let mut take = mailbox.len();
        while take != 0 {
            take -= 1;
            match mailbox.try_recv_one() {
                Ok(envelope) => keep_or_queue(ctx, envelope, &mut seen, &mut queued),
                Err(_) => break,
            }
        }
        if let Some(lane) = priority.as_deref_mut() {
            let mut take = lane.len();
            while take != 0 {
                take -= 1;
                match lane.try_recv_one() {
                    Ok(envelope) => keep_or_queue(ctx, envelope, &mut seen, &mut queued),
                    Err(_) => break,
                }
            }
        }
        if !notice_in_flight(ctx, &seen) {
            break;
        }
        // A task took its notice and is putting it in the queue.
        tokio::task::yield_now().await;
    }
    let mut pending: Option<StopReason> = None;
    let failed = |pending: &Option<StopReason>| {
        matches!(accepted, StopReason::Failure(_))
            || matches!(pending, Some(StopReason::Failure(_)))
    };
    let mut queued = queued.into_iter();
    while let Some(envelope) = queued.next() {
        if kill_stored(stop_rx) {
            // The notice in hand and those behind it hold the user's
            // messages: each is dropped inside a catch, as every other value
            // the stage leaves behind is.
            drop_queued(std::iter::once(envelope).chain(queued), ctx);
            return FinalOutcome::Killed;
        }
        if let ControlFlow::Break(reason) =
            dispatch(actor, ctx, envelope, turn_clock, drain, held, None).await
        {
            if !failed(&pending) {
                pending = Some(reason);
            }
        }
    }
    for (id, reason) in owed {
        if kill_stored(stop_rx) {
            return FinalOutcome::Killed;
        }
        let made = ctx
            .monitors_mut()
            .and_then(|state| state.owed_message(id, reason));
        let outcome = match made {
            None => continue,
            Some(Ok(payload)) => {
                ctx.finish_monitor(id);
                let envelope = Envelope::Message {
                    payload,
                    responder: None,
                };
                dispatch(actor, ctx, envelope, turn_clock, drain, held, None).await
            }
            Some(Err(panic)) => {
                ctx.finish_monitor(id);
                fail_with(ctx, ActorError::Panic(panic))
            }
        };
        if let ControlFlow::Break(reason) = outcome {
            if !failed(&pending) {
                pending = Some(reason);
            }
        }
    }
    for (id, exit) in link_owed {
        if kill_stored(stop_rx) {
            return FinalOutcome::Killed;
        }
        let made = ctx
            .monitors_mut()
            .and_then(|state| state.link_owed_message(id, exit));
        if let Some(made) = made {
            ctx.finish_monitor(id);
            if let ControlFlow::Break(reason) =
                deliver_made(actor, ctx, made, turn_clock, drain, held).await
            {
                if !failed(&pending) {
                    pending = Some(reason);
                }
            }
        }
    }
    // The exit signals queued in the inbox are the last of the stage, and
    // the inbox is sealed: a signal sent from here on is refused.
    let deliver = ctx.can_deliver_exits();
    for item in ctx.stop_lane().abort_record().take_and_seal_signals() {
        let InboxItem::Exit { from, reason, .. } = item else {
            continue;
        };
        if !deliver {
            continue;
        }
        if kill_stored(stop_rx) {
            return FinalOutcome::Killed;
        }
        let exit = LinkExit {
            actor_id: from.actor_id,
            incarnation: Some(from.incarnation),
            reason: DownReason::Stopped(reason),
        };
        let made = ctx
            .monitors_mut()
            .and_then(|state| state.exit_message(exit));
        if let Some(made) = made {
            if let ControlFlow::Break(reason) =
                deliver_made(actor, ctx, made, turn_clock, drain, held).await
            {
                if !failed(&pending) {
                    pending = Some(reason);
                }
            }
        }
    }
    if let Some(state) = ctx.monitors_mut() {
        state.clear();
    }
    FinalOutcome::Done { pending }
}

/// Drops the notices a stage leaves undelivered when a `Kill` ends it, each
/// inside a catch: the message in a notice is the user's, and its drop can
/// panic. The panic is logged with the actor's id under `tracing`, and the
/// actor still ends as `Kill`.
fn drop_queued<A: Actor>(notices: impl Iterator<Item = ActorEnvelope<A>>, ctx: &ActorContext<A>) {
    for notice in notices {
        if let Some(panic) = end_value(notice) {
            log_late_drop(&panic, &ctx.owner());
        }
    }
}

/// Delivers a message the stage made from an exit: through `handle`, or, when
/// the map panicked, as the failure of the stage.
async fn deliver_made<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    made: Result<A::Message, String>,
    turn_clock: &TurnClock,
    drain: &mut ReplyDrain,
    held: &mut HeldAnswer<A::Response>,
) -> ControlFlow<StopReason> {
    match made {
        Ok(payload) => {
            let envelope = Envelope::Message {
                payload,
                responder: None,
            };
            dispatch(actor, ctx, envelope, turn_clock, drain, held, None).await
        }
        Err(panic) => fail_with(ctx, ActorError::Panic(panic)),
    }
}

/// Whether a monitor's task took its notice and the stage has not yet found
/// it in a queue.
fn notice_in_flight<A: Actor>(
    ctx: &mut ActorContext<A>,
    seen: &std::collections::HashSet<MonitorId>,
) -> bool {
    ctx.monitors_mut()
        .is_some_and(|state| state.in_flight(seen))
}

/// Sorts a message the stage took from a queue: a notice of a monitor the
/// actor still holds goes to the deliveries, anything else is kept until the
/// context drops.
fn keep_or_queue<A: Actor>(
    ctx: &mut ActorContext<A>,
    envelope: ActorEnvelope<A>,
    seen: &mut std::collections::HashSet<MonitorId>,
    queued: &mut Vec<ActorEnvelope<A>>,
) {
    let Some(state) = ctx.monitors_mut() else {
        return;
    };
    match &envelope {
        Envelope::Signal { tag, .. } => {
            seen.insert(tag.shared.id());
            if state.wants(tag) {
                queued.push(envelope);
                return;
            }
        }
        Envelope::Message { .. } => {}
    }
    state.stash(envelope);
}
