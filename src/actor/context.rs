//! Actor execution context providing timers, streams, and supervision hooks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use futures_core::Stream;
use tokio_stream::StreamExt;

use tokio::task::{AbortHandle, JoinError, JoinSet};

use crate::actor::handle::ActorHandle;
use crate::actor::panic::{catch_sync, payload_into_string};
use crate::actor::runtime::prune_spec_if_needed;
use crate::actor::supervision::{
    ChildLifecycle, ChildLinkGuard, ChildSpec, ChildState, DeathOutcome, GroupPhase, ManualStop,
    RestartOutcome, SupervisionState, KILL_GRACE, SUPERVISOR_KILL_GRACE,
};
use crate::actor::{runtime, Actor};
use crate::error::{ActorError, SpawnError, StreamError, SupervisionError, TimerError};
use crate::system::ActorSystem;
use crate::types::{
    ActorId, ActorStatus, ChildEvent, ChildFate, ChildInfo, MissPolicy, RecurringId,
    RecurringIdGenerator, RestartStrategy, RestartType, Shutdown, StopLane, StopReason,
    StreamEvent, StreamId, SupervisionAction,
};

/// True if `id` is a member of an in-flight group restart's `Stopping`
/// phase, awaiting its own individual death. Distinguished from any other
/// group membership (an in-flight `Restarting` slot, or a queued-but-not-yet-
/// started one) because it is the only case where a caller's `terminate_child`
/// can still record an override intent instead of being rejected outright:
/// the member's own stop signal is already in flight, so there is nothing
/// left to race.
fn group_stopping_awaiting(sup: &SupervisionState, id: &ActorId) -> bool {
    matches!(
        sup.pending_group.as_ref(),
        Some(GroupPhase::Stopping(group)) if group.awaiting.contains(id)
    )
}

/// Actor execution context providing timers, streams, supervision, and runtime hooks.
pub struct ActorContext<A: Actor> {
    actor_id: ActorId,
    self_handle: ActorHandle<A>,
    runtime: Handle,
    timers: HashMap<RecurringId, TimerRegistration>,
    streams: HashMap<StreamId, StreamRegistration>,
    id_gen: RecurringIdGenerator,
    /// Every timer and stream forwarder task, owned here so none of them can
    /// outlive this context. A finished task (a fired one-shot, a stream that
    /// ran out of items, a cancelled loop noticing its token) is reaped from
    /// here the moment the run loop observes its completion, which is also
    /// when its entry in `timers`/`streams` is removed - so a finished
    /// forwarder never lingers in either count.
    forwarders: JoinSet<()>,
    /// Maps a forwarder task's tokio-assigned id back to the registration it
    /// belongs to, so a completion - however it completes - can find and
    /// remove the right entry from `timers`/`streams` even when a panic
    /// leaves nothing but a [`JoinError`] to identify it by.
    forwarder_kinds: HashMap<tokio::task::Id, ForwarderKind>,
    /// The most recent forwarder panic, if any. A forwarder dying is never
    /// this actor dying: the message loop keeps running exactly as before,
    /// and this is simply where that panic becomes observable.
    last_forwarder_error: Option<ActorError>,
    last_error: Option<ActorError>,
    /// Runtime status plane (`process_info`-style, see the `handle` module
    /// docs). Every write goes through `send_replace`, which always succeeds
    /// even if every `ActorHandle` (and therefore every receiver) has been
    /// dropped - a plain `send` would error at zero receivers.
    status_tx: watch::Sender<ActorStatus>,
    // Supervision fields
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
            forwarders: JoinSet::new(),
            forwarder_kinds: HashMap::new(),
            last_forwarder_error: None,
            last_error: None,
            status_tx,
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

    /// Returns the most recent panic from a timer or stream forwarder task,
    /// if any. A forwarder panic never stops this actor - the message loop
    /// keeps running - so this is the only way to observe one after the
    /// fact.
    pub fn last_forwarder_error(&self) -> Option<&ActorError> {
        self.last_forwarder_error.as_ref()
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

    /// Takes the supervision state out of the context, leaving `None`
    /// behind. Used only by the run loop's Kill cascade-inversion path:
    /// dropping the returned value directly aborts every live child's
    /// watcher task without this actor ever awaiting any of them (see the
    /// `runtime` module docs).
    pub(crate) fn take_supervision(&mut self) -> Option<SupervisionState> {
        self.supervision.take()
    }

    /// Borrows the supervisor's restart and death planes (if this actor is a
    /// supervisor) together with the forwarder plane every actor has,
    /// disjointly enough for the run loop's combined `select!` to poll all
    /// three in one branch. One accessor, not three, because that branch's
    /// future must hold a single `&mut ActorContext` borrow for its whole
    /// span - see the run loop's own docs.
    #[allow(clippy::type_complexity)]
    pub(crate) fn supervision_planes_mut(
        &mut self,
    ) -> (
        Option<(&mut JoinSet<RestartOutcome>, &mut JoinSet<DeathOutcome>)>,
        &mut JoinSet<()>,
    ) {
        let planes = self
            .supervision
            .as_mut()
            .map(|sup| (&mut sup.restart_set, &mut sup.death_set));
        (planes, &mut self.forwarders)
    }

    /// Reaps one forwarder task's completion: removes its registration from
    /// `timers`/`streams` (a fired one-shot or a finished stream leaves zero
    /// registrations behind) and, if it ended in a panic, records it as
    /// [`last_forwarder_error`](Self::last_forwarder_error). A completion
    /// whose registration was already removed (an explicit `cancel_timer`/
    /// `cancel_stream` beat it here) is a harmless no-op.
    pub(crate) fn reap_forwarder(&mut self, outcome: Result<(tokio::task::Id, ()), JoinError>) {
        let (task_id, panic) = match outcome {
            Ok((id, ())) => (id, None),
            Err(err) => {
                let id = err.id();
                let panic = if err.is_panic() {
                    match err.try_into_panic() {
                        Ok(payload) => Some(ActorError::Panic(payload_into_string(payload))),
                        Err(_) => Some(ActorError::Panic("panic payload unavailable".to_string())),
                    }
                } else {
                    None
                };
                (id, panic)
            }
        };
        if let Some(kind) = self.forwarder_kinds.remove(&task_id) {
            match kind {
                ForwarderKind::Timer(id) => {
                    self.timers.remove(&id);
                }
                ForwarderKind::Stream(id) => {
                    self.streams.remove(&id);
                }
            }
        }
        if let Some(err) = panic {
            self.last_forwarder_error = Some(err);
        }
    }

    /// Spawns a forwarder task onto the forwarder plane, remembering which
    /// registration it belongs to so a later completion - success, or a
    /// caught-by-tokio panic - can find its way back to the right entry in
    /// `timers`/`streams`.
    fn spawn_forwarder<F>(&mut self, kind: ForwarderKind, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let abort = self.forwarders.spawn_on(fut, &self.runtime);
        self.forwarder_kinds.insert(abort.id(), kind);
    }

    /// Resolves this actor's system, falling back to the default system
    /// exactly like every spawn path does when no system was explicitly
    /// targeted.
    pub(crate) fn resolved_system(&self) -> Arc<ActorSystem> {
        self.system.clone().unwrap_or_else(ActorSystem::default)
    }

    /// The Tokio runtime handle this actor was spawned on.
    pub(crate) fn runtime_handle(&self) -> &Handle {
        &self.runtime
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
        let is_supervisor = child_config.supervision.is_some();

        let (child_handle, join_handle, child_status_tx) = runtime::spawn_actor(
            child_id.clone(),
            actor,
            child_config.clone(),
            name.clone(),
            self.system.clone(),
            None,
            false,
        )?;

        // The parent spawns and owns the watcher (initial incarnation 0),
        // together with its link guard - constructed here, in the parent's
        // own synchronous code, before the watcher task is spawned, so an
        // abort landing before the watcher is ever polled still fires it.
        // The abort handle is taken first - the Kill escalation backstop.
        let abort = join_handle.abort_handle();
        let grace = if is_supervisor {
            SUPERVISOR_KILL_GRACE
        } else {
            KILL_GRACE
        };
        let reaper = self.resolved_system().reaper_handle(self.runtime_handle());
        // Cloned before the initial guard consumes `reaper` by value: the
        // restart closure below needs the same per-system reaper feed for
        // every future incarnation's own link guard.
        let restart_reaper = reaper.clone();
        let guard = ChildLinkGuard::new(
            child_handle.stop_lane(),
            child_status_tx,
            abort.clone(),
            reaper,
            grace,
        );
        let (fate_tx, fate_rx) = watch::channel(None);

        let system_clone = self.system.clone();

        // Every registration - this initial spawn included - draws its
        // incarnation token from the system's single monotonic counter, so
        // no two incarnations of any child, under any name, ever collide
        // (see `ActorSystem::next_incarnation`).
        let incarnation = self.resolved_system().next_incarnation();

        let sup = self
            .supervision
            .as_mut()
            .expect("supervision presence checked above");
        runtime::spawn_watcher(
            &mut sup.death_set,
            child_id.clone(),
            incarnation,
            join_handle,
            fate_tx,
            guard,
        );

        let child_state = ChildState {
            id: child_id.clone(),
            name: name.clone(),
            spec: ChildSpec {
                restart_type,
                shutdown,
                start_timeout,
                is_supervisor,
            },
            fate_rx,
            abort,
            stop_lane: child_handle.stop_lane(),
            lifecycle: ChildLifecycle::Running(incarnation),
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
        // Decides the fresh incarnation's link-guard grace on every restart,
        // exactly like the initial spawn's own guard above.
        let restart_is_supervisor = is_supervisor;

        sup.restart_fns.insert(
            child_id,
            Box::new(move |seq| {
                let factory = Arc::clone(&factory);
                let system = system_clone.clone();
                let child_id = restart_id.clone();
                let name = restart_name.clone();
                let config = restart_config.clone();
                let start_timeout = restart_start_timeout;
                let is_supervisor = restart_is_supervisor;
                let reaper = restart_reaper.clone();
                Box::pin(async move {
                    // A panicking factory must not kill the restart task
                    // silently: nothing was ever spawned, so there is no
                    // incarnation to guard - this surfaces as FactoryFailed
                    // and charges the budget when the event re-enters
                    // strategy evaluation.
                    let actor = match catch_sync(&*factory) {
                        Ok(actor) => actor,
                        Err(payload) => {
                            let msg = payload_into_string(payload);
                            let reason =
                                StopReason::Failure(SupervisionError::FactoryFailed(msg).into());
                            return RestartOutcome::Failed {
                                child_id,
                                incarnation: seq,
                                reason,
                            };
                        }
                    };

                    let (ack_tx, ack_rx) = oneshot::channel();
                    let (handle, join, status_tx) = match runtime::spawn_actor(
                        child_id.clone(),
                        actor,
                        config,
                        name,
                        system,
                        Some(ack_tx),
                        false,
                    ) {
                        Ok(spawned) => spawned,
                        Err(err) => {
                            let reason = StopReason::Failure(ActorError::Spawn(err));
                            return RestartOutcome::Failed {
                                child_id,
                                incarnation: seq,
                                reason,
                            };
                        }
                    };

                    // Armed the instant the spawn succeeded - before the init
                    // ack is ever awaited - so a parent that dies mid-restart
                    // (dropping its whole restart plane) still kills this
                    // fresh incarnation through the guard's own `Drop`,
                    // whatever stage the wait below is at: pre-ack, mid-init,
                    // or hung forever under `start_timeout: None` (the guard,
                    // not the ack, is the backstop there). The abort handle
                    // is taken first - the Kill escalation backstop.
                    let abort = join.abort_handle();
                    let new_stop_lane = handle.stop_lane();
                    let grace = if is_supervisor {
                        SUPERVISOR_KILL_GRACE
                    } else {
                        KILL_GRACE
                    };
                    let guard = ChildLinkGuard::new(
                        new_stop_lane.clone(),
                        status_tx,
                        abort.clone(),
                        reaper,
                        grace,
                    );

                    // Await the same init contract the top-level spawn
                    // awaits (pre_start + on_started), bounded by the
                    // child's start_timeout if one was set; None waits
                    // indefinitely (OTP start_link parity) - the guard above
                    // is the only backstop in that case.
                    let acked = match start_timeout {
                        Some(dur) => time::timeout(dur, ack_rx).await,
                        None => Ok(ack_rx.await),
                    };

                    match acked {
                        Ok(Ok(Ok(()))) => RestartOutcome::Adopted {
                            child_id,
                            incarnation: seq,
                            new_stop_lane,
                            new_join: join,
                            guard,
                        },
                        Ok(Ok(Err(err))) => {
                            // The fresh incarnation's own task has already
                            // fully exited (its ack fires only from the
                            // teardown tail): the guard's effects would be
                            // harmless no-ops here, same as on an
                            // already-stopped child anywhere else.
                            drop(guard);
                            RestartOutcome::Failed {
                                child_id,
                                incarnation: seq,
                                reason: StopReason::Failure(err),
                            }
                        }
                        Ok(Err(_recv_err)) => {
                            // The ack channel closed without a value: the
                            // fresh incarnation's task ended before it could
                            // ack (a panic outside the caught callback
                            // boundary, or similar) - already fully exited.
                            drop(guard);
                            let reason = StopReason::Failure(ActorError::user(
                                "restart ack channel closed before a reply arrived",
                            ));
                            RestartOutcome::Failed {
                                child_id,
                                incarnation: seq,
                                reason,
                            }
                        }
                        Err(_elapsed) => {
                            // start_timeout expired: the guard - not the ack
                            // - is the backstop for a hung init. Dropping it
                            // here fires the same Kill-raise/status-force/
                            // reaper ladder any other stray incarnation
                            // teardown uses; the real join is then awaited
                            // (bounded, generous enough for the reaper's own
                            // abort to have already run) so this incarnation
                            // is fully gone - its name free - before the
                            // failure is reported upward.
                            drop(guard);
                            let _ = time::timeout(grace + KILL_GRACE, join).await;
                            RestartOutcome::Failed {
                                child_id,
                                incarnation: seq,
                                reason: StopReason::Failure(ActorError::Spawn(
                                    SpawnError::StartTimeout,
                                )),
                            }
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
    /// and a SimpleOneForOne child's dynamic spec is removed entirely. A
    /// child that turns out to have crashed (`Failure`) rather than obeyed
    /// the stop signal is NOT treated as a manual bounce: it is charged to
    /// the restart budget and evaluated by the ordinary strategy instead,
    /// exactly like any other crash.
    ///
    /// Honors the child's [`Shutdown`] policy: a vetoing or slow child is
    /// escalated to Kill at the timeout, with a task-abort backstop behind
    /// it. Returns once the child's fate cell is populated - which tokio
    /// guarantees only happens after every one of the child's own drops,
    /// its registry entry included, have already run - so by the time this
    /// resolves the child is fully gone and its name is free (idempotent
    /// `Ok` if it was already stopped).
    ///
    /// A supervisor-child's stop recursively waits for that subtree's own
    /// teardown: the worst case is the sum of every level's [`Shutdown`]
    /// budget below it, and a [`Shutdown::Infinity`] descendant anywhere in
    /// that subtree makes the wait unbounded (accepted; escape via the
    /// escalation ladder on every non-`Infinity` policy in the chain).
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
    /// deleted immediately (OTP parity), as is a SimpleOneForOne child's. A
    /// racing crash (the child dies of `Failure` at roughly the same moment)
    /// is absorbed into this call's own completion rather than being
    /// reported as an independent failure (OTP unlink-and-flush parity).
    ///
    /// Returns only once the child is fully gone and its registered name is
    /// free: [`delete_child`](Self::delete_child) and a same-name
    /// [`spawn_child`](Self::spawn_child) are guaranteed to succeed
    /// afterward, even from the very same handler invocation that awaited
    /// this call.
    ///
    /// Blocking, escalation, and error semantics (including the recursive
    /// subtree-teardown latency note) match [`stop_child`](Self::stop_child).
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
        let seq = self.resolved_system().next_incarnation();
        let sup = self
            .supervision
            .as_mut()
            .ok_or(SupervisionError::NotASupervisor)?;
        if sup.in_pending_group(&id) {
            return Err(SupervisionError::ChildRestarting(id));
        }
        match sup.registry.get(&id).map(|c| &c.lifecycle) {
            None => return Err(SupervisionError::ChildNotFound(id)),
            Some(ChildLifecycle::Running(_)) => return Err(SupervisionError::ChildRunning(id)),
            Some(ChildLifecycle::Restarting { .. }) | Some(ChildLifecycle::Stopping { .. }) => {
                return Err(SupervisionError::ChildRestarting(id))
            }
            Some(ChildLifecycle::Down { .. }) => {}
        }
        sup.initiate(&id, seq);
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
        match sup.registry.get(&id).map(|c| &c.lifecycle) {
            None => return Err(SupervisionError::ChildNotFound(id)),
            Some(ChildLifecycle::Running(_)) => return Err(SupervisionError::ChildRunning(id)),
            Some(ChildLifecycle::Restarting { .. }) | Some(ChildLifecycle::Stopping { .. }) => {
                return Err(SupervisionError::ChildRestarting(id))
            }
            Some(ChildLifecycle::Down { .. }) => {}
        }
        sup.registry.remove(&id);
        sup.restart_fns.remove(&id);
        Ok(())
    }

    /// Shared machinery for stop_child / terminate_child.
    ///
    /// The commit is synchronous: the ledger moves to `Stopping{kind}` and
    /// the lane is raised with ZERO awaits between the two, so a caller that
    /// drops this future immediately after issuing it has still made the
    /// stop happen - the run loop completes it idempotently regardless (a
    /// dropped future never un-raises a lane or un-commits a transition).
    /// From there this awaits the child's own fate cell (populated only
    /// after every one of the child's drops, its registry entry included,
    /// have already run) under the child's `Shutdown` ladder, escalating to
    /// Kill and then abort() exactly like the automatic shutdown path.
    ///
    /// A member of an in-flight `OneForAll`/`RestForOne` group stop is
    /// rejected UNLESS this is a `Terminate` on a member already awaiting its
    /// individual death: that intent is recorded and honored over the
    /// group's default restart once the group evaluates it, and this call
    /// still awaits that same fate cell before returning.
    async fn manual_stop_child(
        &mut self,
        id: ActorId,
        kind: ManualStop,
    ) -> Result<(), SupervisionError> {
        let sup = self
            .supervision
            .as_ref()
            .ok_or(SupervisionError::NotASupervisor)?;

        if group_stopping_awaiting(sup, &id) {
            if !matches!(kind, ManualStop::Terminate) {
                return Err(SupervisionError::ChildRestarting(id));
            }
            let (mut fate_rx, stop_lane, shutdown, abort, incarnation) = match sup.registry.get(&id)
            {
                Some(child) => (
                    child.fate_rx.clone(),
                    child.stop_lane.clone(),
                    child.spec.shutdown,
                    child.abort.clone(),
                    child.lifecycle.incarnation(),
                ),
                None => return Err(SupervisionError::ChildNotFound(id)),
            };
            if let Some(sup) = self.supervision_mut() {
                if let Some(GroupPhase::Stopping(group)) = sup.pending_group.as_mut() {
                    group.manual_overrides.insert(id.clone());
                }
            }
            // The group already raised the appropriate signal for this
            // member's Shutdown policy; re-raising is a harmless no-op (the
            // lane coalesces same-or-lower-severity raises).
            stop_lane.raise(Self::initial_signal(shutdown));
            let fate =
                Self::await_fate_under_shutdown(&id, &mut fate_rx, shutdown, &stop_lane, abort)
                    .await?;
            // Settle synchronously, exactly like the primary path's commit:
            // the ledger moves to `Down{event_pending: true}` and the
            // `on_child_stopped` snapshot is recorded right now, in THIS
            // handler invocation, so a same-handler `delete_child` and a
            // same-name `spawn_child` both see a settled child immediately
            // instead of waiting for the run loop to reach this death on its
            // own (it cannot: it is busy running this very handler). The run
            // loop's death arm still owns removing this member from the
            // group's `awaiting` set and, once that empties, starting the
            // restart chain - this call never touches either.
            let reason = if matches!(fate.reason, StopReason::Cancelled) {
                StopReason::Kill
            } else {
                fate.reason
            };
            self.classify_manual_completion(&id, incarnation, kind, reason);
            return Ok(());
        }

        if sup.in_pending_group(&id) {
            return Err(SupervisionError::ChildRestarting(id));
        }

        let (stop_lane, shutdown, abort, mut fate_rx, running) = match sup.registry.get(&id) {
            Some(child) => (
                child.stop_lane.clone(),
                child.spec.shutdown,
                child.abort.clone(),
                child.fate_rx.clone(),
                matches!(child.lifecycle, ChildLifecycle::Running(_)),
            ),
            None => return Err(SupervisionError::ChildNotFound(id)),
        };
        if !running {
            let in_flight = matches!(
                sup.registry.get(&id).map(|c| &c.lifecycle),
                Some(ChildLifecycle::Restarting { .. }) | Some(ChildLifecycle::Stopping { .. })
            );
            // OTP terminate_child on an already-stopped (kept) spec returns
            // ok; anything still in flight is reported as restarting.
            return if in_flight {
                Err(SupervisionError::ChildRestarting(id))
            } else {
                Ok(())
            };
        }

        let incarnation = sup
            .registry
            .get(&id)
            .map(|c| c.lifecycle.incarnation())
            .unwrap_or(0);
        // Child-died-first check: a sync `borrow()` before any commit or
        // await. The watcher can populate the fate cell strictly before the
        // run loop's own ledger catches up (they are independent
        // observers of the same completion), so this can legitimately be
        // `Some` here even though the ledger still reads `Running`.
        let already = fate_rx.borrow().clone();

        let fate = if let Some(fate) = already {
            if let Some(sup) = self.supervision_mut() {
                if let Some(child) = sup.registry.get_mut(&id) {
                    let _ = child.transition(ChildLifecycle::Down {
                        incarnation,
                        event_pending: false,
                    });
                }
            }
            fate
        } else {
            // COMMIT: ledger transition + lane raise, zero awaits between.
            if let Some(sup) = self.supervision_mut() {
                if let Some(child) = sup.registry.get_mut(&id) {
                    let _ = child.transition(ChildLifecycle::Stopping { incarnation, kind });
                }
            }
            stop_lane.raise(Self::initial_signal(shutdown));
            Self::await_fate_under_shutdown(&id, &mut fate_rx, shutdown, &stop_lane, abort).await?
        };

        // Within a supervision tree an aborted task can only be our own Kill
        // escalation (AbortHandles are never exposed) - present it truthfully,
        // exactly like the run loop's own death arm does for a natural death.
        let reason = if matches!(fate.reason, StopReason::Cancelled) {
            StopReason::Kill
        } else {
            fate.reason
        };
        self.classify_manual_completion(&id, incarnation, kind, reason);
        Ok(())
    }

    /// The signal a manual stop's commit raises, purely a function of the
    /// child's `Shutdown` policy - `kind` decides what happens once the real
    /// fate is observed, not what gets signaled.
    fn initial_signal(shutdown: Shutdown) -> StopReason {
        match shutdown {
            Shutdown::Kill => StopReason::Kill,
            Shutdown::Timeout(_) | Shutdown::Infinity => StopReason::ParentRequest,
        }
    }

    /// Classifies a manual stop's REAL, observed disposition once its fate
    /// cell resolves, and either builds the `on_child_stopped` snapshot the
    /// run loop's death arm will deliver, or - for a `Bounce` racing a
    /// genuine `Failure` - clears the manual intent entirely so the death is
    /// instead evaluated as an ordinary crash (budget charge, strategy,
    /// restarts), exactly like any other failure.
    ///
    /// Called from two places: `manual_stop_child` itself, once its own
    /// await on the fate cell resolves, and - for cancel-safety - the run
    /// loop's death arm (`handle_child_stopped`), as a fallback for a caller
    /// that dropped its `terminate_child`/`stop_child` future right after the
    /// synchronous commit and before ever observing the fate: the ledger is
    /// still `Stopping{kind}` when the death arm gets there, so it finishes
    /// the classification itself instead of ever treating an un-classified
    /// manual stop as a plain, strategy-evaluated crash.
    pub(crate) fn classify_manual_completion(
        &mut self,
        id: &ActorId,
        incarnation: u64,
        kind: ManualStop,
        reason: StopReason,
    ) {
        if matches!(kind, ManualStop::Bounce) && matches!(reason, StopReason::Failure(_)) {
            if let Some(sup) = self.supervision_mut() {
                if let Some(child) = sup.registry.get_mut(id) {
                    let _ = child.transition(ChildLifecycle::Down {
                        incarnation,
                        event_pending: false,
                    });
                }
            }
            return;
        }

        let simple = self
            .supervision_ref()
            .map(|sup| matches!(sup.config.strategy, RestartStrategy::SimpleOneForOne))
            .unwrap_or(false);
        let restart_type = self
            .supervision_ref()
            .and_then(|sup| sup.registry.get(id))
            .map(|c| c.spec.restart_type);
        // Captured before any pruning below, which may remove the entry
        // entirely (Terminate on Temporary, or a SimpleOneForOne bounce).
        let child_name = self
            .supervision_ref()
            .and_then(|sup| sup.registry.get(id))
            .and_then(|c| c.name.clone());

        let action = match kind {
            ManualStop::Terminate => {
                prune_spec_if_needed(self, id);
                SupervisionAction::Removed
            }
            ManualStop::Bounce if simple => {
                if let Some(sup) = self.supervision_mut() {
                    sup.registry.remove(id);
                    sup.restart_fns.remove(id);
                }
                SupervisionAction::Removed
            }
            ManualStop::Bounce if matches!(restart_type, Some(RestartType::Permanent)) => {
                // Budget-FREE restart: "manual stop is not a failure".
                let seq = self.resolved_system().next_incarnation();
                if let Some(sup) = self.supervision_mut() {
                    sup.initiate(id, seq);
                }
                SupervisionAction::RestartInitiated
            }
            ManualStop::Bounce => {
                prune_spec_if_needed(self, id);
                SupervisionAction::Removed
            }
        };

        // Any entry still registered and not already moved on to a fresh
        // restart reflects `Down` with the event pending for the death arm.
        if let Some(sup) = self.supervision_mut() {
            if let Some(child) = sup.registry.get_mut(id) {
                if !matches!(child.lifecycle, ChildLifecycle::Restarting { .. }) {
                    let _ = child.transition(ChildLifecycle::Down {
                        incarnation,
                        event_pending: true,
                    });
                }
            }
            sup.pending_manual_events.insert(
                (id.clone(), incarnation),
                ChildEvent {
                    child_id: id.clone(),
                    child_name,
                    reason,
                    action,
                },
            );
        }
    }

    /// Awaits the child's fate cell under its `Shutdown` policy's escalation
    /// ladder: `Kill` waits out the grace then aborts, `Timeout` escalates to
    /// Kill at expiry, `Infinity` waits unbounded. Returns the observed real
    /// fate (populated only after every one of the child's own drops,
    /// registry entry included, have already run - the truth-contract
    /// guarantee every caller of this relies on).
    async fn await_fate_under_shutdown(
        id: &ActorId,
        fate_rx: &mut watch::Receiver<Option<ChildFate>>,
        shutdown: Shutdown,
        stop_lane: &StopLane,
        abort: AbortHandle,
    ) -> Result<ChildFate, SupervisionError> {
        match shutdown {
            Shutdown::Kill => Self::await_fate_post_kill(id, fate_rx, abort).await,
            Shutdown::Timeout(after) => {
                let acked = time::timeout(after, fate_rx.wait_for(|f| f.is_some()))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .and_then(|f| f.clone());
                match acked {
                    Some(fate) => Ok(fate),
                    None => {
                        // Escalate: OTP exit(Child, kill) after the shutdown timeout.
                        stop_lane.raise(StopReason::Kill);
                        Self::await_fate_post_kill(id, fate_rx, abort).await
                    }
                }
            }
            Shutdown::Infinity => {
                let _ = fate_rx.wait_for(|f| f.is_some()).await;
                Ok(fate_rx.borrow().clone().expect("wait_for guarantees Some"))
            }
        }
    }

    /// Post-Kill wait: grace for the cooperative Kill, then the abort()
    /// backstop, then one final bounded wait. Only a non-yielding handler can
    /// survive all three (tokio aborts at yield points only) - surfaced as a
    /// typed error instead of hanging the supervisor.
    async fn await_fate_post_kill(
        id: &ActorId,
        fate_rx: &mut watch::Receiver<Option<ChildFate>>,
        abort: AbortHandle,
    ) -> Result<ChildFate, SupervisionError> {
        if time::timeout(KILL_GRACE, fate_rx.wait_for(|f| f.is_some()))
            .await
            .is_ok()
        {
            return Ok(fate_rx.borrow().clone().expect("wait_for guarantees Some"));
        }
        abort.abort();
        if time::timeout(KILL_GRACE, fate_rx.wait_for(|f| f.is_some()))
            .await
            .is_ok()
        {
            Ok(fate_rx.borrow().clone().expect("wait_for guarantees Some"))
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
        self.spawn_forwarder(ForwarderKind::Timer(id), fut);
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
        self.spawn_forwarder(ForwarderKind::Stream(id), fut);
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
        let when = runtime::saturating_deadline(Instant::now(), delay);
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
        self.ctx.spawn_forwarder(ForwarderKind::Timer(id), fut);
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

/// Identifies which registration a forwarder task belongs to, so its
/// completion (however it completes) can be traced back to the right
/// `timers`/`streams` entry.
enum ForwarderKind {
    Timer(RecurringId),
    Stream(StreamId),
}

type MessageFactory<A> = dyn FnMut() -> <A as Actor>::Message + Send + 'static;

async fn recurring_loop<A: Actor>(
    handle: ActorHandle<A>,
    mut factory: Box<MessageFactory<A>>,
    interval: Duration,
    miss_policy: MissPolicy,
    token: CancellationToken,
) {
    let mut next = runtime::saturating_deadline(Instant::now(), interval);
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
            *next = runtime::saturating_deadline(*next, interval);
            while *next <= now {
                *next = runtime::saturating_deadline(*next, interval);
            }
        }
        MissPolicy::Delay => {
            *next = runtime::saturating_deadline(now, interval);
        }
        MissPolicy::CatchUp => {
            *next = runtime::saturating_deadline(*next, interval);
            while *next <= now {
                if token.is_cancelled() {
                    return;
                }
                let msg = (factory.as_mut())();
                if handle.try_notify(msg).is_err() {
                    break;
                }
                *next = runtime::saturating_deadline(*next, interval);
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
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (system_tx, _system_rx) = tokio::sync::mpsc::channel(1);
        let (status_tx, status_rx) = watch::channel(ActorStatus::Running);
        let (stop_lane, _stop_rx) = StopLane::new();
        let id = ActorId::from("dummy");
        let handle = ActorHandle::new(
            id.clone(),
            tx,
            system_tx,
            stop_lane,
            1,
            status_rx,
            "dummy-system".into(),
        );
        ActorContext::new(
            id,
            handle,
            Handle::current(),
            status_tx,
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
