//! Core actor traits and identity types.

/// Actor execution context.
pub mod context;
/// Actor handle for external communication.
pub mod handle;
/// Panic capture for actor callbacks.
pub(crate) mod panic;
/// Runtime configuration and spawning logic.
pub mod runtime;
/// Supervision configuration and child management.
pub mod supervision;

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::error::{ActorError, ActorResult, SpawnError};
use crate::system::ActorSystem;
use crate::types::{ChildEvent, Envelope, StopReason};
use context::ActorContext;
use runtime::ActorConfig;
use supervision::{SupervisionConfig, KILL_GRACE};

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

    /// Phase 1: Validation gate. Return Err to prevent actor from starting.
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

    /// Phase 2: Post-init setup. Schedule timers, spawn children, acquire resources.
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
    ///
    /// # The `Err` contract
    ///
    /// A recoverable, expected condition is an `Ok`-encoded domain value in
    /// `Self::Response`, not an `Err`. Returning `Err` means this actor's
    /// invariants are broken: it stops with `StopReason::Failure` on BOTH the
    /// `send` and `notify` paths (cast-exception parity, matching OTP: an
    /// uncaught exception kills the process the same way whether it came from
    /// a `call` or a `cast`). Crash cleanup belongs in [`on_stopped`](Self::on_stopped),
    /// not in `handle` itself.
    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<Self::Response>> + Send;

    /// Phase 4a: Stop gate. Return `false` to reject graceful/parent-requested stops.
    /// Forced stops (`Failure`, `Cancelled`) bypass this hook entirely.
    fn pre_stop(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = bool> + Send {
        async { true }
    }

    /// Phase 4b: Cleanup. Release resources, notify peers.
    /// OTP equivalent: `terminate(Reason, State)`.
    fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
    }

    /// Called when a supervised child actor stops.
    ///
    /// The [`ChildEvent`] contains the child's identity, stop reason, and the
    /// [`SupervisionAction`](crate::types::SupervisionAction) taken by the runtime.
    fn on_child_stopped(
        &mut self,
        _event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
    }
}

/// Type alias tying the strongly typed envelope to a concrete actor.
pub type ActorEnvelope<A> = Envelope<<A as Actor>::Message, <A as Actor>::Response>;

// ---------------------------------------------------------------------------
// SpawnBuilder - builder chain with IntoFuture
// ---------------------------------------------------------------------------

/// How a [`SpawnBuilder`] observes the actor's initialization outcome.
///
/// Default is [`Await`](SpawnAck::Await): OTP's `start_link` is synchronous
/// with an infinity default timeout, so `.await` on the builder waits for
/// `pre_start` and `on_started` with no deadline unless one is set.
enum SpawnAck {
    /// Await the init ack with no deadline.
    Await,
    /// Skip the ack entirely: `.await` returns as soon as the task is
    /// spawned, before `pre_start` ever runs.
    Detached,
    /// Await the init ack, bounded by a deadline; on expiry the task is
    /// aborted and no lifecycle hook runs.
    Timeout(Duration),
}

/// Builder for spawning an actor with optional name, system, config, and supervision.
///
/// Created by [`ActorExt::spawn`]. Implements [`IntoFuture`] so you can `.await` it directly.
///
/// # Examples
/// ```no_run
/// # use tokio_actors::{actor::{Actor, ActorExt}, ActorResult, ActorContext};
/// # struct MyActor;
/// # impl Actor for MyActor {
/// #     type Message = ();
/// #     type Response = ();
/// #     fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> impl std::future::Future<Output = ActorResult<()>> + Send { async { Ok(()) } }
/// # }
/// # async fn run() {
/// // Anonymous
/// let h = MyActor.spawn().await.unwrap();
/// // Named
/// let h = MyActor.spawn().named("my-actor").await.unwrap();
/// // Named supervisor (can spawn supervised children via ctx.spawn_child)
/// let h = MyActor.spawn().named("my-actor").supervisor().await.unwrap();
/// # }
/// ```
pub struct SpawnBuilder<A: Actor> {
    actor: A,
    name: Option<String>,
    system: Option<Arc<ActorSystem>>,
    config: ActorConfig,
    ack: SpawnAck,
}

impl<A: Actor> SpawnBuilder<A> {
    /// Assigns a name to the actor. The name also serves as the [`ActorId`](crate::types::ActorId).
    /// Named actors are automatically registered in the actor system.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Targets a specific [`ActorSystem`] for registration.
    pub fn on_system(mut self, system: &Arc<ActorSystem>) -> Self {
        self.system = Some(Arc::clone(system));
        self
    }

    /// Overrides the default [`ActorConfig`].
    pub fn with_config(mut self, config: ActorConfig) -> Self {
        self.config = config;
        self
    }

    /// Makes this actor a SUPERVISOR with the default config (OneForOne,
    /// 3 restarts / 5s window). A supervisor can spawn supervised children via
    /// [`ActorContext::spawn_child`](crate::actor::context::ActorContext::spawn_child).
    ///
    /// The actor takes the supervisor role; its children are the supervised
    /// ones.
    pub fn supervisor(mut self) -> Self {
        self.config = self.config.supervisor();
        self
    }

    /// Enables supervision with a custom [`SupervisionConfig`].
    pub fn with_supervision(mut self, sup: SupervisionConfig) -> Self {
        self.config = self.config.with_supervision(sup);
        self
    }

    /// Skips the initialization ack entirely: `.await` returns as soon as the
    /// task is spawned, before `pre_start` ever runs. An init failure is
    /// then silent to this caller: the actor
    /// still stops, but nobody here is told why. This does not change how a
    /// supervised child reports init failure to its parent (the watcher and
    /// restart budget already cover that); it only changes what an
    /// unsupervised, top-level caller of `.spawn()` observes.
    pub fn detached(mut self) -> Self {
        self.ack = SpawnAck::Detached;
        self
    }

    /// Bounds how long `.await` waits for `pre_start`/`on_started` to finish.
    ///
    /// On expiry the task is aborted immediately: no lifecycle hook runs
    /// afterward, matching OTP's untrappable `exit(_, kill)` on a start
    /// timeout. Before the caller gets [`SpawnError::StartTimeout`], this
    /// bounded-waits for the aborted task's own teardown (including its
    /// registry guard drop) to finish, so an immediate retry under the same
    /// name never races a predecessor that is still unwinding.
    pub fn start_timeout(mut self, timeout: Duration) -> Self {
        self.ack = SpawnAck::Timeout(timeout);
        self
    }
}

impl<A: Actor> IntoFuture for SpawnBuilder<A> {
    type Output = Result<handle::ActorHandle<A>, SpawnError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let id_str = self
                .name
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

            match self.ack {
                SpawnAck::Detached => {
                    runtime::into_actor(id_str, self.actor, self.config, self.name, self.system)
                }
                SpawnAck::Await => {
                    let (tx, rx) = oneshot::channel();
                    let (handle, _join, _status_tx) = runtime::spawn_actor(
                        id_str.into(),
                        self.actor,
                        self.config,
                        self.name,
                        self.system,
                        Some(tx),
                        true,
                    )?;
                    match rx.await {
                        Ok(Ok(())) => Ok(handle),
                        Ok(Err(err)) => Err(SpawnError::Init(Box::new(err))),
                        // The ack sender was dropped without sending: not a
                        // reachable path today (the task always acks exactly
                        // once), kept as a defensive fallback.
                        Err(_) => Err(SpawnError::Init(Box::new(ActorError::user(
                            "spawn ack channel closed before a reply arrived",
                        )))),
                    }
                }
                SpawnAck::Timeout(timeout) => {
                    let (tx, rx) = oneshot::channel();
                    let (handle, join, _status_tx) = runtime::spawn_actor(
                        id_str.into(),
                        self.actor,
                        self.config,
                        self.name,
                        self.system,
                        Some(tx),
                        true,
                    )?;
                    match tokio::time::timeout(timeout, rx).await {
                        Ok(Ok(Ok(()))) => Ok(handle),
                        Ok(Ok(Err(err))) => Err(SpawnError::Init(Box::new(err))),
                        // Same defensive fallback as the no-deadline path above.
                        Ok(Err(_)) => Err(SpawnError::Init(Box::new(ActorError::user(
                            "spawn ack channel closed before a reply arrived",
                        )))),
                        Err(_elapsed) => {
                            // Unconditional, untrappable: matches OTP's
                            // exit(_, kill) on a start timeout. No lifecycle
                            // hook runs; the task is dropped mid-poll.
                            join.abort();
                            // Bounded wait for the abort to actually finish
                            // the task's teardown (including its registry
                            // guard drop) before reporting the timeout, so a
                            // caller that immediately retries under the same
                            // name never races a still-unwinding predecessor.
                            let _ = tokio::time::timeout(KILL_GRACE, join).await;
                            Err(SpawnError::StartTimeout)
                        }
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// ActorExt - single spawn() method returning SpawnBuilder
// ---------------------------------------------------------------------------

/// Convenience trait for spawning actors directly from their implementations.
pub trait ActorExt: Actor + Sized {
    /// Creates a [`SpawnBuilder`] for this actor.
    ///
    /// The builder implements [`IntoFuture`], so you can `.await` it directly
    /// for an anonymous spawn, or chain `.named()`, `.on_system()`, `.supervisor()`,
    /// `.with_config()` before awaiting.
    ///
    /// By default `.await` acks the actor's initialization: it does not
    /// return until `pre_start` and `on_started` have both run, with no
    /// deadline (OTP `start_link` infinity-default parity). An `Err` from
    /// either surfaces here as [`SpawnError::Init`]. Chain `.detached()` for
    /// the old fire-and-forget behavior, or `.start_timeout(dur)` to bound
    /// the wait.
    fn spawn(self) -> SpawnBuilder<Self> {
        SpawnBuilder {
            actor: self,
            name: None,
            system: None,
            config: ActorConfig::default(),
            ack: SpawnAck::Await,
        }
    }
}

impl<T> ActorExt for T where T: Actor {}
