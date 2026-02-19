//! Core actor traits and identity types.

/// Actor execution context.
pub mod context;
/// Actor handle for external communication.
pub mod handle;
/// Runtime configuration and spawning logic.
pub mod runtime;

use std::future::Future;
use std::sync::Arc;

use crate::error::{ActorError, ActorResult};
use crate::system::ActorSystem;
use crate::types::{ActorId, Envelope, StopReason};
use context::ActorContext;
use runtime::ActorConfig;

/// Primary trait implemented by all actors.
///
/// An actor is a stateful entity that processes messages sequentially.
/// Each actor runs in its own Tokio task.
pub trait Actor: Sized + Send + 'static {
    /// The type of message this actor receives.
    type Message: Send + 'static;

    /// The type of response this actor produces.
    ///
    /// Use `()` if the actor does not return a response.
    type Response: Send + 'static;

    /// Phase 1: Validation gate — return Err to prevent actor from starting.
    /// OTP equivalent: `init/1` returning `{stop, Reason}`.
    ///
    /// Implementations that acquire resources must clean up before returning `Err`
    /// (self-cleaning pattern), as `on_stopped` will not be called on init failure.
    fn pre_start(
        &mut self,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
    }

    /// Phase 2: Post-init setup — schedule timers, spawn children, acquire resources.
    /// OTP equivalent: `init/1` returning `{ok, State}`.
    ///
    /// Called after `pre_start` succeeds, before the actor enters the message loop.
    fn on_started(
        &mut self,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
    }

    /// Handles a message sent to the actor.
    ///
    /// This method is called sequentially for each message in the mailbox.
    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<Self::Response>> + Send;

    /// Phase 4a: Stop gate — return `false` to reject graceful/parent-requested stops.
    /// Forced stops (`Failure`, `Cancelled`) bypass this hook entirely.
    ///
    /// No retry limit is enforced at this level — retry budgets and
    /// timeout policies belong in the supervision layer (v0.3).
    fn pre_stop(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = bool> + Send {
        async { true }
    }

    /// Phase 4b: Cleanup — release resources, notify peers.
    /// OTP equivalent: `terminate(Reason, State)`.
    fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
    }

    /// Called when a child actor (spawned by this actor) stops.
    fn on_child_stopped(
        &mut self,
        _child_id: &ActorId,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
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
pub trait ActorExt: Actor + Sized {
    /// Consumes the actor, spawns it on the current Tokio runtime, and returns its handle.
    ///
    /// The config parameter is optional - pass `None`, `()`, `ActorConfig::default()`, or `&config`.
    ///
    /// # Example
    /// ```no_run
    /// # use tokio_actors::*;
    /// # use tokio_actors::actor::*;
    /// # struct MyActor;
    /// # impl Actor for MyActor {
    /// #     type Message = ();
    /// #     type Response = ();
    /// #     fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> impl std::future::Future<Output = ActorResult<()>> + Send { async { Ok(()) } }
    /// # }
    /// # async fn run() {
    /// let handle = MyActor.spawn_actor("my-actor", ()).await.unwrap();
    /// # }
    /// ```
    fn spawn_actor(
        self,
        id: impl Into<ActorId> + Send,
        config: impl IntoActorConfig + Send,
    ) -> impl Future<Output = Result<handle::ActorHandle<Self>, crate::error::SpawnError>> + Send
    {
        async move { runtime::into_actor(id, self, config.into_config(), None, None) }
    }

    /// Anonymous spawn with auto-generated UUID as ActorId.
    /// No registry registration. OTP equivalent: `spawn(Fun)`.
    fn spawn(
        self,
    ) -> impl Future<Output = Result<handle::ActorHandle<Self>, crate::error::SpawnError>> + Send
    {
        async move {
            let id = uuid::Uuid::new_v4().to_string();
            runtime::into_actor(id, self, ActorConfig::default(), None, None)
        }
    }

    /// Named spawn in the default system. The name serves as ActorId.
    /// OTP equivalent: `gen_server:start_link({local, Name}, ...)`.
    fn spawn_named(
        self,
        name: impl Into<String> + Send,
    ) -> impl Future<Output = Result<handle::ActorHandle<Self>, crate::error::SpawnError>> + Send
    {
        async move {
            let name = name.into();
            runtime::into_actor(name.clone(), self, ActorConfig::default(), Some(name), None)
        }
    }

    /// Named spawn with config in the default system.
    fn spawn_named_with(
        self,
        name: impl Into<String> + Send,
        config: &ActorConfig,
    ) -> impl Future<Output = Result<handle::ActorHandle<Self>, crate::error::SpawnError>> + Send
    {
        let config = config.clone();
        async move {
            let name = name.into();
            runtime::into_actor(name.clone(), self, config, Some(name), None)
        }
    }

    /// Anonymous spawn targeting a specific system. Auto-generated UUID as ActorId.
    /// Registered in the system's `by_id` map but NOT `by_name`.
    fn spawn_on(
        self,
        system: &Arc<ActorSystem>,
    ) -> impl Future<Output = Result<handle::ActorHandle<Self>, crate::error::SpawnError>> + Send
    {
        let system = Arc::clone(system);
        async move {
            let id = uuid::Uuid::new_v4().to_string();
            runtime::into_actor(id, self, ActorConfig::default(), None, Some(system))
        }
    }

    /// Named spawn targeting a specific system. The name serves as ActorId.
    fn spawn_on_named(
        self,
        system: &Arc<ActorSystem>,
        name: impl Into<String> + Send,
    ) -> impl Future<Output = Result<handle::ActorHandle<Self>, crate::error::SpawnError>> + Send
    {
        let system = Arc::clone(system);
        async move {
            let name = name.into();
            runtime::into_actor(
                name.clone(),
                self,
                ActorConfig::default(),
                Some(name),
                Some(system),
            )
        }
    }

    /// Named spawn with config targeting a specific system.
    fn spawn_on_named_with(
        self,
        system: &Arc<ActorSystem>,
        name: impl Into<String> + Send,
        config: &ActorConfig,
    ) -> impl Future<Output = Result<handle::ActorHandle<Self>, crate::error::SpawnError>> + Send
    {
        let system = Arc::clone(system);
        let config = config.clone();
        async move {
            let name = name.into();
            runtime::into_actor(name.clone(), self, config, Some(name), Some(system))
        }
    }
}

impl<T> ActorExt for T where T: Actor {}
