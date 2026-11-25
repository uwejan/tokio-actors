//! A supervisor's teardown: stopping every child, settling their ends and the strict start wait.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;

use tokio::task::JoinError;

use super::{
    end_supervision, end_value, keep_first_of, record_collected_end, record_restart_outcome,
    report_unexpected_end, walk_child,
};
use crate::actor::context::ActorContext;
use crate::actor::incarnation::LaneRx;
use crate::actor::panic::contain_waker_panic;
#[cfg(test)]
use crate::actor::runtime::end_hooks;
use crate::actor::supervision::rules::{
    strict_closed_ordered, strict_step, StrictAckKind, StrictStep, TeardownAction,
};
use crate::actor::supervision::{
    await_cell, rules, CellOutcome, CellWatch, ChildLifecycle, ChildState, RestartOutcome,
    TaskPlane,
};
use crate::actor::Actor;
use crate::error::ActorError;
use crate::types::{ActorId, ChildStoppedInternal, StopReason};

/// Drains one restart-plane completion during the teardown: records it,
/// keeping a factory drop panic it carries in `drop_panic`, and returns the
/// child and incarnation it concerned, and whether the attempt became a
/// running incarnation.
fn drain_restart_for_teardown<A: Actor>(
    ctx: &mut ActorContext<A>,
    joined: Result<(tokio::task::Id, RestartOutcome), JoinError>,
    drop_panic: &mut Option<String>,
) -> Option<(ActorId, u64, bool)> {
    let supervisor = ctx.owner();
    match joined {
        Ok((task, outcome)) => {
            let sup = ctx.supervision_mut()?;
            // An attempt the final collection reported from its status plane
            // was not stopped by the teardown: its end is not logged as one.
            let warn = !sup
                .task_owner
                .remove(&task)
                .is_some_and(|(child, incarnation, _)| sup.reported_early(&child, incarnation));
            let (child, incarnation, adopted, ended) =
                record_restart_outcome(sup, &supervisor, outcome, warn, drop_panic);
            if ended.is_some() {
                record_collected_end(sup, &child, incarnation);
            }
            Some((child, incarnation, adopted.is_some()))
        }
        Err(err) => {
            let owner = ctx
                .supervision_ref()
                .and_then(|sup| sup.task_owner.get(&err.id()).cloned());
            let (outcome, _) = ctx.lost_restart_task(err);
            match outcome {
                Some(outcome) => {
                    let sup = ctx.supervision_mut()?;
                    let (child, incarnation, adopted, ended) =
                        record_restart_outcome(sup, &supervisor, outcome, true, drop_panic);
                    if ended.is_some() {
                        record_collected_end(sup, &child, incarnation);
                    }
                    Some((child, incarnation, adopted.is_some()))
                }
                None => owner.map(|(child, incarnation, _)| (child, incarnation, false)),
            }
        }
    }
}

/// One stop the teardown waits for: the child, the incarnation whose end
/// settles it, whether the incarnation is a restart attempt, whether the
/// final collection already collected its end (so the end is not logged as
/// one of a stop the teardown made), and the watch on its slot's cell.
struct StopWait {
    id: ActorId,
    token: u64,
    attempt: bool,
    collected: bool,
    watch: CellWatch,
}

/// A wait on a slot's cell that owns its watch.
pub(super) type CellWait = Pin<Box<dyn Future<Output = CellOutcome> + Send>>;

/// The waits of a teardown, each polled only after its own waker fired, so
/// one settled wait costs one poll of that wait and not a poll of every wait
/// still pending.
///
/// Each wait gets a waker of its own ([`WaitWaker`]). Its wake puts the
/// wait's index on the shared queue, once until the wait is polled again, and
/// wakes the task that polls the set, inside the crate's catch. A poll takes
/// the queue and polls only those waits. The queue's lock is never held
/// across a poll of a wait or a wake.
pub(super) struct SettleSet {
    waits: Vec<Option<CellWait>>,
    wakers: Vec<(Arc<WaitWaker>, std::task::Waker)>,
    woken: Arc<Woken>,
    pending: usize,
    #[cfg(test)]
    pub(super) polls: usize,
}

/// The indices of the waits woken since the set last took them, and the
/// waker of the task that polls the set.
#[derive(Default)]
struct Woken(std::sync::Mutex<WokenQueue>);

#[derive(Default)]
struct WokenQueue {
    indices: VecDeque<usize>,
    parent: Option<std::task::Waker>,
}

impl Woken {
    fn queue(&self) -> std::sync::MutexGuard<'_, WokenQueue> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The waker of one wait of a [`SettleSet`].
struct WaitWaker {
    index: usize,
    /// Whether the index is on the queue and the wait not yet polled since.
    queued: AtomicBool,
    woken: Arc<Woken>,
}

impl std::task::Wake for WaitWaker {
    fn wake(self: Arc<Self>) {
        self.put_on_queue();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.put_on_queue();
    }
}

impl WaitWaker {
    /// Queues the wait's index, unless it is queued already, and wakes the
    /// task that polls the set.
    fn put_on_queue(&self) {
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let parent = {
            let mut queue = self.woken.queue();
            queue.indices.push_back(self.index);
            queue.parent.clone()
        };
        if let Some(parent) = parent {
            contain_waker_panic("teardown wait", None, move || parent.wake());
        }
    }
}

impl SettleSet {
    /// Every wait starts on the queue, so its first poll registers it.
    pub(super) fn new(waits: Vec<CellWait>) -> Self {
        let woken = Arc::new(Woken::default());
        let wakers = (0..waits.len())
            .map(|index| {
                let waker = Arc::new(WaitWaker {
                    index,
                    queued: AtomicBool::new(true),
                    woken: woken.clone(),
                });
                let handle = std::task::Waker::from(waker.clone());
                (waker, handle)
            })
            .collect();
        woken.queue().indices.extend(0..waits.len());
        Self {
            pending: waits.len(),
            waits: waits.into_iter().map(Some).collect(),
            wakers,
            woken,
            #[cfg(test)]
            polls: 0,
        }
    }

    /// The waits that have not settled.
    pub(super) fn pending(&self) -> usize {
        self.pending
    }

    /// Resolves with the index and outcome of a wait that settled, and drops
    /// it. Pending while none of the woken waits has.
    pub(super) fn next_settled(&mut self) -> impl Future<Output = (usize, CellOutcome)> + '_ {
        std::future::poll_fn(move |cx| {
            let mut taken = {
                let mut queue = self.woken.queue();
                if !queue
                    .parent
                    .as_ref()
                    .is_some_and(|parent| parent.will_wake(cx.waker()))
                {
                    queue.parent = Some(cx.waker().clone());
                }
                std::mem::take(&mut queue.indices)
            };
            while let Some(index) = taken.pop_front() {
                let (flag, waker) = &self.wakers[index];
                // Cleared before the poll: a wake from here on queues the
                // wait again.
                flag.queued.swap(false, Ordering::AcqRel);
                let Some(wait) = self.waits[index].as_mut() else {
                    continue;
                };
                #[cfg(test)]
                {
                    self.polls += 1;
                }
                let mut wait_cx = std::task::Context::from_waker(waker);
                if let Poll::Ready(outcome) = wait.as_mut().poll(&mut wait_cx) {
                    self.waits[index] = None;
                    self.pending -= 1;
                    if !taken.is_empty() {
                        // Not polled yet: still queued, so put them back first.
                        let mut queue = self.woken.queue();
                        taken.append(&mut queue.indices);
                        queue.indices = taken;
                    }
                    return Poll::Ready((index, outcome));
                }
            }
            Poll::Pending
        })
    }
}

/// Waits for the given stops to settle and records the ends. The restart
/// plane is drained meanwhile: an attempt ordered to stop reports on it, and
/// an outcome that waited there for the supervisor to adopt it is adopted, so
/// its runner honors the order. Each wait ends with its cell's settlement
/// and, for an attempt, the drain of its own report, or the absence of any
/// restart-plane task working for it (its report was taken before the wait);
/// an Unresponsive verdict ends it at once. A factory drop panic a drained
/// attempt carries is kept in `drop_panic`.
///
/// No restart-plane task is started while the teardown waits (the factories
/// are gone), so the restart-plane keys of the targets, read once here, change
/// only when this wait drains one of their tasks, and a counter of the
/// targets not yet done replaces a scan of every target at every wake.
async fn settle_children<A: Actor>(
    ctx: &mut ActorContext<A>,
    waits: Vec<StopWait>,
    stop_rx: &mut LaneRx,
    drop_panic: &mut Option<String>,
) -> Teardown {
    let supervisor = ctx.actor_id().clone();
    let mut targets: Vec<(ActorId, u64, bool)> = Vec::with_capacity(waits.len());
    let mut cells: Vec<CellWait> = Vec::with_capacity(waits.len());
    let mut reported = Vec::with_capacity(waits.len());
    for wait in waits {
        let StopWait {
            id,
            token,
            attempt,
            collected,
            mut watch,
        } = wait;
        targets.push((id, token, attempt));
        reported.push(collected);
        cells.push(Box::pin(async move { await_cell(&mut watch, token).await }));
    }
    let mut cells = SettleSet::new(cells);
    let mut settled: Vec<Option<CellOutcome>> = targets.iter().map(|_| None).collect();
    let mut done = vec![false; targets.len()];
    let mut remaining = targets.len();
    // Every target by its key, and the keys a restart-plane task works for.
    let mut by_key: HashMap<(ActorId, u64), Vec<usize>> = HashMap::new();
    for (index, (id, token, _)) in targets.iter().enumerate() {
        by_key.entry((id.clone(), *token)).or_default().push(index);
    }
    let mut restart_keys: HashSet<(ActorId, u64)> = ctx
        .supervision_ref()
        .map(|sup| {
            sup.task_owner
                .values()
                .filter(|(owner, incarnation, plane)| {
                    *plane == TaskPlane::Restart
                        && by_key.contains_key(&(owner.clone(), *incarnation))
                })
                .map(|(owner, incarnation, _)| (owner.clone(), *incarnation))
                .collect()
        })
        .unwrap_or_default();
    // A target is done once its cell settled and, for an attempt, its own
    // report was drained, or no task of the restart plane works for it any
    // more (its report was taken before this wait: the final collection
    // takes it); an Unresponsive verdict ends it at once.
    let finished = |index: usize,
                    targets: &[(ActorId, u64, bool)],
                    settled: &[Option<CellOutcome>],
                    restart_keys: &HashSet<(ActorId, u64)>| {
        let (id, token, attempt) = &targets[index];
        settled[index].as_ref().is_some_and(|outcome| {
            !attempt
                || matches!(outcome, CellOutcome::Unresponsive)
                || !restart_keys.contains(&(id.clone(), *token))
        })
    };
    if kill_stored(stop_rx) {
        return Teardown::Killed;
    }
    while let Some(sup) = ctx.supervision_mut() {
        if remaining == 0 {
            break;
        }
        let restart_pending = !sup.restart_set.is_empty();
        let cells_pending = cells.pending() > 0;
        tokio::select! {
            biased;
            // A `Kill` raised while the teardown waits turns the rest of it
            // into the `Kill` path. Only a stored `Kill` ends the wait: the
            // lane holds nothing else new once it is narrowed, and a version
            // the loop never consumed is consumed here.
            changed = stop_rx.changed() => {
                match changed {
                    Ok(()) if kill_stored(stop_rx) => return Teardown::Killed,
                    Ok(()) => {}
                }
            }
            (index, outcome) = cells.next_settled(), if cells_pending => {
                settled[index] = Some(outcome);
                if !done[index] && finished(index, &targets, &settled, &restart_keys) {
                    done[index] = true;
                    remaining -= 1;
                }
            }
            joined = sup.restart_set.join_next_with_id(), if restart_pending => {
                if let Some(joined) = joined {
                    let task = match &joined {
                        Ok((task, _)) => *task,
                        Err(err) => err.id(),
                    };
                    let joined_key = sup
                        .task_owner
                        .get(&task)
                        .filter(|(_, _, plane)| *plane == TaskPlane::Restart)
                        .map(|(owner, incarnation, _)| (owner.clone(), *incarnation));
                    let drained = drain_restart_for_teardown(ctx, joined, drop_panic);
                    let mut touched: Vec<usize> = Vec::new();
                    if let Some(key) = joined_key {
                        restart_keys.remove(&key);
                        touched.extend(by_key.get(&key).into_iter().flatten().copied());
                    }
                    if let Some((child, incarnation, adopted)) = drained {
                        let key = (child, incarnation);
                        restart_keys.remove(&key);
                        for &index in by_key.get(&key).into_iter().flatten() {
                            targets[index].2 = false;
                            reported[index] |= !adopted;
                            touched.push(index);
                        }
                    }
                    for index in touched {
                        if !done[index] && finished(index, &targets, &settled, &restart_keys) {
                            done[index] = true;
                            remaining -= 1;
                        }
                    }
                }
            }
        }
    }
    for (index, outcome) in settled.into_iter().enumerate() {
        let Some(CellOutcome::Fate(fate)) = outcome else {
            continue;
        };
        if reported[index] {
            continue;
        }
        let (id, token, _) = &targets[index];
        if let Some(child) = ctx.supervision_ref().and_then(|sup| sup.registry.get(id)) {
            report_unexpected_end(
                &supervisor,
                id,
                *token,
                &fate.reason,
                child.spec.restart_type,
                child.spec.shutdown,
                child.spec.sys.sensitive,
            );
        }
    }
    Teardown::Done
}

/// How the teardown of a supervisor's children ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Teardown {
    /// Every child was settled (or left behind unresponsive).
    Done,
    /// A `Kill` reached the supervisor meanwhile: the rest of the teardown
    /// is the `Kill` path.
    Killed,
}

/// Carries out the teardown's action for one child and names the stop to
/// wait for: the incarnation whose end settles it, and whether that
/// incarnation is a restart attempt. A running child is ordered to stop, a
/// child whose stop is in progress is joined (never given a second ladder;
/// under `Infinity` a vetoable request is asked once more), and a restart
/// attempt is ordered to stop too - it cancels before its factory runs, or
/// stops its starting incarnation.
fn begin_child_stop(id: &ActorId, child: &ChildState, action: TeardownAction) -> Option<StopWait> {
    let (token, attempt) = match (action, child.lifecycle) {
        (TeardownAction::Order, ChildLifecycle::Running(n)) => {
            child.ctl.order_stop(child.spec.shutdown, true);
            (n, false)
        }
        (
            TeardownAction::Join { ask },
            ChildLifecycle::Running(n) | ChildLifecycle::Stopping { incarnation: n, .. },
        ) => {
            if ask {
                child.ctl.ask_again();
            }
            (n, false)
        }
        (TeardownAction::OrderAttempt { ask }, ChildLifecycle::Restarting { next, .. }) => {
            child.ctl.order_stop(child.spec.shutdown, false);
            if ask {
                child.ctl.ask_again();
            }
            (next, true)
        }
        _ => return None,
    };
    Some(StopWait {
        id: id.clone(),
        token,
        attempt,
        collected: false,
        watch: child.ctl.watch(),
    })
}

/// Stops all children under their own `Shutdown` policies. The teardown takes
/// every occupant. Each child's runner (or attempt) raises the first signal,
/// escalates a `Timeout` to Kill at expiry and then to an abort, and records
/// the end; a populated fate implies the child task has fully terminated (the
/// runner only writes it after the child's own `JoinHandle` resolves, which
/// tokio guarantees only after every one of the child's own drops have
/// already run). A child that outlives its whole ladder carries the
/// Unresponsive verdict and is left behind instead of hanging the parent's
/// shutdown. No strategy, restart or `on_child_stopped` runs meanwhile. Never
/// reached when this task is itself dying with Kill - that path drops the
/// supervision state directly instead (see the `run_actor` cascade-inversion
/// comment). The child factories and the supervision state are dropped with
/// every drop caught; the first panic is left in `drop_panic`, as is the panic
/// of a factory's drop a restart attempt made when the factory's spec went
/// during its build.
///
/// A pool orders every child at once, in one step, and then waits for all of
/// them. Every other supervisor stops its children newest first, each after
/// the previous one settled.
pub(super) async fn stop_all_children<A: Actor>(
    ctx: &mut ActorContext<A>,
    stop_rx: &mut LaneRx,
    drop_panic: &mut Option<String>,
) -> Teardown {
    let (strategy, order, factories) = match ctx.supervision_mut() {
        Some(sup) => {
            let factories = std::mem::take(&mut sup.restart_fns);
            sup.pending_group = None;
            sup.note_group();
            for report in std::mem::take(&mut sup.queued_triggers) {
                sup.death_decided(report.incarnation);
            }
            (
                sup.config.strategy,
                sup.registry
                    .iter()
                    .map(|child| child.id.clone())
                    .collect::<Vec<_>>(),
                factories,
            )
        }
        None => return Teardown::Done,
    };
    // The child factories are dropped one at a time, each drop caught.
    let supervisor = ctx.owner();
    for (_, factory) in factories {
        keep_first_of(drop_panic, end_value(factory), &supervisor);
    }

    if rules::teardown_at_once(strategy) {
        let Some(sup) = ctx.supervision_ref() else {
            return Teardown::Done;
        };
        let children: Vec<(ActorId, rules::WalkChild)> = order
            .iter()
            .filter_map(|id| {
                sup.registry
                    .get(id)
                    .map(|child| (id.clone(), walk_child(child)))
            })
            .collect();
        let waits: Vec<StopWait> = rules::teardown_walk(strategy, &children)
            .into_iter()
            .filter_map(|(id, action)| {
                let child = sup.registry.get(&id)?;
                begin_child_stop(&id, child, action)
            })
            .collect();
        if settle_children(ctx, waits, stop_rx, drop_panic).await == Teardown::Killed {
            return Teardown::Killed;
        }
    } else {
        // Newest first.
        for id in order.into_iter().rev() {
            let Some(sup) = ctx.supervision_ref() else {
                return Teardown::Done;
            };
            let Some(child) = sup.registry.get(&id) else {
                continue;
            };
            #[cfg(test)]
            end_hooks::run(end_hooks::WALK, &supervisor);
            let walked = rules::teardown_walk(strategy, &[(id.clone(), walk_child(child))]);
            let Some((_, action)) = walked.into_iter().next() else {
                continue;
            };
            let Some(wait) = begin_child_stop(&id, child, action) else {
                continue;
            };
            if settle_children(ctx, vec![wait], stop_rx, drop_panic).await == Teardown::Killed {
                return Teardown::Killed;
            }
        }
    }
    // The incarnations whose ends the final collection took from their
    // status planes and whose tasks have not returned: the ledger holds them
    // down, and the supervisor publishes its end only once each incarnation
    // ended. No signal is raised: each is ending already. An attempt's wait
    // is bounded by the attempt's own ladder; a runner's ladder is armed with
    // the child's own policy, as the walk's order would arm it.
    let reported: Vec<StopWait> = ctx
        .supervision_ref()
        .map(|sup| {
            sup.reported_ends()
                .into_iter()
                .map(|(id, token, attempt, watch)| {
                    if !attempt {
                        if let Some(child) = sup.registry.get(&id) {
                            if let Some(ctl) = child.slot_of(token) {
                                ctl.order_bound(child.spec.shutdown);
                            }
                        }
                    }
                    StopWait {
                        id,
                        token,
                        attempt,
                        collected: true,
                        watch,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    if !reported.is_empty() {
        #[cfg(test)]
        end_hooks::run(end_hooks::REPORTED_WAIT, &supervisor);
        if settle_children(ctx, reported, stop_rx, drop_panic).await == Teardown::Killed {
            return Teardown::Killed;
        }
    }
    if let Some(sup) = ctx.take_supervision() {
        keep_first_of(drop_panic, end_supervision(sup, &supervisor), &supervisor);
    }
    Teardown::Done
}

/// The reports of incarnation ends the supervisor's message loop left
/// pending, collected for their own deliveries.
pub(super) struct Collected {
    pub(super) report: ChildStoppedInternal,
    pub(super) name: Option<String>,
}

/// What the final deliveries ended with.
pub(super) enum FinalOutcome {
    /// Every delivery ran. `pending` is the exit reason a failed delivery
    /// makes of the supervisor, applied when `on_stopped` returns.
    Done { pending: Option<StopReason> },
    /// A `Kill` reached the supervisor: the rest of the deliveries and
    /// `on_stopped` are skipped.
    Killed,
}

/// Whether the stop lane holds a `Kill`, read without consuming it.
pub(super) fn kill_stored(stop_rx: &LaneRx) -> bool {
    matches!(stop_rx.borrow().reason, Some(StopReason::Kill))
}

/// How the strict start's wait for the supervisor's children ended.
pub(super) enum StrictOutcome {
    /// Every child spawned while starting finished its initialization.
    Done,
    /// A `Kill` is stored on the supervisor's lane.
    Killed,
    /// A child's initialization failed: the supervisor's start fails with it.
    Failed(ActorError),
}

/// Resolves when a `Kill` is stored on the lane, without marking any version
/// seen: a request of another kind stays for the message loop, which reads it
/// after the start. The wake slot is the run loop's own, armed only while
/// this future is polled and disarmed when it drops.
struct KillStored<'a> {
    rx: &'a LaneRx,
    slot: &'a crate::actor::incarnation::WakeSlot,
}

impl Future for KillStored<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if kill_stored(self.rx) {
            return Poll::Ready(());
        }
        self.slot.register(cx.waker());
        if kill_stored(self.rx) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for KillStored<'_> {
    fn drop(&mut self) {
        self.slot.disarm();
    }
}

/// The strict start's wait: for each child spawned while the supervisor
/// started, in the order they were spawned, until its initialization is
/// acknowledged. The children initialize concurrently; only the wait is in
/// order. The wait has no bound of its own (the start bound of the
/// supervisor's spawn, if any, cuts it); a `Kill` ends it at once. A child
/// that fails, or ended before its initialization finished without its
/// supervisor removing, replacing or ordering that end, fails the start with
/// [`ActorError::ChildStart`]. Each acknowledgment belongs to the incarnation
/// that owes it: what the ledger holds for the child decides only for that
/// incarnation, and its failure reason is read from that incarnation's slot.
pub(super) async fn await_strict_children<A: Actor>(
    ctx: &mut ActorContext<A>,
    stop_rx: &LaneRx,
) -> StrictOutcome {
    let acks = match ctx.supervision_mut() {
        Some(sup) => sup.take_strict_acks(),
        None => return StrictOutcome::Done,
    };
    let record = ctx.stop_lane().abort_record().clone();
    for (child, incarnation, child_record, mut ack) in acks {
        let answer = tokio::select! {
            biased;
            () = KillStored { rx: stop_rx, slot: record.reader() } => {
                return StrictOutcome::Killed;
            }
            answer = &mut ack => answer,
        };
        let kind = match &answer {
            Ok(Ok(())) => StrictAckKind::Ok,
            Ok(Err(_)) => StrictAckKind::Err,
            // The acknowledgment closed once the child's reason was final:
            // an ignored start is no failure to start.
            Err(_closed) if child_record.ended_ignored() => StrictAckKind::Ignored,
            Err(_closed) => StrictAckKind::Closed {
                ordered: {
                    let held = ctx
                        .supervision_ref()
                        .and_then(|sup| sup.registry.get(&child));
                    strict_closed_ordered(
                        held.map(|state| state.lifecycle.occupant()),
                        incarnation,
                        held.and_then(|state| state.slot_of(incarnation))
                            .is_some_and(|ctl| ctl.order_before_end()),
                    )
                },
            },
        };
        if strict_step(kind) == StrictStep::Next {
            continue;
        }
        let error = match answer {
            Ok(Err(err)) => err,
            _ => {
                // The child ended before it acknowledged: its final reason is
                // the error, `Failure`'s own, else a description.
                let status = ctx
                    .supervision_ref()
                    .and_then(|sup| sup.registry.get(&child))
                    .and_then(|state| state.slot_of(incarnation))
                    .and_then(|ctl| ctl.status());
                let reason = match status {
                    Some(mut status) => {
                        tokio::select! {
                            biased;
                            () = KillStored { rx: stop_rx, slot: record.reader() } => {
                                return StrictOutcome::Killed;
                            }
                            reason = crate::system::wait_final_reason(&mut status) => reason,
                        }
                    }
                    None => StopReason::Kill,
                };
                match reason {
                    StopReason::Failure(err) => err,
                    other => ActorError::user(format!("child ended during start: {other}")),
                }
            }
        };
        return StrictOutcome::Failed(ActorError::ChildStart {
            child,
            error: Box::new(error),
        });
    }
    StrictOutcome::Done
}
