use std::ops::ControlFlow;
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::actor::supervision::{
    evaluate_strategy, StrategyOutcome, SupervisionConfig, SupervisionState,
};
use crate::actor::{context::ActorContext, handle::ActorHandle, Actor, ActorEnvelope};
use crate::error::SpawnError;
use crate::system::{ActorSystem, RegistryGuard};
use crate::types::{
    ActorId, ActorStatus, ActorStatusInfo, ChildEvent, ChildStoppedInternal, Envelope, Shutdown,
    StopReason, SupervisionAction, SystemMessage,
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

    /// Enables supervision with default configuration (OneForOne, 3 restarts / 5s).
    pub fn supervised(mut self) -> Self {
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

pub(crate) fn into_actor<A: Actor>(
    id: impl Into<ActorId>,
    actor: A,
    config: impl Into<ActorConfig>,
    name: Option<String>,
    system: Option<Arc<ActorSystem>>,
) -> Result<ActorHandle<A>, SpawnError> {
    let (handle, _join) = spawn_actor(id.into(), actor, config.into(), name, system, None)?;
    Ok(handle)
}

/// Spawns an actor, returning both the handle and the JoinHandle for the task.
///
/// `parent_system_tx` is set when this actor is spawned as a supervised child. The
/// runtime will send `ChildStopped` events through this channel when the actor stops.
pub(crate) fn spawn_actor<A: Actor>(
    id: ActorId,
    actor: A,
    config: ActorConfig,
    name: Option<String>,
    system: Option<Arc<ActorSystem>>,
    parent_system_tx: Option<mpsc::Sender<SystemMessage>>,
) -> Result<(ActorHandle<A>, JoinHandle<()>), SpawnError> {
    let handle = Handle::try_current().map_err(|_| SpawnError::MissingRuntime)?;
    let mailbox_capacity = config.mailbox.capacity;
    let (tx, rx) = mpsc::channel(mailbox_capacity);
    let (system_tx, system_rx) = mpsc::channel::<SystemMessage>(64);
    let actor_handle = ActorHandle::new(id.clone(), tx, system_tx.clone(), mailbox_capacity);

    let supervision = config.supervision.map(SupervisionState::new);
    let context = ActorContext::new(
        id.clone(),
        actor_handle.clone(),
        handle.clone(),
        system_tx,
        system.clone(),
        name.clone(),
        supervision,
    );

    // Register in the target system when a name or explicit system is provided.
    let guard = if name.is_some() || system.is_some() {
        let target = system.unwrap_or_else(ActorSystem::default);
        target.register_actor::<A>(&id, name.as_deref(), &actor_handle)?;
        Some(RegistryGuard::new(target, id, name))
    } else {
        None
    };

    let join = handle.spawn(run_actor(
        actor,
        context,
        rx,
        system_rx,
        parent_system_tx,
        guard,
    ));

    Ok((actor_handle, join))
}

// ---------------------------------------------------------------------------
// Actor run loop
// ---------------------------------------------------------------------------

async fn run_actor<A: Actor>(
    mut actor: A,
    mut ctx: ActorContext<A>,
    mut mailbox: mpsc::Receiver<ActorEnvelope<A>>,
    mut system_rx: mpsc::Receiver<SystemMessage>,
    parent_system_tx: Option<mpsc::Sender<SystemMessage>>,
    registry_guard: Option<RegistryGuard>,
) {
    // Phase 1: pre_start validation (fail-fast gate)
    if let Err(err) = actor.pre_start(&mut ctx).await {
        ctx.record_failure(err.clone());
        ctx.set_status(ActorStatus::Stopped);
        notify_parent(&parent_system_tx, ctx.actor_id(), StopReason::Failure(err));
        return;
    }

    // Phase 2: on_started initialization
    let mut stop_reason = match actor.on_started(&mut ctx).await {
        Ok(()) => {
            ctx.set_status(ActorStatus::Running);
            StopReason::Graceful
        }
        Err(err) => {
            ctx.record_failure(err.clone());
            ctx.set_status(ActorStatus::Stopped);
            notify_parent(&parent_system_tx, ctx.actor_id(), StopReason::Failure(err));
            return;
        }
    };

    // Phase 3: Message loop - biased select! gives system messages priority
    loop {
        tokio::select! {
            biased;

            // System channel - priority over user messages
            sys_msg = system_rx.recv() => {
                match sys_msg {
                    Some(SystemMessage::Stop(reason)) => {
                        // Tier 3 (Kill): bypass ALL callbacks, stop children, return
                        if matches!(reason, StopReason::Kill) {
                            ctx.set_status(ActorStatus::Stopped);
                            stop_all_children(&mut ctx).await;
                            notify_parent(&parent_system_tx, ctx.actor_id(), reason);
                            drop(registry_guard);
                            return;
                        }
                        // Tier 1 (Graceful/ParentRequest): pre_stop gate, vetoable
                        if matches!(reason, StopReason::Graceful | StopReason::ParentRequest)
                            && !actor.pre_stop(&reason, &mut ctx).await
                        {
                            continue; // Actor rejected the stop
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
                        let action = handle_child_stopped(&mut actor, &mut ctx, &event).await;
                        if matches!(action, SupervisionAction::BudgetExhausted) {
                            stop_reason = StopReason::Failure(
                                crate::error::SupervisionError::BudgetExhausted.into()
                            );
                            break;
                        }
                    }
                    Some(SystemMessage::RestartComplete { seq, child_id, new_system_tx, new_join_handle }) => {
                        if let Some(sup) = ctx.supervision_mut() {
                            sup.registry.update_restarted(&child_id, seq, new_system_tx, new_join_handle);
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
                    None => break, // mailbox closed
                }
            }
        }
    }

    // Phase 4: on_stopped
    ctx.set_status(ActorStatus::Stopping);
    if let Err(err) = actor.on_stopped(&stop_reason, &mut ctx).await {
        stop_reason = StopReason::Failure(err);
    }

    // Phase 5: Stop all children (reverse start order)
    stop_all_children(&mut ctx).await;

    ctx.set_status(ActorStatus::Stopped);

    // Phase 6: Notify parent
    notify_parent(&parent_system_tx, ctx.actor_id(), stop_reason.clone());

    #[cfg(feature = "tracing")]
    tracing::info!(
        actor_id = %ctx.actor_id(),
        reason = %stop_reason,
        "Actor stopped"
    );

    #[cfg(not(feature = "tracing"))]
    let _ = stop_reason;

    // Held for its Drop impl. RegistryGuard::drop unregisters the actor.
    drop(registry_guard);
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
            let outcome = actor.handle(payload, ctx).await;
            match outcome {
                Ok(response) => {
                    if let Some(tx) = responder {
                        let _ = tx.send(Ok(response));
                    }
                    ControlFlow::Continue(())
                }
                Err(err) => {
                    if let Some(tx) = responder {
                        // For send (request-response), return error and stop actor
                        let _ = tx.send(Err(err.clone()));
                        ControlFlow::Break(StopReason::Failure(err))
                    } else {
                        // For notify (fire-and-forget), call error handler but continue
                        actor.handle_failure(err);
                        ControlFlow::Continue(())
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Child supervision helpers
// ---------------------------------------------------------------------------

/// Handles a ChildStopped event: evaluates the strategy and notifies the actor.
async fn handle_child_stopped<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    event: &ChildStoppedInternal,
) -> SupervisionAction {
    // Mark child as dead
    let child_name = if let Some(sup) = ctx.supervision_mut() {
        if let Some(child) = sup.registry.get_mut(&event.child_id) {
            child.is_alive = false;
            child.name.clone()
        } else {
            None
        }
    } else {
        None
    };

    // Evaluate strategy
    let action = if let Some(sup) = ctx.supervision_mut() {
        match evaluate_strategy(sup, &event.child_id, &event.reason) {
            StrategyOutcome::Restart(to_restart) => {
                // Initiate restarts for all children in the list
                for id in &to_restart {
                    initiate_restart(ctx, id);
                }
                SupervisionAction::Restarted
            }
            StrategyOutcome::Remove => {
                if let Some(sup) = ctx.supervision_mut() {
                    // For SimpleOneForOne, remove the child spec entirely
                    if matches!(
                        sup.config.strategy,
                        crate::types::RestartStrategy::SimpleOneForOne
                    ) {
                        sup.registry.remove(&event.child_id);
                    }
                }
                SupervisionAction::Removed
            }
            StrategyOutcome::BudgetExhausted => SupervisionAction::BudgetExhausted,
        }
    } else {
        SupervisionAction::NotSupervised
    };

    // Notify actor via on_child_stopped
    let child_event = ChildEvent {
        child_id: event.child_id.clone(),
        child_name,
        reason: event.reason.clone(),
        action,
    };
    let _ = actor.on_child_stopped(&child_event, ctx).await;

    action
}

/// Initiates a non-blocking restart for a child.
fn initiate_restart<A: Actor>(ctx: &mut ActorContext<A>, child_id: &ActorId) {
    let sup = match ctx.supervision_mut() {
        Some(s) => s,
        None => return,
    };

    // Get seq first, then mutate child
    let seq = sup.registry.next_seq();

    let child = match sup.registry.get_mut(child_id) {
        Some(c) => c,
        None => return,
    };

    child.pending_restart_seq = Some(seq);
    child.is_alive = false;
    let child_name = child.name.clone();

    // Look up the type-erased restart function
    if let Some(restart_fn) = sup.restart_fns.get(child_id) {
        let fut = restart_fn(seq, child_name);
        tokio::spawn(fut);
    }
}

/// Stops all children in reverse start order, respecting their Shutdown policies.
async fn stop_all_children<A: Actor>(ctx: &mut ActorContext<A>) {
    let children = match ctx.supervision_mut() {
        Some(sup) => sup.registry.drain_all(),
        None => return,
    };

    // Stop in reverse order (last started = first stopped)
    for child in children.into_iter().rev() {
        if !child.is_alive {
            continue;
        }

        match child.spec.shutdown {
            Shutdown::Kill => {
                let _ = child
                    .system_tx
                    .send(SystemMessage::Stop(StopReason::Kill))
                    .await;
            }
            Shutdown::Timeout(duration) => {
                let _ = child
                    .system_tx
                    .send(SystemMessage::Stop(StopReason::ParentRequest))
                    .await;
                let _ = tokio::time::timeout(duration, child.join_handle).await;
            }
            Shutdown::Infinity => {
                let _ = child
                    .system_tx
                    .send(SystemMessage::Stop(StopReason::ParentRequest))
                    .await;
                let _ = child.join_handle.await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

fn notify_parent(
    parent_tx: &Option<mpsc::Sender<SystemMessage>>,
    actor_id: &ActorId,
    reason: StopReason,
) {
    if let Some(tx) = parent_tx {
        let event = ChildStoppedInternal {
            child_id: actor_id.clone(),
            reason,
        };
        let _ = tx.try_send(SystemMessage::ChildStopped(event));
    }
}

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
