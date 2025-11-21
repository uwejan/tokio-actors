use std::ops::ControlFlow;

use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::actor::{context::ActorContext, handle::ActorHandle, Actor, ActorEnvelope};
use crate::error::SpawnError;
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

    let context = ActorContext::new(id, actor_handle.clone(), handle.clone());

    handle.spawn(run_actor(actor, context, rx));

    Ok(actor_handle)
}

async fn run_actor<A: Actor>(
    mut actor: A,
    mut ctx: ActorContext<A>,
    mut mailbox: mpsc::Receiver<ActorEnvelope<A>>,
) {
    let mut stop_reason = match actor.on_started(&mut ctx).await {
        Ok(()) => {
            ctx.set_status(ActorStatus::Running);
            StopReason::Graceful
        }
        Err(err) => {
            ctx.record_failure(err.clone());
            ctx.set_status(ActorStatus::Stopped);
            return;
        }
    };

    while let Some(envelope) = mailbox.recv().await {
        match dispatch(&mut actor, &mut ctx, envelope).await {
            ControlFlow::Continue(()) => {}
            ControlFlow::Break(reason) => {
                stop_reason = reason;
                break;
            }
        }
    }

    ctx.set_status(ActorStatus::Stopping);
    if let Err(err) = actor.on_stopped(&mut ctx).await {
        stop_reason = StopReason::Failure(err);
    }
    ctx.set_status(ActorStatus::Stopped);

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
