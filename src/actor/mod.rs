//! Core actor traits and identity types.

/// Actor execution context.
pub mod context;
/// Actor handle for external communication.
pub mod handle;
/// Runtime configuration and spawning logic.
pub mod runtime;

use async_trait::async_trait;

use crate::error::{ActorError, ActorResult};
use crate::types::{ActorId, Envelope, StopReason};
use context::ActorContext;
use runtime::ActorConfig;

/// Primary trait implemented by all actors.
///
/// An actor is a stateful entity that processes messages sequentially.
/// Each actor runs in its own Tokio task.
#[async_trait]
pub trait Actor: Sized + Send + 'static {
    /// The type of message this actor receives.
    type Message: Send + 'static;

    /// The type of response this actor produces.
    ///
    /// Use `()` if the actor does not return a response.
    type Response: Send + 'static;

    /// Called when the actor is started, before processing any messages.
    ///
    /// Use this to initialize state, schedule timers, or spawn child actors.
    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    /// Handles a message sent to the actor.
    ///
    /// This method is called sequentially for each message in the mailbox.
    async fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response>;

    /// Called when the actor is stopping.
    ///
    /// Use this to clean up resources, close connections, or notify other actors.
    async fn on_stopped(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    /// Called when a child actor (spawned by this actor) stops.
    async fn on_child_stopped(
        &mut self,
        _child_id: &ActorId,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        Ok(())
    }

    /// Called when a message handler returns an error during a `notify` (fire-and-forget) call.
    ///
    /// Since `notify` does not wait for a response, this hook allows the actor to log or handle
    /// the failure internally.
    fn handle_failure(&mut self, _error: ActorError) {}
}

/// Type alias tying the strongly typed envelope to a concrete actor.
pub type ActorEnvelope<A> = Envelope<<A as Actor>::Message, <A as Actor>::Response>;

/// Helper trait for flexible ActorConfig parameter.
///
/// This allows passing `()`, `None`, `ActorConfig::default()`, or `&config` to `spawn_actor`.
pub trait IntoActorConfig {
    /// Converts the value into an `ActorConfig`.
    fn into_config(self) -> ActorConfig;
}

impl IntoActorConfig for ActorConfig {
    fn into_config(self) -> ActorConfig {
        self
    }
}

impl IntoActorConfig for &ActorConfig {
    fn into_config(self) -> ActorConfig {
        self.clone()
    }
}

impl IntoActorConfig for Option<ActorConfig> {
    fn into_config(self) -> ActorConfig {
        self.unwrap_or_default()
    }
}

impl IntoActorConfig for () {
    fn into_config(self) -> ActorConfig {
        ActorConfig::default()
    }
}

/// Convenience trait for spawning actors directly from their implementations.
#[async_trait]
pub trait ActorExt: Actor + Sized {
    /// Consumes the actor, spawns it on the current Tokio runtime, and returns its handle.
    ///
    /// The config parameter is optional - pass `None`, `()`, `ActorConfig::default()`, or `&config`.
    ///
    /// # Example
    /// ```no_run
    /// # use tokio_actors::*;
    /// # use tokio_actors::actor::*;
    /// # use async_trait::async_trait;
    /// # struct MyActor;
    /// # #[async_trait]
    /// # impl Actor for MyActor {
    /// #     type Message = ();
    /// #     type Response = ();
    /// #     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    /// # }
    /// # async fn run() {
    /// let handle = MyActor.spawn_actor("my-actor", ()).await.unwrap();
    /// # }
    /// ```
    async fn spawn_actor(
        self,
        id: impl Into<ActorId> + Send,
        config: impl IntoActorConfig + Send,
    ) -> Result<handle::ActorHandle<Self>, crate::error::SpawnError> {
        runtime::into_actor(id, self, config.into_config())
    }
}

impl<T> ActorExt for T where T: Actor {}
