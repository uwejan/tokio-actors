//! Actor task spawning and the message-dispatch run loop.
//!
//! The run loop's `select!` is `biased`, always draining the system channel
//! (`Stop`, `GetStatus`, `ChildStopped`, `RestartComplete`) before the user
//! mailbox. This ordering is deliberate, not incidental: it mirrors the OTP
//! spirit of signals and supervisor traffic pre-empting ordinary message-queue
//! processing, so a pending `Stop` is honored and a child's death is observed
//! even while the mailbox is full, and `get_status` answers without waiting
//! behind user traffic. The residual cost is the mirror image of that benefit:
//! a sustained flood of child-death events on the system channel can starve
//! the mailbox, delaying user message processing for as long as the flood
//! keeps the system channel non-empty.

use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle};

use crate::actor::panic::{catch_callback, payload_into_string};
use crate::actor::supervision::{
    evaluate_strategy, GroupPhase, GroupRestart, ManualStop, StrategyOutcome, SupervisionConfig,
    SupervisionState, KILL_GRACE,
};
use crate::actor::{context::ActorContext, handle::ActorHandle, Actor, ActorEnvelope};
use crate::error::{ActorError, SpawnError, SupervisionError};
use crate::system::{ActorSystem, RegistryGuard};
use crate::types::{
    ActorId, ActorStatus, ActorStatusInfo, ChildEvent, ChildStoppedInternal, Envelope,
    RestartStrategy, RestartType, Shutdown, StopReason, SupervisionAction, SystemMessage,
};

/// Configuration for the actor's mailbox.
#[derive(Debug, Clone)]
pub struct MailboxConfig {
    /// The maximum number of messages the mailbox can hold.
    pub capacity: usize,
}

impl Default for MailboxConfig {
    fn default() -> Self {
        Self { capacity: 64 }
    }
}

impl MailboxConfig {
    /// Sets the mailbox capacity.
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }
}

/// Configuration for spawning an actor.
#[derive(Debug, Clone, Default)]
pub struct ActorConfig {
    /// Mailbox configuration.
    pub mailbox: MailboxConfig,
    /// Supervision configuration. `Some` if this actor is a supervisor.
    pub supervision: Option<SupervisionConfig>,
}

impl<'a> From<&'a ActorConfig> for ActorConfig {
    fn from(value: &'a ActorConfig) -> Self {
        value.clone()
    }
}

impl ActorConfig {
    /// Sets the mailbox capacity.
    pub fn with_mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox.capacity = capacity;
        self
    }

    /// Sets the complete mailbox configuration.
    pub fn with_mailbox(mut self, mailbox: MailboxConfig) -> Self {
        self.mailbox = mailbox;
        self
    }

    /// Makes the actor a SUPERVISOR with the default configuration
    /// (OneForOne, 3 restarts / 5s window).
    ///
    /// The actor takes the supervisor role; its children are the supervised
    /// ones.
    pub fn supervisor(mut self) -> Self {
        self.supervision = Some(SupervisionConfig::default());
        self
    }

    /// Enables supervision with a custom configuration.
    pub fn with_supervision(mut self, config: SupervisionConfig) -> Self {
        self.supervision = Some(config);
        self
    }
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Spawns an actor detached: no initialization ack, `_join` discarded. This
/// is the fire-and-forget spawn behind
/// [`SpawnBuilder::detached`](crate::actor::SpawnBuilder::detached).
pub(crate) fn into_actor<A: Actor>(
    id: impl Into<ActorId>,
    actor: A,
    config: impl Into<ActorConfig>,
    name: Option<String>,
    system: Option<Arc<ActorSystem>>,
) -> Result<ActorHandle<A>, SpawnError> {
    // Only reachable from the top-level `.detached()` spawn path: always a root.
    let (handle, _join) = spawn_actor(id.into(), actor, config.into(), name, system, None, true)?;
    Ok(handle)
}

/// Spawns an actor, returning the handle and the task's `JoinHandle`.
///
/// The task completes with the actor's final [`StopReason`]. For supervised
/// children the caller wraps the `JoinHandle` in a watcher
/// ([`spawn_watcher`]); the watcher, not the dying actor, is the authoritative
/// death signal (a panicking actor cannot self-report).
///
/// `ack`, if present, is signalled exactly once from inside the task: `Ok(())`
/// right after the actor transitions to [`ActorStatus::Running`] (post
/// `on_started`, pre message loop), or `Err` once `pre_start`/`on_started`
/// failed AND the full teardown (including the registry guard drop) has run.
/// Callers that do not want to wait (supervised children, `.detached()`
/// spawns) pass `None`.
///
/// `is_root` distinguishes a top-level `SpawnBuilder` spawn (`true`) from a
/// supervised child (`false`, `spawn_child` and its restart path). System
/// shutdown only signals roots directly; a root's own supervision cascade
/// takes its children down in turn.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_actor<A: Actor>(
    id: ActorId,
    actor: A,
    config: ActorConfig,
    name: Option<String>,
    system: Option<Arc<ActorSystem>>,
    ack: Option<oneshot::Sender<Result<(), ActorError>>>,
    is_root: bool,
) -> Result<(ActorHandle<A>, JoinHandle<StopReason>), SpawnError> {
    let handle = Handle::try_current().map_err(|_| SpawnError::MissingRuntime)?;
    let mailbox_capacity = config.mailbox.capacity;
    let (tx, rx) = mpsc::channel(mailbox_capacity);
    let (system_tx, system_rx) = mpsc::channel::<SystemMessage>(64);
    let (status_tx, status_rx) = watch::channel(ActorStatus::Initializing);
    let actor_handle = ActorHandle::new(
        id.clone(),
        tx,
        system_tx.clone(),
        mailbox_capacity,
        status_rx,
    );

    let supervision = config.supervision.map(SupervisionState::new);
    let context = ActorContext::new(
        id.clone(),
        actor_handle.clone(),
        handle.clone(),
        status_tx,
        system_tx.clone(),
        system.clone(),
        name.clone(),
        supervision,
    );

    // Register in the target system when a name or explicit system is
    // provided. The name-claim happens BEFORE the task is spawned; the
    // AbortHandle does not exist yet at this point (it only exists once
    // `handle.spawn` returns a JoinHandle), so it is attached in a second
    // step just below.
    let target_system = if name.is_some() || system.is_some() {
        Some(system.unwrap_or_else(ActorSystem::default))
    } else {
        None
    };
    let guard = if let Some(target) = &target_system {
        target.register_actor::<A>(&id, name.as_deref(), &actor_handle, is_root)?;
        Some(RegistryGuard::new(target.clone(), id.clone(), name.clone()))
    } else {
        None
    };

    let join = handle.spawn(run_actor(
        actor, context, rx, system_rx, system_tx, guard, ack,
    ));

    // Attach the abort handle to the just-created registry entries. Any
    // shutdown sweep that lands in the brief window before this call treats
    // the entry as not-yet-abortable (see `AnyActorHandle::abort`).
    if let Some(target) = &target_system {
        target.attach_abort(&id, name.as_deref(), join.abort_handle());
    }

    Ok((actor_handle, join))
}

/// Spawns the watcher task for a supervised child.
///
/// The watcher owns the child's `JoinHandle<StopReason>`: it awaits the task's
/// completion (guaranteed by tokio to happen only after ALL the child's drops,
/// including its `RegistryGuard`, have finished - so the registered name is
/// already free when the death is observed), derives the death reason (normal
/// return, panic, or abort), and delivers `ChildStopped` to the parent with an
/// awaited send - a child's death is never silently dropped.
pub(crate) fn spawn_watcher(
    child_id: ActorId,
    incarnation: u64,
    join: JoinHandle<StopReason>,
    parent_tx: mpsc::Sender<SystemMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let reason = match join.await {
            Ok(reason) => reason,
            Err(err) if err.is_panic() => match err.try_into_panic() {
                Ok(payload) => StopReason::Failure(ActorError::Panic(payload_into_string(payload))),
                Err(_) => {
                    StopReason::Failure(ActorError::Panic("panic payload unavailable".to_string()))
                }
            },
            Err(_) => StopReason::Cancelled,
        };
        // If the parent is gone (or shutting down with its system receiver
        // already dropped), the send fails fast: nobody is left to notify.
        let _ = parent_tx
            .send(SystemMessage::ChildStopped(ChildStoppedInternal {
                child_id,
                reason,
                incarnation,
            }))
            .await;
    })
}

// ---------------------------------------------------------------------------
// Actor run loop
// ---------------------------------------------------------------------------

/// Runs the actor's whole lifecycle: init, message loop, teardown.
///
/// `ack` (see [`spawn_actor`]) is resolved from exactly one of two places in
/// this function: the success arm right after the `Running` transition, or
/// the very end, after `registry_guard` has dropped, if init failed. That
/// ordering is load-bearing (N3 / OTP 26 corpse-consumed parity): it is why
/// the two former early-return blocks for `pre_start`/`on_started` failures
/// are now a labeled block that falls through to the shared teardown tail
/// instead of returning directly.
async fn run_actor<A: Actor>(
    mut actor: A,
    mut ctx: ActorContext<A>,
    mut mailbox: mpsc::Receiver<ActorEnvelope<A>>,
    mut system_rx: mpsc::Receiver<SystemMessage>,
    system_tx: mpsc::Sender<SystemMessage>,
    registry_guard: Option<RegistryGuard>,
    mut ack: Option<oneshot::Sender<Result<(), ActorError>>>,
) -> StopReason {
    // Phase 1 + 2: pre_start/on_started validation (fail-fast gate). A panic
    // is a failed init, exactly like `Err` (self-cleaning contract:
    // on_stopped is not called). The outcome is captured instead of returned
    // directly so a failure still falls through to the shared teardown tail
    // below: the failure ack (if any) must only fire after the registry guard
    // has dropped, so an immediate same-name respawn can never race the dying
    // task and observe NameTaken (OTP 26 corpse-consumed parity).
    let init_result: Result<(), ActorError> = 'init: {
        // Phase 1: pre_start validation.
        let phase1 = match catch_callback(actor.pre_start(&mut ctx)).await {
            Ok(result) => result,
            Err(payload) => Err(ActorError::Panic(payload_into_string(payload))),
        };
        if let Err(err) = phase1 {
            break 'init Err(err);
        }

        // Phase 2: on_started initialization.
        let phase2 = match catch_callback(actor.on_started(&mut ctx)).await {
            Ok(result) => result,
            Err(payload) => Err(ActorError::Panic(payload_into_string(payload))),
        };
        if let Err(err) = phase2 {
            break 'init Err(err);
        }

        Ok(())
    };

    let mut stop_reason = match &init_result {
        Ok(()) => {
            ctx.set_status(ActorStatus::Running);
            // Success ack: right after the Running transition, before the
            // actor ever touches its mailbox.
            if let Some(tx) = ack.take() {
                let _ = tx.send(Ok(()));
            }
            StopReason::Graceful
        }
        Err(err) => {
            ctx.record_failure(err.clone());
            StopReason::Failure(err.clone())
        }
    };

    // Phase 3: Message loop - biased select! gives system messages priority.
    // Never entered when init failed: the actor must never enter the loop.
    if init_result.is_ok() {
        loop {
            tokio::select! {
                biased;

                // System channel - priority over user messages
                sys_msg = system_rx.recv() => {
                    match sys_msg {
                        Some(SystemMessage::Stop(reason)) => {
                            // Tier 3 (Kill): bypass ALL callbacks (brutal_kill parity)
                            if matches!(reason, StopReason::Kill) {
                                stop_reason = StopReason::Kill;
                                break;
                            }
                            // Tier 1 (Graceful/ParentRequest): pre_stop gate, vetoable.
                            // A pre_stop panic lets the stop proceed as a Failure.
                            if matches!(reason, StopReason::Graceful | StopReason::ParentRequest) {
                                match catch_callback(actor.pre_stop(&reason, &mut ctx)).await {
                                    Ok(true) => {
                                        stop_reason = reason;
                                        break;
                                    }
                                    Ok(false) => continue, // Actor rejected the stop
                                    Err(payload) => {
                                        stop_reason = StopReason::Failure(ActorError::Panic(
                                            payload_into_string(payload),
                                        ));
                                        break;
                                    }
                                }
                            }
                            // Tier 2 (Failure/Cancelled): non-vetoable, on_stopped still runs
                            stop_reason = reason;
                            break;
                        }
                        Some(SystemMessage::GetStatus(reply_tx)) => {
                            let info = build_status_info(&ctx);
                            let _ = reply_tx.send(info);
                        }
                        Some(SystemMessage::ChildStopped(event)) => {
                            match handle_child_stopped(&mut actor, &mut ctx, event).await {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(reason) => {
                                    stop_reason = reason;
                                    break;
                                }
                            }
                        }
                        Some(SystemMessage::RestartComplete { seq, child_id, new_system_tx, new_join }) => {
                            match on_restart_complete(&mut actor, &mut ctx, seq, child_id, new_system_tx, new_join, &system_tx).await {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(reason) => {
                                    stop_reason = reason;
                                    break;
                                }
                            }
                        }
                        None => break, // system channel closed
                    }
                }

                // User mailbox
                envelope = mailbox.recv() => {
                    match envelope {
                        Some(env) => {
                            match dispatch(&mut actor, &mut ctx, env).await {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(reason) => {
                                    stop_reason = reason;
                                    break;
                                }
                            }
                        }
                        None => break, // mailbox closed (all handles dropped)
                    }
                }
            }
        }

        // Phase 4: on_stopped (terminate/2 parity: runs on failures and panics,
        // skipped only for Kill / brutal_kill).
        if !matches!(stop_reason, StopReason::Kill) {
            ctx.set_status(ActorStatus::Stopping);
            match catch_callback(actor.on_stopped(&stop_reason, &mut ctx)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    stop_reason = StopReason::Failure(err);
                }
                Err(payload) => {
                    let panic_err = ActorError::Panic(payload_into_string(payload));
                    ctx.record_failure(panic_err.clone());
                    // A cleanup panic must not mask the original failure.
                    if !matches!(stop_reason, StopReason::Failure(_)) {
                        stop_reason = StopReason::Failure(panic_err);
                    }
                }
            }
        }
    } // init_result.is_ok(): Phase 3 (loop) + Phase 4 (on_stopped)

    // Close the system receiver BEFORE stopping children: late system senders
    // (watchers of already-dead children, escalation timers, get_status
    // callers) fail fast instead of parking, which keeps the watcher awaits
    // in stop_all_children deadlock-free by construction.
    drop(system_rx);

    // Phase 5: Stop all children (reverse start order, honoring Shutdown).
    stop_all_children(&mut ctx).await;

    ctx.set_status(ActorStatus::Stopped);

    #[cfg(feature = "tracing")]
    tracing::info!(
        actor_id = %ctx.actor_id(),
        reason = %stop_reason,
        "Actor stopped"
    );

    // Held for its Drop impl. RegistryGuard::drop unregisters the actor.
    // Tokio guarantees this task's completion is observable (by the watcher)
    // only after all drops have finished - the registered name is free before
    // the parent can act on the death.
    drop(registry_guard);

    // The failure ack (if any) fires only now, after the registry guard has
    // dropped: an immediate same-name respawn from the caller can never
    // observe NameTaken (OTP 26 corpse-consumed parity).
    if let Err(err) = init_result {
        if let Some(tx) = ack {
            let _ = tx.send(Err(err));
        }
    }

    stop_reason
}

// ---------------------------------------------------------------------------
// Dispatch (user messages only - Stop moved to system channel)
// ---------------------------------------------------------------------------

async fn dispatch<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    envelope: ActorEnvelope<A>,
) -> ControlFlow<StopReason> {
    match envelope {
        Envelope::Message { payload, responder } => {
            match catch_callback(actor.handle(payload, ctx)).await {
                Ok(Ok(response)) => {
                    if let Some(tx) = responder {
                        let _ = tx.send(Ok(response));
                    }
                    ControlFlow::Continue(())
                }
                Ok(Err(err)) => {
                    // Cast-exception parity: an `Err` from `handle` stops
                    // the actor identically on the notify and send paths. The
                    // send path additionally has a caller to notify, so that
                    // delivery happens first when a responder is present.
                    if let Some(tx) = responder {
                        let _ = tx.send(Err(err.clone()));
                    }
                    ControlFlow::Break(StopReason::Failure(err))
                }
                Err(payload) => {
                    // A panic crashes the actor on BOTH paths exactly like a
                    // returned `Err` does (cast-exception parity, same as the
                    // arm above): the only difference is the panic payload
                    // has to be caught and wrapped here instead of being
                    // handed back verbatim. A send() caller still receives it
                    // as a matchable error before the actor stops.
                    let err = ActorError::Panic(payload_into_string(payload));
                    if let Some(tx) = responder {
                        let _ = tx.send(Err(err.clone()));
                    }
                    ControlFlow::Break(StopReason::Failure(err))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Child supervision
// ---------------------------------------------------------------------------

/// Handles child-death events: stale-incarnation filtering, group-restart
/// bookkeeping, strategy evaluation, restart initiation, and the
/// `on_child_stopped` callback.
///
/// Runs as a worklist so that a completing group restart can drain the
/// failure events that were queued while the group was pending.
async fn handle_child_stopped<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    event: ChildStoppedInternal,
) -> ControlFlow<StopReason> {
    let mut work: VecDeque<ChildStoppedInternal> = VecDeque::new();
    work.push_back(event);

    while let Some(ev) = work.pop_front() {
        // Stale-incarnation filter: a superseded instance's death is not a
        // death of the child spec the registry currently tracks.
        let stale = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&ev.child_id))
            .map(|child| !child.accepts_incarnation(ev.incarnation))
            .unwrap_or(false);
        if stale {
            continue;
        }

        // Within a supervision tree an aborted task can only be our own Kill
        // escalation (AbortHandles are never exposed) - present it truthfully.
        let ev = {
            let known = ctx
                .supervision_ref()
                .map(|sup| sup.registry.get(&ev.child_id).is_some())
                .unwrap_or(false);
            if known && matches!(ev.reason, StopReason::Cancelled) {
                ChildStoppedInternal {
                    reason: StopReason::Kill,
                    ..ev
                }
            } else {
                ev
            }
        };

        // Manual-stop path: stop_child / terminate_child deaths bypass
        // strategy evaluation entirely - a manual stop is never a failure
        // (no budget charge, no sibling restarts; OTP terminate_child).
        let manual = ctx
            .supervision_mut()
            .and_then(|sup| sup.registry.get_mut(&ev.child_id))
            .and_then(|child| {
                let taken = child.manual_stop.take();
                if taken.is_some() {
                    child.is_alive = false;
                }
                taken
            });
        if let Some(kind) = manual {
            let simple = ctx
                .supervision_ref()
                .map(|sup| matches!(sup.config.strategy, RestartStrategy::SimpleOneForOne))
                .unwrap_or(false);
            let restart_type = ctx
                .supervision_ref()
                .and_then(|sup| sup.registry.get(&ev.child_id))
                .map(|child| child.spec.restart_type);
            let action = match kind {
                ManualStop::Terminate => {
                    prune_spec_if_needed(ctx, &ev.child_id);
                    SupervisionAction::Removed
                }
                ManualStop::Bounce if simple => {
                    // A manually stopped SimpleOneForOne child has its
                    // dynamic spec removed entirely.
                    if let Some(sup) = ctx.supervision_mut() {
                        sup.registry.remove(&ev.child_id);
                        sup.restart_fns.remove(&ev.child_id);
                    }
                    SupervisionAction::Removed
                }
                ManualStop::Bounce if matches!(restart_type, Some(RestartType::Permanent)) => {
                    // Budget-FREE restart: "manual stop is not a failure".
                    if let Some(sup) = ctx.supervision_mut() {
                        sup.initiate(&ev.child_id);
                    }
                    SupervisionAction::RestartInitiated
                }
                ManualStop::Bounce => {
                    // Transient (ParentRequest is clean) / Temporary stay down.
                    prune_spec_if_needed(ctx, &ev.child_id);
                    SupervisionAction::Removed
                }
            };
            let child_name = ctx
                .supervision_ref()
                .and_then(|sup| sup.registry.get(&ev.child_id))
                .and_then(|child| child.name.clone());
            let child_event = ChildEvent {
                child_id: ev.child_id.clone(),
                child_name,
                reason: ev.reason.clone(),
                action,
            };
            if let Err(payload) = catch_callback(actor.on_child_stopped(&child_event, ctx)).await {
                return ControlFlow::Break(StopReason::Failure(ActorError::Panic(
                    payload_into_string(payload),
                )));
            }
            continue;
        }

        // Group-membership path: deaths belonging to an in-flight
        // OneForAll/RestForOne restart (either phase).
        let mut member_of_group = false;
        let mut retry_front = false;
        let mut begin_chain: Option<Vec<ActorId>> = None;
        if let Some(sup) = ctx.supervision_mut() {
            match sup.pending_group.as_mut() {
                Some(GroupPhase::Stopping(group)) => {
                    if group.awaiting.remove(&ev.child_id) {
                        member_of_group = true;
                        if let Some(child) = sup.registry.get_mut(&ev.child_id) {
                            child.is_alive = false;
                        }
                        if group.awaiting.is_empty() {
                            begin_chain = match sup.pending_group.take() {
                                Some(GroupPhase::Stopping(group)) => Some(group.restart_order),
                                _ => None,
                            };
                        }
                    } else {
                        // Independent failure while a group is pending: defer
                        // its evaluation until the group completes (FIFO).
                        sup.queued_triggers.push_back(ev);
                        continue;
                    }
                }
                Some(GroupPhase::Restarting(queue)) => {
                    if queue.front() == Some(&ev.child_id) {
                        // The in-flight member's restart failed (factory
                        // panic, spawn error, or the fresh incarnation died
                        // before adoption): retry it, charging the budget
                        // (OTP try_again_restart parity).
                        member_of_group = true;
                        retry_front = true;
                        if let Some(child) = sup.registry.get_mut(&ev.child_id) {
                            child.is_alive = false;
                        }
                    } else {
                        sup.queued_triggers.push_back(ev);
                        continue;
                    }
                }
                None => {}
            }
        }

        if member_of_group {
            let mut action = SupervisionAction::RestartInitiated;
            if retry_front {
                let budget_ok = ctx
                    .supervision_mut()
                    .map(|sup| sup.budget.check_and_record())
                    .unwrap_or(false);
                if budget_ok {
                    initiate_restart(ctx, &ev.child_id);
                } else {
                    action = SupervisionAction::BudgetExhausted;
                }
            } else if let Some(order) = begin_chain {
                // Group fully stopped: restart SEQUENTIALLY in start order.
                start_restart_chain(ctx, order, &mut work);
            }
            let child_name = ctx
                .supervision_ref()
                .and_then(|sup| sup.registry.get(&ev.child_id))
                .and_then(|child| child.name.clone());
            let child_event = ChildEvent {
                child_id: ev.child_id.clone(),
                child_name,
                reason: ev.reason.clone(),
                action,
            };
            if let Err(payload) = catch_callback(actor.on_child_stopped(&child_event, ctx)).await {
                return ControlFlow::Break(StopReason::Failure(ActorError::Panic(
                    payload_into_string(payload),
                )));
            }
            if matches!(action, SupervisionAction::BudgetExhausted) {
                ctx.record_failure(SupervisionError::BudgetExhausted.into());
                return ControlFlow::Break(StopReason::ParentRequest);
            }
            continue;
        }

        // Normal path: mark dead, evaluate the strategy, act.
        let child_name = if let Some(sup) = ctx.supervision_mut() {
            if let Some(child) = sup.registry.get_mut(&ev.child_id) {
                child.is_alive = false;
                child.name.clone()
            } else {
                None
            }
        } else {
            None
        };

        let outcome = ctx
            .supervision_mut()
            .map(|sup| evaluate_strategy(sup, &ev.child_id, &ev.reason));

        let action = match outcome {
            None => SupervisionAction::NotSupervised,
            Some(StrategyOutcome::RestartOne(id)) => {
                initiate_restart(ctx, &id);
                SupervisionAction::RestartInitiated
            }
            Some(StrategyOutcome::RestartGroup {
                stop_reverse,
                restart_order,
            }) => {
                if stop_reverse.is_empty() {
                    // No live members to stop: go straight to the sequential
                    // restart chain.
                    start_restart_chain(ctx, restart_order, &mut work);
                } else {
                    begin_group_stop(ctx, &stop_reverse).await;
                    if let Some(sup) = ctx.supervision_mut() {
                        sup.pending_group = Some(GroupPhase::Stopping(GroupRestart {
                            awaiting: stop_reverse.iter().cloned().collect(),
                            restart_order,
                        }));
                    }
                }
                SupervisionAction::RestartInitiated
            }
            Some(StrategyOutcome::Remove) => {
                prune_spec_if_needed(ctx, &ev.child_id);
                SupervisionAction::Removed
            }
            Some(StrategyOutcome::BudgetExhausted) => SupervisionAction::BudgetExhausted,
        };

        let child_event = ChildEvent {
            child_id: ev.child_id.clone(),
            child_name,
            reason: ev.reason.clone(),
            action,
        };
        if let Err(payload) = catch_callback(actor.on_child_stopped(&child_event, ctx)).await {
            return ControlFlow::Break(StopReason::Failure(ActorError::Panic(
                payload_into_string(payload),
            )));
        }

        if matches!(action, SupervisionAction::BudgetExhausted) {
            ctx.record_failure(SupervisionError::BudgetExhausted.into());
            // OTP parity: an intensity-exceeded supervisor exits with
            // `shutdown` (our ParentRequest), so its own supervisor's
            // Transient policy does not restart it. Direct break: not
            // vetoable by pre_stop.
            return ControlFlow::Break(StopReason::ParentRequest);
        }
    }

    ControlFlow::Continue(())
}

/// Starts the sequential restart chain for a completed group: sets the
/// Restarting phase and initiates the FIRST member; each subsequent member is
/// initiated by [`on_restart_complete`] when its predecessor is adopted
/// (sequential registration, OTP left-to-right). An empty order just drains
/// the queued triggers.
fn start_restart_chain<A: Actor>(
    ctx: &mut ActorContext<A>,
    order: Vec<ActorId>,
    work: &mut VecDeque<ChildStoppedInternal>,
) {
    let queue: VecDeque<ActorId> = order.into();
    match queue.front().cloned() {
        Some(front) => {
            if let Some(sup) = ctx.supervision_mut() {
                sup.pending_group = Some(GroupPhase::Restarting(queue));
            }
            initiate_restart(ctx, &front);
        }
        None => {
            if let Some(sup) = ctx.supervision_mut() {
                sup.pending_group = None;
                while let Some(queued) = sup.queued_triggers.pop_front() {
                    work.push_back(queued);
                }
            }
        }
    }
}

/// Handles a `RestartComplete`: validates the seq token, adopts the new
/// incarnation (spawning its watcher), advances a sequential group-restart
/// chain, and - when the chain completes - processes the failure events that
/// were queued while the group was in flight.
async fn on_restart_complete<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    seq: u64,
    child_id: ActorId,
    new_system_tx: mpsc::Sender<SystemMessage>,
    new_join: JoinHandle<StopReason>,
    system_tx: &mpsc::Sender<SystemMessage>,
) -> ControlFlow<StopReason> {
    let accepted = ctx
        .supervision_ref()
        .and_then(|sup| sup.registry.get(&child_id))
        .map(|child| child.pending_restart_seq == Some(seq))
        .unwrap_or(false);
    if !accepted {
        // Superseded restart (a newer seq is pending or the spec is gone):
        // kill the stray instance instead of leaking a duplicate. Its death
        // event would carry a stale incarnation and be ignored anyway.
        let _ = new_system_tx
            .send(SystemMessage::Stop(StopReason::Kill))
            .await;
        drop(new_join);
        return ControlFlow::Continue(());
    }

    // The parent spawns the watcher itself so watcher events can never outrun
    // this message on the channel (the incarnation is adopted before its
    // death could possibly be observed). The abort handle is taken first -
    // the Kill escalation backstop for this incarnation.
    let abort = new_join.abort_handle();
    let watcher = spawn_watcher(child_id.clone(), seq, new_join, system_tx.clone());

    let mut next_in_chain: Option<ActorId> = None;
    let mut drained: Vec<ChildStoppedInternal> = Vec::new();
    if let Some(sup) = ctx.supervision_mut() {
        sup.registry
            .update_restarted(&child_id, seq, new_system_tx, watcher, abort);
        if let Some(GroupPhase::Restarting(queue)) = sup.pending_group.as_mut() {
            if queue.front() == Some(&child_id) {
                queue.pop_front();
                match queue.front().cloned() {
                    Some(next) => next_in_chain = Some(next),
                    None => {
                        sup.pending_group = None;
                        drained.extend(sup.queued_triggers.drain(..));
                    }
                }
            }
        }
    }

    if let Some(next) = next_in_chain {
        initiate_restart(ctx, &next);
    }
    for ev in drained {
        if let ControlFlow::Break(reason) = handle_child_stopped(actor, ctx, ev).await {
            return ControlFlow::Break(reason);
        }
    }
    ControlFlow::Continue(())
}

/// Sends stop signals to the live members of a group restart, in reverse
/// start order, honoring each child's `Shutdown` policy. `Timeout` arms a
/// fire-and-forget escalation timer that sends Kill at expiry (a no-op if the
/// child already stopped: its channel is closed by then).
async fn begin_group_stop<A: Actor>(ctx: &mut ActorContext<A>, stop_reverse: &[ActorId]) {
    let mut targets: Vec<(mpsc::Sender<SystemMessage>, Shutdown, AbortHandle)> = Vec::new();
    if let Some(sup) = ctx.supervision_ref() {
        for id in stop_reverse {
            if let Some(child) = sup.registry.get(id) {
                targets.push((
                    child.system_tx.clone(),
                    child.spec.shutdown,
                    child.abort.clone(),
                ));
            }
        }
    }
    for (tx, shutdown, abort) in targets {
        match shutdown {
            Shutdown::Kill => {
                let _ = tx.send(SystemMessage::Stop(StopReason::Kill)).await;
                spawn_grace_abort(abort);
            }
            Shutdown::Timeout(after) => {
                let _ = tx
                    .send(SystemMessage::Stop(StopReason::ParentRequest))
                    .await;
                let escalate = tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    // No-op if the child already stopped (channel closed /
                    // task finished).
                    let _ = escalate.send(SystemMessage::Stop(StopReason::Kill)).await;
                    tokio::time::sleep(KILL_GRACE).await;
                    abort.abort();
                });
            }
            Shutdown::Infinity => {
                let _ = tx
                    .send(SystemMessage::Stop(StopReason::ParentRequest))
                    .await;
            }
        }
    }
}

/// Arms the abort() backstop behind an already-sent Kill: after the grace, a
/// task that has not terminated cooperatively is aborted. Harmless if the
/// task already finished.
fn spawn_grace_abort(abort: AbortHandle) {
    tokio::spawn(async move {
        tokio::time::sleep(KILL_GRACE).await;
        abort.abort();
    });
}

/// Prunes a child's spec after a non-restart outcome when OTP says so:
/// Temporary specs are deleted as soon as the process terminates, and
/// SimpleOneForOne removes dynamic specs likewise. A transient child stopped
/// cleanly keeps its spec (is_alive=false, the pid=undefined analog) for
/// restart_child.
fn prune_spec_if_needed<A: Actor>(ctx: &mut ActorContext<A>, child_id: &ActorId) {
    if let Some(sup) = ctx.supervision_mut() {
        let prune = matches!(sup.config.strategy, RestartStrategy::SimpleOneForOne)
            || sup
                .registry
                .get(child_id)
                .map(|c| matches!(c.spec.restart_type, RestartType::Temporary))
                .unwrap_or(false);
        if prune {
            sup.registry.remove(child_id);
            sup.restart_fns.remove(child_id);
        }
    }
}

/// Initiates a non-blocking restart for a child using its stored restart
/// closure (which captured the child's id, name, and config by value).
fn initiate_restart<A: Actor>(ctx: &mut ActorContext<A>, child_id: &ActorId) {
    if let Some(sup) = ctx.supervision_mut() {
        sup.initiate(child_id);
    }
}

/// Stops all children in reverse start order, respecting their Shutdown
/// policies. Awaits each child's WATCHER handle: watcher completion implies
/// the child task has fully terminated. `Timeout` escalates to Kill at
/// expiry. Deadlock-free because the caller drops its system receiver first
/// (parked watcher sends fail fast).
async fn stop_all_children<A: Actor>(ctx: &mut ActorContext<A>) {
    let children = match ctx.supervision_mut() {
        Some(sup) => {
            sup.restart_fns.clear();
            sup.pending_group = None;
            sup.queued_triggers.clear();
            sup.registry.drain_all()
        }
        None => return,
    };

    // Stop in reverse order (last started = first stopped)
    for child in children.into_iter().rev() {
        if !child.is_alive {
            continue;
        }

        let mut watcher = child.watcher_handle;
        let abort = child.abort;
        match child.spec.shutdown {
            Shutdown::Kill => {
                let _ = child
                    .system_tx
                    .send(SystemMessage::Stop(StopReason::Kill))
                    .await;
                if tokio::time::timeout(KILL_GRACE, &mut watcher)
                    .await
                    .is_err()
                {
                    abort.abort();
                    // Final bound: only a non-yielding loop survives abort;
                    // detach rather than hang the parent's shutdown.
                    let _ = tokio::time::timeout(KILL_GRACE, &mut watcher).await;
                }
            }
            Shutdown::Timeout(after) => {
                let _ = child
                    .system_tx
                    .send(SystemMessage::Stop(StopReason::ParentRequest))
                    .await;
                if tokio::time::timeout(after, &mut watcher).await.is_err() {
                    // Escalate: a vetoing or slow child is brutally killed
                    // (OTP: exit(Child, kill) after the shutdown timeout),
                    // with the abort() backstop behind it.
                    let _ = child
                        .system_tx
                        .send(SystemMessage::Stop(StopReason::Kill))
                        .await;
                    if tokio::time::timeout(KILL_GRACE, &mut watcher)
                        .await
                        .is_err()
                    {
                        abort.abort();
                        let _ = tokio::time::timeout(KILL_GRACE, &mut watcher).await;
                    }
                }
            }
            Shutdown::Infinity => {
                let _ = child
                    .system_tx
                    .send(SystemMessage::Stop(StopReason::ParentRequest))
                    .await;
                let _ = watcher.await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

fn build_status_info<A: Actor>(ctx: &ActorContext<A>) -> ActorStatusInfo {
    let (child_count, name) = ctx
        .supervision_ref()
        .map(|sup| (sup.registry.len(), ctx.actor_name().cloned()))
        .unwrap_or((0, ctx.actor_name().cloned()));

    ActorStatusInfo {
        id: ctx.actor_id().clone(),
        name,
        status: ctx.status(),
        mailbox_len: ctx.self_handle().mailbox_len(),
        mailbox_capacity: ctx.self_handle().mailbox_capacity(),
        child_count,
        timer_count: ctx.active_timer_count(),
        stream_count: ctx.active_stream_count(),
    }
}
