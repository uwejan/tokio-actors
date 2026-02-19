use std::ops::ControlFlow;
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::actor::{context::ActorContext, handle::ActorHandle, Actor, ActorEnvelope};
use crate::error::SpawnError;
use crate::system::{ActorSystem, RegistryGuard};
use crate::types::{ActorId, ActorStatus, Envelope, StopReason};

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
    /// Optional name for registry registration.
    pub(crate) name: Option<String>,
    /// Target system for registration.
    pub(crate) system: Option<Arc<ActorSystem>>,
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

    /// Sets the actor name for registry registration.
    #[allow(dead_code)]
    pub(crate) fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the target system for registration.
    #[allow(dead_code)]
    pub(crate) fn with_system(mut self, system: Arc<ActorSystem>) -> Self {
        self.system = Some(system);
        self
    }
}

/// Event sent to a parent actor when one of its children stops.
///
/// This is a pre-supervision primitive — it only notifies. Supervision
/// policies (restart strategies, budgets) are v0.3 scope.
#[derive(Debug, Clone)]
pub struct ChildStoppedEvent {
    /// The ID of the child actor that stopped.
    pub child_id: ActorId,
    /// The reason the child stopped.
    pub reason: StopReason,
}

/// Type-erased parent notification channel.
///
/// When a child actor stops, the runtime calls this to inform the parent.
pub(crate) trait ParentNotifier: Send + Sync {
    /// Notify the parent that a child has stopped.
    fn notify_child_stopped(&self, child_id: ActorId, reason: StopReason);
}

/// Typed implementation that sends `ChildStoppedEvent` to the parent's mailbox.
#[allow(dead_code)]
pub(crate) struct TypedParentNotifier<P: Actor> {
    parent_handle: ActorHandle<P>,
}

#[allow(dead_code)]
impl<P: Actor> TypedParentNotifier<P>
where
    P::Message: From<ChildStoppedEvent>,
{
    pub fn new(parent_handle: ActorHandle<P>) -> Self {
        Self { parent_handle }
    }
}

impl<P: Actor> ParentNotifier for TypedParentNotifier<P>
where
    P::Message: From<ChildStoppedEvent>,
{
    fn notify_child_stopped(&self, child_id: ActorId, reason: StopReason) {
        let event = ChildStoppedEvent { child_id, reason };
        let _ = self.parent_handle.try_notify(event.into());
    }
}

pub(crate) fn into_actor<A: Actor>(
    id: impl Into<ActorId>,
    actor: A,
    config: impl Into<ActorConfig>,
) -> Result<ActorHandle<A>, SpawnError> {
    spawn_actor(id.into(), actor, config.into())
}

pub(crate) fn spawn_actor<A: Actor>(
    id: ActorId,
    actor: A,
    config: ActorConfig,
) -> Result<ActorHandle<A>, SpawnError> {
    let handle = Handle::try_current().map_err(|_| SpawnError::MissingRuntime)?;
    let mailbox_capacity = config.mailbox.capacity;
    let (tx, rx) = mpsc::channel(mailbox_capacity);
    let actor_handle = ActorHandle::new(id.clone(), tx, mailbox_capacity);

    let context = ActorContext::new(id.clone(), actor_handle.clone(), handle.clone());

    // Register in the target system if a name is configured
    let guard = if config.name.is_some() {
        let system = config.system.unwrap_or_else(ActorSystem::default);
        system.register_actor::<A>(&id, config.name.as_deref(), &actor_handle)?;
        Some(RegistryGuard::new(system, id, config.name))
    } else {
        None
    };

    handle.spawn(run_actor(actor, context, rx, None, guard));

    Ok(actor_handle)
}

async fn run_actor<A: Actor>(
    mut actor: A,
    mut ctx: ActorContext<A>,
    mut mailbox: mpsc::Receiver<ActorEnvelope<A>>,
    parent_notifier: Option<Box<dyn ParentNotifier>>,
    _registry_guard: Option<RegistryGuard>,
) {
    // Phase 1: pre_start validation (fail-fast gate)
    if let Err(err) = actor.pre_start(&mut ctx).await {
        ctx.record_failure(err.clone());
        ctx.set_status(ActorStatus::Stopped);
        if let Some(notifier) = parent_notifier {
            notifier.notify_child_stopped(ctx.actor_id().clone(), StopReason::Failure(err));
        }
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
            if let Some(notifier) = parent_notifier {
                notifier.notify_child_stopped(ctx.actor_id().clone(), StopReason::Failure(err));
            }
            return;
        }
    };

    // Phase 3: Message loop with 3-tier stop handling
    'msg: while let Some(envelope) = mailbox.recv().await {
        match dispatch(&mut actor, &mut ctx, envelope).await {
            ControlFlow::Continue(()) => {}
            ControlFlow::Break(reason) => {
                // Tier 3 (Kill): bypass ALL callbacks
                if matches!(reason, StopReason::Kill) {
                    ctx.set_status(ActorStatus::Stopped);
                    if let Some(notifier) = parent_notifier {
                        notifier.notify_child_stopped(ctx.actor_id().clone(), reason);
                    }
                    return; // Skip on_stopped entirely
                }
                // Tier 1 (Graceful/ParentRequest): pre_stop gate, vetoable
                if matches!(reason, StopReason::Graceful | StopReason::ParentRequest)
                    && !actor.pre_stop(&reason, &mut ctx).await
                {
                    continue 'msg; // Actor rejected the stop
                }
                // Tier 2 (Failure/Cancelled): non-vetoable, on_stopped still runs
                stop_reason = reason;
                break;
            }
        }
    }

    // Phase 4: Shutdown
    ctx.set_status(ActorStatus::Stopping);
    if let Err(err) = actor.on_stopped(&stop_reason, &mut ctx).await {
        stop_reason = StopReason::Failure(err);
    }
    ctx.set_status(ActorStatus::Stopped);

    // Phase 5: Notify parent
    if let Some(notifier) = parent_notifier {
        notifier.notify_child_stopped(ctx.actor_id().clone(), stop_reason.clone());
    }

    #[cfg(feature = "tracing")]
    tracing::info!(
        actor_id = %ctx.actor_id(),
        reason = %stop_reason,
        "Actor stopped"
    );

    #[cfg(not(feature = "tracing"))]
    let _ = stop_reason;
}

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
        Envelope::Stop(reason) => ControlFlow::Break(reason),
    }
}
