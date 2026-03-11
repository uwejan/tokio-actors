//! Actor execution context providing timers, streams, and supervision hooks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use futures_core::Stream;
use tokio_stream::StreamExt;

use crate::actor::handle::ActorHandle;
use crate::actor::supervision::{ChildSpec, ChildState, SupervisionState};
use crate::actor::{runtime, Actor};
use crate::error::{ActorError, StreamError, SupervisionError, TimerError};
use crate::system::ActorSystem;
use crate::types::{
    ActorId, ActorStatus, ChildInfo, MissPolicy, RecurringId, RecurringIdGenerator, RestartType,
    Shutdown, StreamEvent, StreamId, SystemMessage,
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
    status: ActorStatus,
    // Supervision fields
    system_tx: mpsc::Sender<SystemMessage>,
    system: Option<Arc<ActorSystem>>,
    name: Option<String>,
    supervision: Option<SupervisionState>,
}

impl<A: Actor> ActorContext<A> {
    pub(crate) fn new(
        actor_id: ActorId,
        handle: ActorHandle<A>,
        runtime: Handle,
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
            status: ActorStatus::Initializing,
            system_tx,
            system,
            name,
            supervision,
        }
    }

    // -- Identity & status --------------------------------------------------

    /// Returns the unique identifier of this actor.
    pub fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    /// Returns the registered name of this actor, if any.
    pub fn actor_name(&self) -> Option<&String> {
        self.name.as_ref()
    }

    /// Returns a handle to this actor.
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
        self.status
    }

    pub(crate) fn set_status(&mut self, status: ActorStatus) {
        self.status = status;
    }

    // -- Supervision --------------------------------------------------------

    pub(crate) fn supervision_mut(&mut self) -> Option<&mut SupervisionState> {
        self.supervision.as_mut()
    }

    pub(crate) fn supervision_ref(&self) -> Option<&SupervisionState> {
        self.supervision.as_ref()
    }

    /// Creates a [`ChildSpawnBuilder`] for spawning a supervised child actor.
    ///
    /// The factory is called immediately to create the initial instance, and stored
    /// for future restarts. Chain `.named()`, `.restart_type()`, `.shutdown()`,
    /// `.with_config()` before `.await`ing to customize.
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
    /// - [`SupervisionError::NotASupervisor`] if this actor has no supervision config.
    /// - [`SpawnError`](crate::error::SpawnError) variants if the child fails to spawn.
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
    ) -> Result<ActorHandle<C>, crate::error::ActorError>
    where
        F: Fn() -> C + Send + Sync + 'static,
        C: Actor,
    {
        let sup = self
            .supervision
            .as_mut()
            .ok_or(SupervisionError::NotASupervisor)?;

        let actor = factory();
        let child_config = config.unwrap_or_default();
        let child_id: ActorId = name
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
            .into();

        let (child_handle, join_handle) = runtime::spawn_actor(
            child_id.clone(),
            actor,
            child_config,
            name.clone(),
            self.system.clone(),
            Some(self.system_tx.clone()), // wire parent notification
        )?;

        let child_state = ChildState {
            id: child_id,
            name,
            spec: ChildSpec {
                restart_type,
                shutdown,
            },
            join_handle,
            system_tx: child_handle.system_tx(),
            is_alive: true,
            pending_restart_seq: None,
        };

        sup.registry.register(child_state);

        // Store the factory for restarts, captured in a restart closure
        // that the runtime can invoke later.
        let parent_system_tx = self.system_tx.clone();
        let system_clone = self.system.clone();
        let factory = Arc::new(factory);
        let child_id_for_restart = child_handle.id().clone();

        sup.restart_fns.insert(
            child_id_for_restart,
            Box::new(move |seq, child_name| {
                let factory = Arc::clone(&factory);
                let parent_tx = parent_system_tx.clone();
                let system = system_clone.clone();
                Box::pin(async move {
                    let actor = factory();
                    let child_config = runtime::ActorConfig::default();
                    let child_id: ActorId = child_name
                        .clone()
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
                        .into();

                    match runtime::spawn_actor(
                        child_id.clone(),
                        actor,
                        child_config,
                        child_name,
                        system,
                        Some(parent_tx.clone()),
                    ) {
                        Ok((handle, join)) => {
                            let _ = parent_tx
                                .send(SystemMessage::RestartComplete {
                                    seq,
                                    child_id,
                                    new_system_tx: handle.system_tx(),
                                    new_join_handle: join,
                                })
                                .await;
                        }
                        Err(_) => {
                            let _ = parent_tx
                                .send(SystemMessage::ChildStopped(
                                    crate::types::ChildStoppedInternal {
                                        child_id,
                                        reason: crate::types::StopReason::Failure(
                                            crate::error::ActorError::user("restart spawn failed"),
                                        ),
                                    },
                                ))
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

    /// Manually stops a supervised child.
    pub async fn stop_child(&mut self, child: impl Into<ActorId>) -> Result<(), SupervisionError> {
        let id = child.into();
        let sup = self
            .supervision
            .as_ref()
            .ok_or(SupervisionError::NotASupervisor)?;

        let child = sup
            .registry
            .get(&id)
            .ok_or(SupervisionError::ChildNotFound(id.clone()))?;

        let _ = child
            .system_tx
            .send(SystemMessage::Stop(crate::types::StopReason::ParentRequest))
            .await;

        Ok(())
    }

    // -- Timers -------------------------------------------------------------

    /// Creates a [`ScheduleBuilder`] for scheduling a message.
    ///
    /// Chain `.at(instant)` or `.after(delay)` for one-shot, or `.every(interval)`
    /// for recurring timers. Then `.await` to register.
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
    pub fn schedule(&mut self, message: A::Message) -> ScheduleBuilder<'_, A>
    where
        A::Message: Clone + Sync + Send + 'static,
    {
        ScheduleBuilder { ctx: self, message }
    }

    /// Internal: register a one-shot timer.
    fn register_oneshot(
        &mut self,
        message: A::Message,
        when: Instant,
    ) -> Result<RecurringId, TimerError>
    where
        A::Message: Send + 'static,
    {
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

    // -- Streams ------------------------------------------------------------

    /// Attaches an external stream to this actor's mailbox.
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
/// Created by [`ActorContext::spawn_child`]. Implements [`IntoFuture`] so you
/// can `.await` it directly, or chain `.named()`, `.restart_type()`,
/// `.shutdown()`, `.with_config()` before awaiting.
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
    _child: std::marker::PhantomData<C>,
}

impl<'ctx, A: Actor, F, C: Actor> ChildSpawnBuilder<'ctx, A, F, C>
where
    F: Fn() -> C + Send + Sync + 'static,
{
    /// Assigns a name to the child. The name also serves as the child's
    /// [`ActorId`](crate::types::ActorId).
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
}

impl<'ctx, A: Actor, F, C: Actor> std::future::IntoFuture for ChildSpawnBuilder<'ctx, A, F, C>
where
    F: Fn() -> C + Send + Sync + 'static,
{
    type Output = Result<ActorHandle<C>, crate::error::ActorError>;
    type IntoFuture = std::future::Ready<Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        std::future::ready(self.ctx.spawn_child_internal(
            self.factory,
            self.name,
            self.restart_type,
            self.shutdown,
            self.config,
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
pub struct ScheduleBuilder<'ctx, A: Actor> {
    ctx: &'ctx mut ActorContext<A>,
    message: A::Message,
}

impl<'ctx, A: Actor> ScheduleBuilder<'ctx, A>
where
    A::Message: Clone + Sync + Send + 'static,
{
    /// Schedule a one-shot message at a specific [`Instant`].
    pub fn at(self, when: Instant) -> OneShotSchedule {
        OneShotSchedule {
            result: self.ctx.register_oneshot(self.message, when),
        }
    }

    /// Schedule a one-shot message after a [`Duration`].
    pub fn after(self, delay: Duration) -> OneShotSchedule {
        let when = Instant::now() + delay;
        OneShotSchedule {
            result: self.ctx.register_oneshot(self.message, when),
        }
    }

    /// Schedule a recurring timer at `interval`. Default `MissPolicy::Skip`.
    ///
    /// Chain `.on_miss(policy)` before `.await` to override.
    pub fn every(self, interval: Duration) -> RecurringScheduleBuilder<'ctx, A> {
        let msg = self.message;
        RecurringScheduleBuilder {
            ctx: self.ctx,
            factory: Arc::new(move || msg.clone()),
            interval,
            miss_policy: MissPolicy::Skip,
        }
    }
}

/// Terminal for a one-shot timer. `.await` this to get the [`RecurringId`].
pub struct OneShotSchedule {
    result: Result<RecurringId, TimerError>,
}

impl std::future::IntoFuture for OneShotSchedule {
    type Output = Result<RecurringId, TimerError>;
    type IntoFuture = std::future::Ready<Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        std::future::ready(self.result)
    }
}

/// Builder for a recurring timer. `.await` to register with default
/// `MissPolicy::Skip`, or chain `.on_miss(policy)` first.
pub struct RecurringScheduleBuilder<'ctx, A: Actor> {
    ctx: &'ctx mut ActorContext<A>,
    factory: Arc<MessageFactory<A>>,
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

type MessageFactory<A> = dyn Fn() -> <A as Actor>::Message + Send + Sync + 'static;

async fn recurring_loop<A: Actor>(
    handle: ActorHandle<A>,
    factory: Arc<MessageFactory<A>>,
    interval: Duration,
    miss_policy: MissPolicy,
    token: CancellationToken,
) {
    let mut next = Instant::now() + interval;
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            _ = time::sleep_until(next) => {
                let msg = (factory.as_ref())();
                let _ = handle.notify(msg).await;
                adjust_next(&mut next, interval, miss_policy, &token, &handle, &factory).await;
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
    factory: &Arc<MessageFactory<A>>,
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
                let msg = (factory.as_ref())();
                if handle.try_notify(msg).is_err() {
                    break;
                }
                *next += interval;
            }
        }
    }
}
