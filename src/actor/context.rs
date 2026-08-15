//! Actor execution context providing timers, streams, and supervision hooks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use futures_core::Stream;
use tokio_stream::StreamExt;

use tokio::task::AbortHandle;

use crate::actor::handle::ActorHandle;
use crate::actor::panic::{catch_sync, payload_into_string};
use crate::actor::supervision::{ChildSpec, ChildState, ManualStop, SupervisionState, KILL_GRACE};
use crate::actor::{runtime, Actor};
use crate::error::{ActorError, SpawnError, StreamError, SupervisionError, TimerError};
use crate::system::ActorSystem;
use crate::types::{
    ActorId, ActorStatus, ChildInfo, ChildStoppedInternal, MissPolicy, RecurringId,
    RecurringIdGenerator, RestartType, Shutdown, StopReason, StreamEvent, StreamId, SystemMessage,
};

/// Actor execution context providing timers, streams, supervision, and runtime hooks.
pub struct ActorContext<A: Actor> {
    actor_id: ActorId,
    self_handle: ActorHandle<A>,
    runtime: Handle,
    timers: HashMap<RecurringId, TimerRegistration>,
    streams: HashMap<StreamId, StreamRegistration>,
    id_gen: RecurringIdGenerator,
    last_error: Option<ActorError>,
    /// Runtime status plane (`process_info`-style, see the `handle` module
    /// docs). Every write goes through `send_replace`, which always succeeds
    /// even if every `ActorHandle` (and therefore every receiver) has been
    /// dropped - a plain `send` would error at zero receivers.
    status_tx: watch::Sender<ActorStatus>,
    // Supervision fields
    system_tx: mpsc::Sender<SystemMessage>,
    system: Option<Arc<ActorSystem>>,
    name: Option<String>,
    supervision: Option<SupervisionState>,
}

impl<A: Actor> ActorContext<A> {
    // Internal constructor, called from exactly one spawn site with every
    // field already resolved; a params-object would only move the count
    // around, not reduce it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        actor_id: ActorId,
        handle: ActorHandle<A>,
        runtime: Handle,
        status_tx: watch::Sender<ActorStatus>,
        system_tx: mpsc::Sender<SystemMessage>,
        system: Option<Arc<ActorSystem>>,
        name: Option<String>,
        supervision: Option<SupervisionState>,
    ) -> Self {
        Self {
            actor_id,
            self_handle: handle,
            runtime,
            timers: HashMap::new(),
            streams: HashMap::new(),
            id_gen: RecurringIdGenerator::default(),
            last_error: None,
            status_tx,
            system_tx,
            system,
            name,
            supervision,
        }
    }

    // - Identity & status --------------------------------------------------

    /// Returns the unique identifier of this actor.
    pub fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    /// Returns the registered name of this actor, if any.
    pub fn actor_name(&self) -> Option<&String> {
        self.name.as_ref()
    }

    /// Returns a handle to this actor.
    ///
    /// This handle, and any internal self-reference derived from it (a
    /// recurring [`schedule`](Self::schedule) or an attached
    /// [`add_stream`](Self::add_stream) forwarder both hold a clone
    /// internally), never extends the actor's lifetime: process lifetime
    /// governs timer lifetime, not the reverse. An actor still runs until it
    /// is stopped, killed, or its system shuts down, whether or not any
    /// external handle remains; when it stops, every internal self-reference
    /// is torn down with it. Parity: Erlang/OTP pid-addressed timers are
    /// automatically canceled when their target process dies (erlang.org,
    /// OTP 28, `erlang:start_timer/4`).
    pub fn self_handle(&self) -> ActorHandle<A> {
        self.self_handle.clone()
    }

    /// Records a failure that occurred during message processing.
    pub fn record_failure(&mut self, error: ActorError) {
        self.last_error = Some(error);
    }

    /// Returns the last error recorded by this actor, if any.
    pub fn last_error(&self) -> Option<&ActorError> {
        self.last_error.as_ref()
    }

    /// Returns the current lifecycle status of the actor.
    pub fn status(&self) -> ActorStatus {
        *self.status_tx.borrow()
    }

    pub(crate) fn set_status(&mut self, status: ActorStatus) {
        // `send_replace`, not `send`: this must succeed even if every
        // `ActorHandle` (and therefore every watch receiver) has already been
        // dropped, which a plain `send` would treat as an error.
        self.status_tx.send_replace(status);
    }

    // - Supervision --------------------------------------------------------

    pub(crate) fn supervision_mut(&mut self) -> Option<&mut SupervisionState> {
        self.supervision.as_mut()
    }

    pub(crate) fn supervision_ref(&self) -> Option<&SupervisionState> {
        self.supervision.as_ref()
    }

    /// Creates a [`ChildSpawnBuilder`] for spawning a supervised child actor.
    ///
    /// The factory is called to create the initial instance, and stored for
    /// future restarts. Chain `.named()`, `.restart_type()`, `.shutdown()`,
    /// `.with_config()` before `.await`ing to customize.
    ///
    /// Unlike the top-level [`ActorExt::spawn`](crate::actor::ActorExt::spawn),
    /// this never awaits the child's `pre_start`/`on_started` ack: the
    /// watcher and restart budget already give the supervisor full,
    /// asynchronous visibility into a child's init failure, and synchronously
    /// awaiting that ack from inside the parent's own callback would invite
    /// the same self-call deadlock documented on
    /// [`ActorHandle::send`](crate::actor::handle::ActorHandle::send).
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult, RestartType, SupervisionConfig};
    ///
    /// #[derive(Default)]
    /// struct Supervisor;
    /// #[derive(Default)]
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    /// }
    ///
    /// impl Actor for Supervisor {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         // Defaults: anonymous, Permanent, Shutdown::Timeout(5s)
    ///         ctx.spawn_child(Worker::default).await?;
    ///
    ///         // Named + transient
    ///         ctx.spawn_child(Worker::default)
    ///             .named("worker")
    ///             .restart_type(RestartType::Transient)
    ///             .await?;
    ///         Ok(())
    ///     }
    /// }
    /// ```
    ///
    /// # Errors
    /// - [`SpawnError::NotASupervisor`]
    ///   if this actor has no supervision config.
    /// - [`SpawnError::DuplicateChild`]
    ///   if a live child or a kept spec (a child previously stopped with
    ///   [`terminate_child`](Self::terminate_child)) already occupies this
    ///   name - the factory is never called (OTP `already_present` parity).
    ///   [`delete_child`](Self::delete_child) the old spec first to reuse the
    ///   name.
    /// - other [`SpawnError`] variants if the child
    ///   fails to spawn.
    pub fn spawn_child<F, C>(&mut self, factory: F) -> ChildSpawnBuilder<'_, A, F, C>
    where
        F: Fn() -> C + Send + Sync + 'static,
        C: Actor,
    {
        ChildSpawnBuilder {
            ctx: self,
            factory,
            name: None,
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::default(),
            config: None,
            start_timeout: None,
            _child: std::marker::PhantomData,
        }
    }

    /// Internal spawn logic used by [`ChildSpawnBuilder`].
    fn spawn_child_internal<F, C>(
        &mut self,
        factory: F,
        name: Option<String>,
        restart_type: RestartType,
        shutdown: Shutdown,
        config: Option<runtime::ActorConfig>,
        start_timeout: Option<Duration>,
    ) -> Result<ActorHandle<C>, SpawnError>
    where
        F: Fn() -> C + Send + Sync + 'static,
        C: Actor,
    {
        if self.supervision.is_none() {
            return Err(SpawnError::NotASupervisor);
        }

        let child_id: ActorId = name
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
            .into();

        // OTP `already_present` parity: a live child or a kept spec (retained
        // by `terminate_child`) under this id rejects the spawn before the
        // factory ever runs. `delete_child` clears the spec so the id can be
        // reused.
        let duplicate = self
            .supervision
            .as_ref()
            .expect("supervision presence checked above")
            .registry
            .get(&child_id)
            .is_some();
        if duplicate {
            return Err(SpawnError::DuplicateChild(child_id));
        }

        let actor = factory();
        let child_config = config.unwrap_or_default();

        let (child_handle, join_handle) = runtime::spawn_actor(
            child_id.clone(),
            actor,
            child_config.clone(),
            name.clone(),
            self.system.clone(),
            None,
            false,
        )?;

        // The parent spawns and owns the watcher (initial incarnation 0).
        // The watcher, not the child itself, delivers the death event, so a
        // panicking child is just as visible as a polite one. The abort handle
        // is taken first - the Kill escalation backstop.
        let abort = join_handle.abort_handle();
        let watcher =
            runtime::spawn_watcher(child_id.clone(), 0, join_handle, self.system_tx.clone());

        let parent_system_tx = self.system_tx.clone();
        let system_clone = self.system.clone();

        let child_state = ChildState {
            id: child_id.clone(),
            name: name.clone(),
            spec: ChildSpec {
                restart_type,
                shutdown,
                start_timeout,
            },
            watcher_handle: watcher,
            abort,
            system_tx: child_handle.system_tx(),
            is_alive: true,
            pending_restart_seq: None,
            current_incarnation: 0,
            manual_stop: None,
        };

        let sup = self
            .supervision
            .as_mut()
            .expect("supervision presence checked above");
        sup.registry.register(child_state);

        // Restart closure: captures the child's spec BY VALUE (original id,
        // name, resolved config) so every restart reuses it exactly - the
        // Rust equivalent of OTP child-spec immutability. The child keeps its
        // ActorId across restarts, named or anonymous.
        let factory = Arc::new(factory);
        let restart_id = child_id.clone();
        let restart_name = name;
        let restart_config = child_config;
        // Captured alongside the rest of the restart spec so the restart
        // closure (whose only input is the incarnation sequence number) can
        // bound its wait for this child's init without reading the registry.
        let restart_start_timeout = start_timeout;

        sup.restart_fns.insert(
            child_id,
            Box::new(move |seq| {
                let factory = Arc::clone(&factory);
                let parent_tx = parent_system_tx.clone();
                let system = system_clone.clone();
                let child_id = restart_id.clone();
                let name = restart_name.clone();
                let config = restart_config.clone();
                let start_timeout = restart_start_timeout;
                Box::pin(async move {
                    // A panicking factory must not kill the restart task
                    // silently: it surfaces as FactoryFailed and charges the
                    // budget when the event re-enters strategy evaluation.
                    let actor = match catch_sync(&*factory) {
                        Ok(actor) => actor,
                        Err(payload) => {
                            let msg = payload_into_string(payload);
                            let reason =
                                StopReason::Failure(SupervisionError::FactoryFailed(msg).into());
                            let _ = parent_tx
                                .send(SystemMessage::ChildStopped(ChildStoppedInternal {
                                    child_id,
                                    reason,
                                    incarnation: seq,
                                }))
                                .await;
                            return;
                        }
                    };

                    let (ack_tx, ack_rx) = oneshot::channel();
                    match runtime::spawn_actor(
                        child_id.clone(),
                        actor,
                        config,
                        name,
                        system,
                        Some(ack_tx),
                        false,
                    ) {
                        Ok((handle, join)) => {
                            // The abort handle is taken before the ack is
                            // awaited (the Kill escalation backstop for a hung
                            // init); the JoinHandle itself is only handed to
                            // the parent once the ack confirms Running.
                            let abort = join.abort_handle();
                            let new_system_tx = handle.system_tx();

                            // Await the same init contract the top-level spawn
                            // awaits (pre_start + on_started), bounded by the
                            // child's start_timeout if one was set; None waits
                            // indefinitely (OTP start_link parity).
                            let acked = match start_timeout {
                                Some(dur) => time::timeout(dur, ack_rx).await,
                                None => Ok(ack_rx.await),
                            };

                            match acked {
                                Ok(Ok(Ok(()))) => {
                                    // The parent adopts the incarnation and
                                    // spawns the watcher itself on acceptance,
                                    // so watcher events can never outrun this
                                    // message on the channel.
                                    let _ = parent_tx
                                        .send(SystemMessage::RestartComplete {
                                            seq,
                                            child_id,
                                            new_system_tx,
                                            new_join: join,
                                        })
                                        .await;
                                }
                                Ok(Ok(Err(err))) => {
                                    let reason = StopReason::Failure(err);
                                    let _ = parent_tx
                                        .send(SystemMessage::ChildStopped(ChildStoppedInternal {
                                            child_id,
                                            reason,
                                            incarnation: seq,
                                        }))
                                        .await;
                                }
                                Ok(Err(_recv_err)) => {
                                    // The ack channel closed without a value:
                                    // the fresh incarnation's task ended
                                    // before it could ack (a panic outside the
                                    // caught callback boundary, or similar).
                                    let reason = StopReason::Failure(ActorError::user(
                                        "restart ack channel closed before a reply arrived",
                                    ));
                                    let _ = parent_tx
                                        .send(SystemMessage::ChildStopped(ChildStoppedInternal {
                                            child_id,
                                            reason,
                                            incarnation: seq,
                                        }))
                                        .await;
                                }
                                Err(_elapsed) => {
                                    // start_timeout expired: kill the hung
                                    // incarnation through the same post-Kill
                                    // ladder manual stops use, then report the
                                    // failure through the ordinary path.
                                    let _ = new_system_tx
                                        .send(SystemMessage::Stop(StopReason::Kill))
                                        .await;
                                    let _ = ActorContext::<A>::await_child_exit(
                                        &child_id,
                                        &new_system_tx,
                                        abort,
                                    )
                                    .await;
                                    let reason = StopReason::Failure(ActorError::Spawn(
                                        SpawnError::StartTimeout,
                                    ));
                                    let _ = parent_tx
                                        .send(SystemMessage::ChildStopped(ChildStoppedInternal {
                                            child_id,
                                            reason,
                                            incarnation: seq,
                                        }))
                                        .await;
                                }
                            }
                        }
                        Err(err) => {
                            let reason = StopReason::Failure(ActorError::Spawn(err));
                            let _ = parent_tx
                                .send(SystemMessage::ChildStopped(ChildStoppedInternal {
                                    child_id,
                                    reason,
                                    incarnation: seq,
                                }))
                                .await;
                        }
                    }
                })
            }),
        );

        Ok(child_handle)
    }

    /// Returns introspection info for all supervised children.
    pub fn children(&self) -> Vec<ChildInfo> {
        self.supervision
            .as_ref()
            .map_or(Vec::new(), |s| s.registry.children_info())
    }

    /// Manually stops a supervised child and lets its policy restart it
    /// (a "bounce"): a Permanent child is restarted BUDGET-FREE ("a manual
    /// stop is not a failure"), Transient and Temporary children stay down,
    /// and a SimpleOneForOne child's dynamic spec is removed entirely.
    ///
    /// Honors the child's [`Shutdown`] policy: a vetoing or slow child is
    /// escalated to Kill at the timeout, with a task-abort backstop behind
    /// it. Returns once the child has stopped (idempotent `Ok` if it was
    /// already stopped).
    ///
    /// To stop a child WITHOUT any restart, use
    /// [`terminate_child`](Self::terminate_child).
    ///
    /// # Errors
    /// - [`SupervisionError::NotASupervisor`] / [`SupervisionError::ChildNotFound`]
    /// - [`SupervisionError::ChildRestarting`] while the child has a restart
    ///   in flight or belongs to a pending group restart.
    /// - [`SupervisionError::ChildUnresponsive`] if the child survives even
    ///   the abort backstop (a non-yielding handler; see the crate docs).
    pub async fn stop_child(&mut self, child: impl Into<ActorId>) -> Result<(), SupervisionError> {
        self.manual_stop_child(child.into(), ManualStop::Bounce)
            .await
    }

    /// Stops a supervised child WITHOUT restarting it - the OTP
    /// `terminate_child/2` equivalent. The child spec is kept (visible in
    /// [`children`](Self::children) with `is_alive == false`) so the child can
    /// later be revived with [`restart_child`](Self::restart_child) or removed
    /// with [`delete_child`](Self::delete_child); a Temporary child's spec is
    /// deleted immediately (OTP parity), as is a SimpleOneForOne child's.
    ///
    /// Blocking, escalation, and error semantics match
    /// [`stop_child`](Self::stop_child).
    pub async fn terminate_child(
        &mut self,
        child: impl Into<ActorId>,
    ) -> Result<(), SupervisionError> {
        self.manual_stop_child(child.into(), ManualStop::Terminate)
            .await
    }

    /// Revives a child previously stopped with
    /// [`terminate_child`](Self::terminate_child) - the OTP `restart_child/2`
    /// equivalent. Initiates the restart from the stored child spec and
    /// returns; completion is observable via a name lookup or
    /// [`children`](Self::children).
    ///
    /// # Errors
    /// - [`SupervisionError::NotASupervisor`] / [`SupervisionError::ChildNotFound`]
    /// - [`SupervisionError::ChildRunning`] if the child is alive.
    /// - [`SupervisionError::ChildRestarting`] if a restart is already in
    ///   flight or the child belongs to a pending group restart.
    pub fn restart_child(&mut self, child: impl Into<ActorId>) -> Result<(), SupervisionError> {
        let id = child.into();
        let sup = self
            .supervision
            .as_mut()
            .ok_or(SupervisionError::NotASupervisor)?;
        if sup.in_pending_group(&id) {
            return Err(SupervisionError::ChildRestarting(id));
        }
        match sup.registry.get(&id) {
            None => return Err(SupervisionError::ChildNotFound(id)),
            Some(c) if c.is_alive => return Err(SupervisionError::ChildRunning(id)),
            Some(c) if c.pending_restart_seq.is_some() => {
                return Err(SupervisionError::ChildRestarting(id))
            }
            Some(_) => {}
        }
        sup.initiate(&id);
        Ok(())
    }

    /// Removes a stopped child's spec - the OTP `delete_child/2` equivalent.
    /// The child must not be running or restarting; use
    /// [`terminate_child`](Self::terminate_child) first.
    ///
    /// # Errors
    /// Same set as [`restart_child`](Self::restart_child).
    pub fn delete_child(&mut self, child: impl Into<ActorId>) -> Result<(), SupervisionError> {
        let id = child.into();
        let sup = self
            .supervision
            .as_mut()
            .ok_or(SupervisionError::NotASupervisor)?;
        if sup.in_pending_group(&id) {
            return Err(SupervisionError::ChildRestarting(id));
        }
        match sup.registry.get(&id) {
            None => return Err(SupervisionError::ChildNotFound(id)),
            Some(c) if c.is_alive => return Err(SupervisionError::ChildRunning(id)),
            Some(c) if c.pending_restart_seq.is_some() => {
                return Err(SupervisionError::ChildRestarting(id))
            }
            Some(_) => {}
        }
        sup.registry.remove(&id);
        sup.restart_fns.remove(&id);
        Ok(())
    }

    /// Shared machinery for stop_child / terminate_child: marks the manual
    /// stop (so the death event bypasses strategy evaluation), signals per the
    /// child's Shutdown policy, and waits for the child's system channel to
    /// close (the deadlock-free near-termination signal; awaiting the watcher
    /// from inside the supervisor's own callback could park forever).
    async fn manual_stop_child(
        &mut self,
        id: ActorId,
        kind: ManualStop,
    ) -> Result<(), SupervisionError> {
        let sup = self
            .supervision
            .as_ref()
            .ok_or(SupervisionError::NotASupervisor)?;
        if sup.in_pending_group(&id) {
            return Err(SupervisionError::ChildRestarting(id));
        }
        let (child_tx, shutdown, abort, alive, pending) = match sup.registry.get(&id) {
            Some(child) => (
                child.system_tx.clone(),
                child.spec.shutdown,
                child.abort.clone(),
                child.is_alive,
                child.pending_restart_seq.is_some(),
            ),
            None => return Err(SupervisionError::ChildNotFound(id)),
        };
        if pending {
            return Err(SupervisionError::ChildRestarting(id));
        }
        if !alive {
            // OTP terminate_child on an already-stopped child returns ok.
            return Ok(());
        }

        // Mark BEFORE signalling so the death event takes the manual path.
        if let Some(sup) = self.supervision.as_mut() {
            if let Some(child) = sup.registry.get_mut(&id) {
                child.manual_stop = Some(kind);
            }
        }

        match shutdown {
            Shutdown::Kill => {
                let _ = child_tx.send(SystemMessage::Stop(StopReason::Kill)).await;
                Self::await_child_exit(&id, &child_tx, abort).await
            }
            Shutdown::Timeout(after) => {
                let _ = child_tx
                    .send(SystemMessage::Stop(StopReason::ParentRequest))
                    .await;
                if time::timeout(after, child_tx.closed()).await.is_ok() {
                    return Ok(());
                }
                // Escalate: OTP exit(Child, kill) after the shutdown timeout.
                let _ = child_tx.send(SystemMessage::Stop(StopReason::Kill)).await;
                Self::await_child_exit(&id, &child_tx, abort).await
            }
            Shutdown::Infinity => {
                let _ = child_tx
                    .send(SystemMessage::Stop(StopReason::ParentRequest))
                    .await;
                child_tx.closed().await;
                Ok(())
            }
        }
    }

    /// Post-Kill wait: grace for the cooperative Kill, then the abort()
    /// backstop, then one final bounded wait. Only a non-yielding handler can
    /// survive all three (tokio aborts at yield points only) - surfaced as a
    /// typed error instead of hanging the supervisor.
    async fn await_child_exit(
        id: &ActorId,
        child_tx: &mpsc::Sender<SystemMessage>,
        abort: AbortHandle,
    ) -> Result<(), SupervisionError> {
        if time::timeout(KILL_GRACE, child_tx.closed()).await.is_ok() {
            return Ok(());
        }
        abort.abort();
        if time::timeout(KILL_GRACE, child_tx.closed()).await.is_ok() {
            Ok(())
        } else {
            Err(SupervisionError::ChildUnresponsive(id.clone()))
        }
    }

    // - Timers -------------------------------------------------------------

    /// Creates a [`ScheduleBuilder`] for scheduling a message.
    ///
    /// Chain `.at(instant)` or `.after(delay)` for one-shot, or `.every(interval)`
    /// for recurring timers, then `.await` to register. Registration happens
    /// entirely inside that final `.await`: a builder that is constructed and
    /// dropped without being awaited registers nothing and never fires. Every
    /// builder returned from this chain is `#[must_use]`, so that mistake is a
    /// compiler warning rather than a silent no-op.
    ///
    /// Use [`every_with`](Self::every_with) instead of `.every()` when the
    /// message type does not implement `Clone`.
    ///
    /// A registered schedule holds an internal self-reference (see
    /// [`self_handle`](Self::self_handle)) that never extends the actor's
    /// lifetime: the timer is torn down when the actor stops, not the other
    /// way around.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio::time::Duration;
    /// use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult, MissPolicy};
    ///
    /// #[derive(Default)]
    /// struct MyActor;
    ///
    /// #[derive(Clone)]
    /// enum Msg { Ping, Tick }
    ///
    /// impl Actor for MyActor {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, _: Msg, _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         // One-shot after 5 seconds
    ///         ctx.schedule(Msg::Ping).after(Duration::from_secs(5)).await?;
    ///
    ///         // Recurring every 100ms (default MissPolicy::Skip)
    ///         ctx.schedule(Msg::Tick).every(Duration::from_millis(100)).await?;
    ///
    ///         // Recurring with explicit MissPolicy
    ///         ctx.schedule(Msg::Tick).every(Duration::from_millis(100))
    ///             .on_miss(MissPolicy::CatchUp).await?;
    ///         Ok(())
    ///     }
    /// }
    /// ```
    pub fn schedule(&mut self, message: A::Message) -> ScheduleBuilder<'_, A> {
        ScheduleBuilder { ctx: self, message }
    }

    /// Creates a [`RecurringScheduleBuilder`] whose message is produced by
    /// `factory` on every tick, instead of being cloned from a single
    /// template. Use this when `Message` does not implement `Clone`; when it
    /// does, `.schedule(msg).every(interval)` is the shorter spelling.
    ///
    /// Chain `.on_miss(policy)` before `.await` to override the default
    /// [`MissPolicy::Skip`]. As with every schedule builder, nothing is
    /// registered until the builder is awaited.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio::time::Duration;
    /// use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
    ///
    /// struct Payload(Vec<u8>); // not Clone
    /// enum Msg { Tick(Payload) }
    ///
    /// #[derive(Default)]
    /// struct MyActor;
    ///
    /// impl Actor for MyActor {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, _: Msg, _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.every_with(Duration::from_millis(100), || Msg::Tick(Payload(Vec::new())))
    ///             .await?;
    ///         Ok(())
    ///     }
    /// }
    /// ```
    pub fn every_with<F>(
        &mut self,
        interval: Duration,
        factory: F,
    ) -> RecurringScheduleBuilder<'_, A>
    where
        F: FnMut() -> A::Message + Send + 'static,
    {
        RecurringScheduleBuilder {
            ctx: self,
            factory: Box::new(factory),
            interval,
            miss_policy: MissPolicy::Skip,
        }
    }

    /// Internal: register a one-shot timer.
    fn register_oneshot(
        &mut self,
        message: A::Message,
        when: Instant,
    ) -> Result<RecurringId, TimerError> {
        let id = self.id_gen.next();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.self_handle.clone();
        let fut = async move {
            tokio::select! {
                _ = cancel_clone.cancelled() => {}
                _ = time::sleep_until(when) => {
                    let _ = handle.notify(message).await;
                }
            }
        };
        self.runtime.spawn(fut);
        self.timers.insert(id, TimerRegistration { token });
        Ok(id)
    }

    /// Cancels a specific timer by its ID.
    pub fn cancel_timer(&mut self, id: RecurringId) -> Result<(), TimerError> {
        match self.timers.remove(&id) {
            Some(entry) => {
                entry.token.cancel();
                Ok(())
            }
            None => Err(TimerError::NotFound),
        }
    }

    /// Cancels all active timers.
    pub fn cancel_all_timers(&mut self) {
        for entry in self.timers.values() {
            entry.token.cancel();
        }
        self.timers.clear();
    }

    /// Returns the number of active timers.
    pub fn active_timer_count(&self) -> usize {
        self.timers.len()
    }

    // - Streams ------------------------------------------------------------

    /// Attaches an external stream to this actor's mailbox.
    ///
    /// Like a recurring [`schedule`](Self::schedule), the forwarder task
    /// holds an internal self-reference (see [`self_handle`](Self::self_handle))
    /// that never extends the actor's lifetime: it is torn down when the
    /// actor stops.
    pub fn add_stream<S>(&mut self, stream: S) -> StreamId
    where
        S: Stream + Send + Unpin + 'static,
        S::Item: Send + 'static,
        A::Message: From<StreamEvent<S::Item>>,
    {
        let id = self.id_gen.next_stream_id();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.self_handle.clone();
        let fut = stream_forward::<A, S>(stream, handle, cancel_clone);
        self.runtime.spawn(fut);
        self.streams.insert(id, StreamRegistration { token });
        id
    }

    /// Cancels a specific stream by its ID.
    pub fn cancel_stream(&mut self, id: StreamId) -> Result<(), StreamError> {
        match self.streams.remove(&id) {
            Some(entry) => {
                entry.token.cancel();
                Ok(())
            }
            None => Err(StreamError::NotFound),
        }
    }

    /// Cancels all active streams.
    pub fn cancel_all_streams(&mut self) {
        for entry in self.streams.values() {
            entry.token.cancel();
        }
        self.streams.clear();
    }

    /// Returns the number of active streams.
    pub fn active_stream_count(&self) -> usize {
        self.streams.len()
    }
}

// ---------------------------------------------------------------------------
// ChildSpawnBuilder - builder chain for supervised child spawning
// ---------------------------------------------------------------------------

/// Builder for spawning a supervised child actor.
///
/// Created by [`ActorContext::spawn_child`]. Implements
/// [`IntoFuture`](std::future::IntoFuture) so you can `.await` it directly,
/// or chain `.named()`, `.restart_type()`, `.shutdown()`, `.with_config()`
/// before awaiting. Resolves to `Result<ActorHandle<C>, SpawnError>` - see
/// [`spawn_child`](ActorContext::spawn_child) for the error variants.
///
/// # Examples
/// ```rust,no_run
/// use tokio::time::Duration;
/// use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult, RestartType, Shutdown, SupervisionConfig};
///
/// #[derive(Default)]
/// struct Supervisor;
/// #[derive(Default)]
/// struct Worker;
///
/// impl Actor for Worker {
///     type Message = ();
///     type Response = ();
///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
/// }
///
/// impl Actor for Supervisor {
///     type Message = ();
///     type Response = ();
///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
///
///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         // Anonymous child, defaults (Permanent, Timeout(5s))
///         ctx.spawn_child(Worker::default).await?;
///
///         // Named child with custom restart policy
///         ctx.spawn_child(Worker::default)
///             .named("worker")
///             .restart_type(RestartType::Transient)
///             .shutdown(Shutdown::Timeout(Duration::from_secs(10)))
///             .await?;
///         Ok(())
///     }
/// }
/// ```
pub struct ChildSpawnBuilder<'ctx, A: Actor, F, C: Actor> {
    ctx: &'ctx mut ActorContext<A>,
    factory: F,
    name: Option<String>,
    restart_type: RestartType,
    shutdown: Shutdown,
    config: Option<runtime::ActorConfig>,
    start_timeout: Option<Duration>,
    _child: std::marker::PhantomData<C>,
}

impl<'ctx, A: Actor, F, C: Actor> ChildSpawnBuilder<'ctx, A, F, C>
where
    F: Fn() -> C + Send + Sync + 'static,
{
    /// Assigns a name to the child. The name also serves as the child's
    /// [`ActorId`].
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the child's [`RestartType`]. Default: [`RestartType::Permanent`].
    pub fn restart_type(mut self, restart_type: RestartType) -> Self {
        self.restart_type = restart_type;
        self
    }

    /// Sets the child's [`Shutdown`] policy. Default: [`Shutdown::Timeout(5s)`](Shutdown::Timeout).
    pub fn shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Overrides the default [`ActorConfig`](runtime::ActorConfig) for the child.
    pub fn with_config(mut self, config: runtime::ActorConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Bounds the supervisor's wait for this child's initialization to
    /// complete during a restart. `None` (default) waits indefinitely,
    /// matching Erlang/OTP supervisor semantics; setting a bound is a
    /// deliberate deviation that trades strict parity for a supervisor that
    /// can never be stalled by one child's hung init. Initial spawning never
    /// awaits initialization (see [`spawn_child`](ActorContext::spawn_child)),
    /// so this bound applies only to restarts.
    pub fn start_timeout(mut self, dur: Duration) -> Self {
        self.start_timeout = Some(dur);
        self
    }
}

impl<'ctx, A: Actor, F, C: Actor> std::future::IntoFuture for ChildSpawnBuilder<'ctx, A, F, C>
where
    F: Fn() -> C + Send + Sync + 'static,
{
    type Output = Result<ActorHandle<C>, SpawnError>;
    type IntoFuture = std::future::Ready<Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        std::future::ready(self.ctx.spawn_child_internal(
            self.factory,
            self.name,
            self.restart_type,
            self.shutdown,
            self.config,
            self.start_timeout,
        ))
    }
}

// ---------------------------------------------------------------------------
// ScheduleBuilder - builder chain for timer scheduling
// ---------------------------------------------------------------------------

/// Builder for scheduling timers.
///
/// Created by [`ActorContext::schedule`].
/// Chain `.at(instant)` or `.after(delay)` for one-shot timers,
/// or `.every(interval)` for recurring timers, then `.await`.
///
/// Nothing is registered until the terminal schedule is awaited - dropping a
/// builder (or the schedule it produces) without awaiting it registers no
/// timer, and it never fires.
///
/// # Examples
/// ```rust,no_run
/// use tokio::time::Duration;
/// use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult, MissPolicy};
///
/// #[derive(Default)]
/// struct MyActor;
///
/// #[derive(Clone)]
/// enum Msg { Tick }
///
/// impl Actor for MyActor {
///     type Message = Msg;
///     type Response = ();
///     async fn handle(&mut self, _: Msg, _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
///
///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         // One-shot after delay
///         ctx.schedule(Msg::Tick).after(Duration::from_secs(5)).await?;
///
///         // Recurring with default MissPolicy (Skip)
///         ctx.schedule(Msg::Tick).every(Duration::from_millis(100)).await?;
///
///         // Recurring with explicit MissPolicy
///         ctx.schedule(Msg::Tick).every(Duration::from_millis(100))
///             .on_miss(MissPolicy::CatchUp).await?;
///         Ok(())
///     }
/// }
/// ```
#[must_use = "a schedule registers nothing until awaited; a dropped builder never fires"]
pub struct ScheduleBuilder<'ctx, A: Actor> {
    ctx: &'ctx mut ActorContext<A>,
    message: A::Message,
}

impl<'ctx, A: Actor> ScheduleBuilder<'ctx, A> {
    /// Schedule a one-shot message at a specific [`Instant`].
    pub fn at(self, when: Instant) -> OneShotSchedule<'ctx, A> {
        OneShotSchedule {
            ctx: self.ctx,
            message: self.message,
            when,
        }
    }

    /// Schedule a one-shot message after a [`Duration`].
    pub fn after(self, delay: Duration) -> OneShotSchedule<'ctx, A> {
        let when = Instant::now() + delay;
        OneShotSchedule {
            ctx: self.ctx,
            message: self.message,
            when,
        }
    }

    /// Schedule a recurring timer at `interval`. Default `MissPolicy::Skip`.
    ///
    /// Chain `.on_miss(policy)` before `.await` to override. Requires
    /// `Message: Clone` (the template is cloned for every tick) - use
    /// [`ActorContext::every_with`] instead for a message type that is not
    /// `Clone`.
    pub fn every(self, interval: Duration) -> RecurringScheduleBuilder<'ctx, A>
    where
        A::Message: Clone,
    {
        let msg = self.message;
        RecurringScheduleBuilder {
            ctx: self.ctx,
            factory: Box::new(move || msg.clone()),
            interval,
            miss_policy: MissPolicy::Skip,
        }
    }
}

/// Terminal for a one-shot timer. `.await` this to register the timer and
/// get the [`RecurringId`] - the timer does not exist until this future
/// resolves, so a dropped, un-awaited `OneShotSchedule` registers nothing and
/// never fires.
#[must_use = "a schedule registers nothing until awaited; a dropped builder never fires"]
pub struct OneShotSchedule<'ctx, A: Actor> {
    ctx: &'ctx mut ActorContext<A>,
    message: A::Message,
    when: Instant,
}

impl<'ctx, A: Actor> std::future::IntoFuture for OneShotSchedule<'ctx, A> {
    type Output = Result<RecurringId, TimerError>;
    type IntoFuture = std::future::Ready<Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        std::future::ready(self.ctx.register_oneshot(self.message, self.when))
    }
}

/// Builder for a recurring timer. `.await` to register with default
/// `MissPolicy::Skip`, or chain `.on_miss(policy)` first. Returned by
/// [`ScheduleBuilder::every`] (message cloned from a template per tick) and
/// by [`ActorContext::every_with`] (message produced by a factory closure).
#[must_use = "a schedule registers nothing until awaited; a dropped builder never fires"]
pub struct RecurringScheduleBuilder<'ctx, A: Actor> {
    ctx: &'ctx mut ActorContext<A>,
    factory: Box<MessageFactory<A>>,
    interval: Duration,
    miss_policy: MissPolicy,
}

impl<'ctx, A: Actor> RecurringScheduleBuilder<'ctx, A> {
    /// Sets the [`MissPolicy`] for this recurring timer.
    /// Default is [`MissPolicy::Skip`].
    pub fn on_miss(mut self, policy: MissPolicy) -> Self {
        self.miss_policy = policy;
        self
    }
}

impl<'ctx, A: Actor> std::future::IntoFuture for RecurringScheduleBuilder<'ctx, A> {
    type Output = Result<RecurringId, TimerError>;
    type IntoFuture = std::future::Ready<Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        let id = self.ctx.id_gen.next();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.ctx.self_handle.clone();
        let fut = recurring_loop(
            handle,
            self.factory,
            self.interval,
            self.miss_policy,
            cancel_clone,
        );
        self.ctx.runtime.spawn(fut);
        self.ctx.timers.insert(id, TimerRegistration { token });
        std::future::ready(Ok(id))
    }
}

impl<A: Actor> Drop for ActorContext<A> {
    fn drop(&mut self) {
        for entry in self.timers.values() {
            entry.token.cancel();
        }
        for entry in self.streams.values() {
            entry.token.cancel();
        }
    }
}

// ---------------------------------------------------------------------------
// Internal types and helpers
// ---------------------------------------------------------------------------

struct TimerRegistration {
    token: CancellationToken,
}

struct StreamRegistration {
    token: CancellationToken,
}

type MessageFactory<A> = dyn FnMut() -> <A as Actor>::Message + Send + 'static;

async fn recurring_loop<A: Actor>(
    handle: ActorHandle<A>,
    mut factory: Box<MessageFactory<A>>,
    interval: Duration,
    miss_policy: MissPolicy,
    token: CancellationToken,
) {
    let mut next = Instant::now() + interval;
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            _ = time::sleep_until(next) => {
                let msg = (factory.as_mut())();
                let _ = handle.notify(msg).await;
                adjust_next(&mut next, interval, miss_policy, &token, &handle, &mut factory).await;
            }
        }
    }
}

async fn stream_forward<A, S>(mut stream: S, handle: ActorHandle<A>, token: CancellationToken)
where
    A: Actor,
    S: Stream + Send + Unpin + 'static,
    S::Item: Send + 'static,
    A::Message: From<StreamEvent<S::Item>>,
{
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            item = StreamExt::next(&mut stream) => {
                match item {
                    Some(value) => {
                        let msg: A::Message = StreamEvent::Data(value).into();
                        if handle.notify(msg).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let msg: A::Message = StreamEvent::Finished.into();
                        let _ = handle.notify(msg).await;
                        break;
                    }
                }
            }
        }
    }
}

async fn adjust_next<A: Actor>(
    next: &mut Instant,
    interval: Duration,
    miss_policy: MissPolicy,
    token: &CancellationToken,
    handle: &ActorHandle<A>,
    factory: &mut Box<MessageFactory<A>>,
) {
    let now = Instant::now();
    match miss_policy {
        MissPolicy::Skip => {
            *next += interval;
            while *next <= now {
                *next += interval;
            }
        }
        MissPolicy::Delay => {
            *next = now + interval;
        }
        MissPolicy::CatchUp => {
            *next += interval;
            while *next <= now {
                if token.is_cancelled() {
                    return;
                }
                let msg = (factory.as_mut())();
                if handle.try_notify(msg).is_err() {
                    break;
                }
                *next += interval;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::supervision::{SupervisionConfig, SupervisionState};

    #[derive(Default)]
    struct Dummy;

    impl Actor for Dummy {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    fn dummy_ctx() -> ActorContext<Dummy> {
        let (tx, _rx) = mpsc::channel(1);
        let (system_tx, _system_rx) = mpsc::channel(1);
        let (status_tx, status_rx) = watch::channel(ActorStatus::Running);
        let id = ActorId::from("dummy");
        let handle = ActorHandle::new(id.clone(), tx, system_tx.clone(), 1, status_rx);
        ActorContext::new(
            id,
            handle,
            Handle::current(),
            status_tx,
            system_tx,
            None,
            None,
            Some(SupervisionState::new(SupervisionConfig::default())),
        )
    }

    #[tokio::test]
    async fn start_timeout_defaults_to_none() {
        let mut ctx = dummy_ctx();
        let builder = ctx.spawn_child(Dummy::default);
        assert_eq!(builder.start_timeout, None);
    }

    #[tokio::test]
    async fn start_timeout_builder_sets_value() {
        let mut ctx = dummy_ctx();
        let builder = ctx
            .spawn_child(Dummy::default)
            .start_timeout(Duration::from_millis(200));
        assert_eq!(builder.start_timeout, Some(Duration::from_millis(200)));
    }
}
