//! Core actor traits and identity types.

/// Actor execution context.
pub mod context;
/// Actor handle for external communication.
pub mod handle;
/// Panic capture for actor callbacks.
pub(crate) mod panic;
/// Runtime configuration and spawning logic.
pub mod runtime;
/// Subtree tokens: the end of an actor and of everything it spawned.
pub(crate) mod subtree;
/// Supervision configuration and child management.
pub mod supervision;

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::error::{ActorResult, SpawnError};
use crate::system::ActorSystem;
use crate::types::{ChildEvent, Envelope, StopReason};
use context::ActorContext;
use runtime::ActorConfig;
use supervision::{SupervisionConfig, ABORT_SETTLE, DEFAULT_KILL_GRACE};

/// Primary trait implemented by all actors.
///
/// An actor is a stateful entity that processes messages sequentially.
/// Each actor runs in its own Tokio task.
///
/// # The end of an actor
///
/// When an actor's task runs to its end, a supervisor's child factories drop
/// with its children's teardown; then its name is freed, then everything else
/// it owned is dropped (its queued messages, its value), and only then does its
/// status read [`ActorStatus::Stopped`](crate::types::ActorStatus::Stopped).
/// Its mailbox stays open while `on_stopped` runs, so a message sent then is
/// accepted and then dropped unhandled with the queued messages; a send after
/// `on_stopped` returns is refused. Its timers and streams are released from
/// the close of its mailbox on, which comes once `on_stopped` has returned, the
/// initialization failed, or a `Kill` ended the run (a `Kill` skips
/// `on_stopped`): each one when its forwarder next runs, and what a forwarder
/// that has not run still owns is dropped by the end, before `Stopped`. The
/// name stays registered while the actor's children may still run, so an
/// observer of such a release may still find the name registered: only
/// `Stopped`, or a closed status plane, means the name is free. The value's
/// `Drop` therefore runs inside the stop bound of whoever stops the actor, and
/// a `Drop` that waits for its own actor's end, through
/// [`wait_stopped`](crate::actor::handle::ActorHandle::wait_stopped) for
/// example, waits forever. The actor's reason is final once its stop lane is
/// sealed, after its children's teardown and its timers' and streams' end and
/// before its queued messages, its value and its context are dropped: it is
/// written into its status plane then, and never changes. A panic in a drop
/// before that point (a timer or stream still registered at the end, a child
/// factory or the supervision state at the teardown, a factory whose last
/// reference a restart in progress then drops) is caught: it keeps an earlier
/// `Kill` or failure as the reason and makes any other reason the panic. An
/// attempt whose spec is removed during its build starts no incarnation; one
/// that finished its build before the removal starts it, and the teardown stops
/// it, or under `Kill` its link guard kills it. A timer or stream cancelled
/// before the end (in `on_stopped`, for example) belongs to the run, and a
/// panic in its drop never changes the reason. A panic in the drop of a queued
/// message, the value or the context comes after the reason is final: it is
/// caught, the end goes on, and `Stopped` is still published. With the
/// `tracing` feature, every caught drop panic that leaves the reason as it
/// stands is logged as a warning naming the actor, a later panic that a step of
/// the end discards beside the one it keeps included. Every log line of the
/// crate is written inside a catch, so a `tracing` subscriber that panics is
/// contained there: it never changes an actor's reason, never cuts its end
/// short, and never aborts the process from a drop. Two aborts are beyond
/// any catch, and Rust always turns them into an abort: a panic hook that
/// itself panics, and a subscriber that panics again while its own panic
/// unwinds (a value in its `event` whose `Drop` panics during that
/// unwind).
///
/// During the run, the drops the actor's loop makes of user values outside a
/// callback are caught too: a response nobody receives (a `notify`, or a
/// `send` whose caller is gone) and the factory of a child whose spec the
/// loop removes (the end of a `Temporary` child, or of a `SimpleOneForOne`
/// child that is not restarted, a group's absorption of a member's end, the
/// classification of a manual stop whose call was dropped). A panic in one of them stops the actor as a panic in `handle`
/// does: with `Failure(Panic(..))`, through `on_stopped` and the ordered stop
/// of its children; a child's end the turn took and did not deliver is
/// delivered once, with the child's name, before `on_stopped`. A factory a
/// supervision call removes (`delete_child`, a `stop_child` or
/// `terminate_child` that removes the spec) drops as that call's last step,
/// so a panic there is a panic of the callback that made the call.
///
/// When an actor's task is aborted, its status never reads `Stopped`: its
/// status channel closes, which reads as ended, only once its name is free and
/// everything it owned is dropped, the message a callback in progress was
/// handling included. Its reason, `Kill`, is written once its name is free and
/// before its queued messages drop, so a request queued there, or an awaited
/// spawn whose initialization the abort cut, is answered without waiting for
/// those drops. A task aborted before its first poll is the exception: its
/// context writes `Kill` after its value drops and while its name is still
/// registered, so a request queued there can be answered before the name is
/// free, and its awaited spawn is answered last. A panic in one of those drops
/// is caught (and, with the `tracing` feature, logged naming the actor), and
/// the actor ends as killed. An abort drops the timers' and streams' tasks with
/// the actor.
///
/// A panic of a user call in a timer's or stream's task (the timer's
/// factory, the stream's poll, a conversion into the actor's message) or of
/// the runtime itself happens in a poll: it ends that task, is recorded as
/// the actor's
/// [`last_forwarder_error`](crate::actor::context::ActorContext::last_forwarder_error),
/// and never changes the actor's reason. Within the drops of an abort at the
/// actor's end, the first drop panic is that task's panic, classified by
/// registration as above, and a recurring timer's factory or a stream whose
/// drop panics beside it is logged, naming the actor; an abort of the
/// actor's task leaves the first unlogged. A stream that ended during the
/// run has its own drop panic logged when its forwarder does not report it:
/// when a later panic, of the conversion into its `Finished` message or of
/// that message's drop, is the one reported, or when an abort at the
/// actor's end drops the forwarder while its `Finished` waits for room and
/// nothing later panics. The drops of the timers' and streams' tasks at an abort must
/// finish within the short wait that confirms an abort: one that takes longer leaves the actor
/// reading as one that does not yield, so a restart waiting for it ends with
/// `ChildUnresponsive` and `kill_by_id` reports `Unresponsive`. A timer's or
/// stream's `Drop` should therefore not block. A task whose own poll unwinds
/// outside every catch ends in the same order as an aborted one, with the
/// reason `Failure(Panic(..))` instead of `Kill`: its status plane, its
/// supervisor's report of its end and an awaited spawn whose initialization the
/// unwind cut (as [`SpawnError::Init`]) read that one reason.
///
/// # Compile-time contracts
///
/// Every actor, and every message and response type it declares, must be
/// `Send`: an actor can be scheduled onto any worker thread the runtime
/// chooses, so nothing it owns can be tied to a single thread.
///
/// ```compile_fail
/// // error[E0277]: `Rc<()>` cannot be sent between threads safely
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// struct RcActor(std::rc::Rc<()>);
///
/// impl Actor for RcActor {
///     type Message = ();
///     type Response = ();
///
///     async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
/// ```
///
/// ```rust
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// struct ArcActor(std::sync::Arc<()>);
///
/// impl Actor for ArcActor {
///     type Message = ();
///     type Response = ();
///
///     async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
/// ```
///
/// An actor, and the message and response types it declares, must also be
/// `'static`: nothing borrowed from outside can be named in `Self`,
/// `Message`, or `Response`.
///
/// ```compile_fail
/// // error[E0477]: the type `&'a str` does not fulfill the required lifetime
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// struct Holder<'a>(std::marker::PhantomData<&'a ()>);
///
/// impl<'a> Actor for Holder<'a> {
///     type Message = &'a str;
///     type Response = ();
///
///     async fn handle(&mut self, _msg: Self::Message, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
/// ```
///
/// ```rust
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// struct StaticHolder;
///
/// impl Actor for StaticHolder {
///     type Message = &'static str;
///     type Response = ();
///
///     async fn handle(&mut self, _msg: Self::Message, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
/// ```
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
    /// Forced stops (`Failure`, `ParentKilled`, `Cancelled`) bypass this hook entirely.
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
    ///
    /// Returning `Err` stops this supervisor with that failure, the same
    /// contract [`handle`](Self::handle) has (OTP: a callback that fails
    /// terminates the process).
    ///
    /// A supervisor that stops for any reason other than a kill first
    /// receives, once each, the child deaths still pending when its message
    /// loop ended, with
    /// [`SupervisionAction::SupervisorStopping`](crate::types::SupervisionAction::SupervisorStopping)
    /// and `ctx.status()` reading `Stopping`, before `on_stopped`. No restart
    /// follows, a kill that arrives meanwhile ends the remaining deliveries,
    /// supervision calls made here return
    /// [`SupervisionError::SupervisorStopping`](crate::error::SupervisionError::SupervisorStopping)
    /// (a spawn returns
    /// [`SpawnError::SupervisorStopping`]),
    /// and an `Err` returned here, or a panic, makes the supervisor's exit
    /// reason `Failure` once `on_stopped` has returned, as an `Err` from
    /// `on_stopped` does, and is recorded as its last error, while the
    /// remaining deliveries still run. A child whose end comes after the
    /// supervisor has collected the deaths still pending when its message
    /// loop ended, one stopped by the supervisor's own teardown included, is
    /// not reported here; with the `tracing` feature, such an end with an
    /// unexpected reason is logged as a warning. A supervisor whose
    /// `pre_start` or `on_started` fails receives none: it never ran its
    /// message loop, as it never receives `on_stopped`; with the `tracing`
    /// feature, a child that ended with an unexpected reason during that
    /// initialization is logged as a warning.
    ///
    /// A `SupervisorStopping` error from a supervision call made here is an
    /// `Err` like any other: returned with `?`, it makes the supervisor's
    /// exit reason `Failure`, so a `Transient` supervisor stopped through its
    /// handle, by its parent's `stop_child` or by its exhausted restart
    /// budget is then restarted by its parent (one its parent removes with
    /// `terminate_child` or stops in its own shutdown is not restarted
    /// whatever it returns). Check `event.action` for
    /// `SupervisionAction::SupervisorStopping` before calling.
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
    kill_grace: Duration,
}

impl<A: Actor> std::fmt::Debug for SpawnBuilder<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnBuilder")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
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
    /// supervised child reports init failure to its parent (the runner and
    /// restart budget already cover that); it only changes what an
    /// unsupervised, top-level caller of `.spawn()` observes.
    pub fn detached(mut self) -> Self {
        self.ack = SpawnAck::Detached;
        self
    }

    /// Bounds how long `.await` waits for `pre_start`/`on_started` to finish. A
    /// timeout beyond 30 years is treated as 30 years.
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

    /// Sets how long this actor is given, after a `Kill` is raised on it, to
    /// reach its next turn boundary before its task is aborted. Default:
    /// 120 ms.
    ///
    /// `Duration::ZERO` aborts the task as soon as the `Kill` is raised, so a
    /// callback in progress is cut at its next `.await` and never observes
    /// the `Kill` itself. The grace applies to every kill of this actor:
    /// [`ActorSystem::kill`](crate::system::ActorSystem::kill),
    /// [`ActorSystem::kill_by_id`](crate::system::ActorSystem::kill_by_id),
    /// [`ActorHandle::stop`](crate::actor::handle::ActorHandle::stop) with
    /// [`StopReason::Kill`], the kill step of
    /// [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within)
    /// and of system shutdown, its supervisor's stop of it, and the kill a
    /// worker receives when its own supervisor is killed. An actor that is
    /// itself a supervisor stops in order when its supervisor is killed,
    /// bounded by its own [`Shutdown`](crate::types::Shutdown), and the
    /// grace applies to the `Kill` raised once that window has passed;
    /// [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within)
    /// called with `StopReason::Kill` waits its own `within` instead, for
    /// that call. A grace beyond 30 years is capped there, so in practice it
    /// never aborts. A callback that never yields survives the abort as well
    /// and is reported unresponsive.
    pub fn kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = grace;
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
                SpawnAck::Detached => runtime::into_actor(
                    id_str,
                    self.actor,
                    self.config,
                    self.name,
                    self.system,
                    self.kill_grace,
                ),
                SpawnAck::Await => {
                    let (tx, rx) = oneshot::channel();
                    let (handle, _join, _subtree) = runtime::spawn_actor(
                        id_str.into(),
                        self.actor,
                        self.config,
                        self.name,
                        self.system,
                        Some(tx),
                        true,
                        self.kill_grace,
                        None,
                    )?;
                    match rx.await {
                        Ok(Ok(())) => Ok(handle),
                        Ok(Err(err)) => Err(SpawnError::Init(Box::new(err))),
                        // The ack sender dropped without a reply: the actor
                        // stopped before it finished initializing.
                        Err(_) => Err(closed_ack_error(&handle).await),
                    }
                }
                SpawnAck::Timeout(timeout) => {
                    let (tx, rx) = oneshot::channel();
                    let (handle, join, _subtree) = runtime::spawn_actor(
                        id_str.into(),
                        self.actor,
                        self.config,
                        self.name,
                        self.system,
                        Some(tx),
                        true,
                        self.kill_grace,
                        None,
                    )?;
                    match tokio::time::timeout(runtime::bounded_wait(timeout), rx).await {
                        Ok(Ok(Ok(()))) => Ok(handle),
                        Ok(Ok(Err(err))) => Err(SpawnError::Init(Box::new(err))),
                        // Same closed-ack rule as the no-deadline path above.
                        Ok(Err(_)) => Err(closed_ack_error(&handle).await),
                        Err(_elapsed) => {
                            // Unconditional, untrappable: matches OTP's
                            // exit(_, kill) on a start timeout. No lifecycle
                            // hook runs; the task is dropped mid-poll.
                            handle
                                .stop_lane()
                                .abort_record()
                                .abort(&join.abort_handle());
                            // Bounded wait for the abort to actually finish
                            // the task's teardown (including its registry
                            // guard drop) before reporting the timeout, so a
                            // caller that immediately retries under the same
                            // name never races a still-unwinding predecessor.
                            let _ = tokio::time::timeout(ABORT_SETTLE, join).await;
                            Err(SpawnError::StartTimeout)
                        }
                    }
                }
            }
        })
    }
}

/// The error for a spawn whose ack sender dropped without a reply, read from
/// the reason the actor's status plane carries, so the spawn and the plane
/// give one reason. A `Kill` cut the initialization, or reached the end of a
/// failed initialization before the reason was final, and the actor answers
/// it right after sealing its stop lane, before its queued messages and its
/// value drop; or the task was aborted, and the plane reads `Kill`: the spawn
/// is [`SpawnError::Killed`]. The task's own poll unwound outside every catch
/// during the initialization, and the plane reads that panic as a failure:
/// the spawn is [`SpawnError::Init`] with it. The wait ends as soon as the
/// actor's reason is final, so it never waits through those drops.
async fn closed_ack_error<A: Actor>(handle: &handle::ActorHandle<A>) -> SpawnError {
    let mut status_rx = handle.status_rx();
    match crate::system::wait_final_reason(&mut status_rx).await {
        StopReason::Failure(err) => SpawnError::Init(Box::new(err)),
        _ => SpawnError::Killed,
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
    /// By default `.await` acks the actor's initialization: it does not return
    /// until `pre_start` and `on_started` have both run, with no deadline (OTP
    /// `start_link` infinity-default parity). An `Err` from either surfaces
    /// here as [`SpawnError::Init`], unless a `Kill` reaches the actor's end
    /// before its reason is final: the spawn then reads [`SpawnError::Killed`],
    /// as the actor's status plane reads `Kill`. Chain `.detached()` for a
    /// fire-and-forget spawn that returns before initialization runs, or
    /// `.start_timeout(dur)` to bound the wait.
    fn spawn(self) -> SpawnBuilder<Self> {
        SpawnBuilder {
            actor: self,
            name: None,
            system: None,
            config: ActorConfig::default(),
            ack: SpawnAck::Await,
            kill_grace: DEFAULT_KILL_GRACE,
        }
    }
}

impl<T> ActorExt for T where T: Actor {}
