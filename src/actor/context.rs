//! Actor execution context providing timers, streams, and supervision hooks.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use futures_core::Stream;
use tokio_stream::StreamExt;

use tokio::task::{JoinError, JoinSet};

use crate::actor::handle::ActorHandle;
use crate::actor::panic::{catch_sync, payload_into_string, PanicPayload};
use crate::actor::subtree::{SubtreeHold, SubtreeLink};
use crate::actor::supervision::{
    await_cell, await_predecessor, await_subtree, cancelled_before_factory, manual_commit, rules,
    run_attempt, slot_pair_for, Attempt, CellOutcome, ChildLifecycle, ChildLinkGuard, ChildSpec,
    ChildState, CommitEffect, DeathOutcome, ManualOp, ManualReply, ManualStop, Origin, OwnStop,
    Predecessor, RestartFn, RestartOutcome, SlotIo, SupervisionState, TaskPlane, Verdicts,
    DEFAULT_KILL_GRACE,
};
use crate::actor::{runtime, Actor};
use crate::error::{ActorError, SpawnError, StreamError, SupervisionError, TimerError};
use crate::system::ActorSystem;
use crate::types::{
    ActorId, ActorStatus, ChildEvent, ChildInfo, MissPolicy, RecurringId, RecurringIdGenerator,
    RestartStrategy, RestartType, Shutdown, StatusValue, StopChildOutcome, StopReason, StreamEvent,
    StreamId, TurnClock,
};

/// Zero-sized marker that deliberately does not implement `Clone`, so
/// that `ActorContext` can never become `Clone` through a change to its
/// other fields.
struct NotClone;

/// The error a manual call answers with, or the outcome of a call that may
/// proceed.
fn manual_reply(reply: ManualReply, id: &ActorId) -> Result<StopChildOutcome, SupervisionError> {
    match reply {
        ManualReply::Ok => Ok(StopChildOutcome::Stopped),
        ManualReply::Absorbed => Ok(StopChildOutcome::Absorbed),
        ManualReply::ChildNotFound => Err(SupervisionError::ChildNotFound(id.clone())),
        ManualReply::ChildRestarting => Err(SupervisionError::ChildRestarting(id.clone())),
        ManualReply::ChildRunning => Err(SupervisionError::ChildRunning(id.clone())),
    }
}

/// Actor execution context providing timers, streams, supervision, and runtime hooks.
///
/// # Compile-time contracts
///
/// `ActorContext` never implements `Clone`: there is exactly one context per
/// actor, lent to each callback for the duration of that call.
/// [`self_handle`](Self::self_handle) is the supported way to keep a
/// reference to the actor once the call returns.
///
/// ```compile_fail
/// // error[E0277]: the trait bound `ActorContext<MyActor>: Clone` is not satisfied
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// # #[derive(Default)]
/// # struct MyActor;
/// #
/// # impl Actor for MyActor {
/// #     type Message = ();
/// #     type Response = ();
/// #
/// #     async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
/// #         Ok(())
/// #     }
/// # }
/// #
/// fn assert_clone<T: Clone>() {}
/// assert_clone::<ActorContext<MyActor>>();
/// ```
///
/// A bound `ActorContext` does satisfy: it is `Send`, so it can be polled
/// across await points on any worker thread the runtime chooses.
///
/// ```rust
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// # #[derive(Default)]
/// # struct MyActor;
/// #
/// # impl Actor for MyActor {
/// #     type Message = ();
/// #     type Response = ();
/// #
/// #     async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
/// #         Ok(())
/// #     }
/// # }
/// #
/// fn assert_send<T: Send>() {}
/// assert_send::<ActorContext<MyActor>>();
/// ```
///
/// The reference `handle` receives borrows this context only for the
/// duration of that call, so it cannot be moved into a detached task.
/// [`self_handle`](Self::self_handle) is the supported way to reach the
/// actor from a task that outlives the call.
///
/// ```compile_fail
/// // error[E0521]: borrowed data escapes outside of method
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// # #[derive(Default)]
/// # struct MyActor;
/// #
/// impl Actor for MyActor {
///     type Message = ();
///     type Response = ();
///
///     async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         tokio::spawn(async move {
///             let _escaped = ctx;
///         });
///         Ok(())
///     }
/// }
/// ```
///
/// ```rust
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// # #[derive(Default)]
/// # struct MyActor;
/// #
/// impl Actor for MyActor {
///     type Message = ();
///     type Response = ();
///
///     async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         let me = ctx.self_handle();
///         tokio::spawn(async move {
///             let _ = me;
///         });
///         Ok(())
///     }
/// }
/// ```
pub struct ActorContext<A: Actor> {
    actor_id: ActorId,
    self_handle: ActorHandle<A>,
    runtime: Handle,
    timers: HashMap<RecurringId, TimerRegistration>,
    streams: HashMap<StreamId, StreamRegistration>,
    id_gen: RecurringIdGenerator,
    #[allow(dead_code)]
    _not_clone: NotClone,
    /// Every timer and stream forwarder task, owned here so none of them can
    /// outlive this context. A finished task (a fired one-shot, a stream that
    /// ran out of items, a cancelled loop noticing its token) is reaped from
    /// here the moment the run loop observes its completion, which is also
    /// when its entry in `timers`/`streams` is removed - so a finished
    /// forwarder never lingers in either count.
    forwarders: JoinSet<ForwarderEnd>,
    /// Maps a forwarder task's tokio-assigned id back to the registration it
    /// belongs to, so a completion - however it completes - can find and
    /// remove the right entry from `timers`/`streams` even when a panic
    /// leaves nothing but a [`JoinError`] to identify it by.
    forwarder_kinds: HashMap<tokio::task::Id, ForwarderKind>,
    /// The most recent forwarder panic, if any. A forwarder dying is never
    /// this actor dying: the message loop keeps running exactly as before,
    /// and this is simply where that panic becomes observable.
    last_forwarder_error: Option<ActorError>,
    /// The most recent panic of a task the crate runs to supervise this
    /// actor's children. Such a panic never stops this actor.
    last_supervision_task_error: Option<ActorError>,
    last_error: Option<ActorError>,
    /// Runtime status plane (`process_info`-style, see the `handle` module
    /// docs): the status and the final reason. Every write goes through
    /// `send_if_modified`, which succeeds even when every receiver has been
    /// dropped (a plain `send` would error at zero receivers) and leaves a
    /// reason already written as it is.
    status_tx: watch::Sender<StatusValue>,
    /// Set while this actor's task is polled: tells this task's own unwind
    /// from an abort or an unrelated unwind when the context drops.
    task_marker: runtime::TaskMarker,
    // Supervision fields
    system: Option<Arc<ActorSystem>>,
    name: Option<String>,
    supervision: Option<SupervisionState>,
    /// The actor's turn clock, bracketing the callbacks the run loop runs
    /// through the context.
    turn_clock: TurnClock,
    /// The actor's hold on its subtree token: the actors it spawns attach to
    /// it, and it is released only when the actor has ended.
    subtree: Arc<SubtreeHold>,
}

impl<A: Actor> std::fmt::Debug for ActorContext<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorContext")
            .field("actor_id", &self.actor_id)
            .field("name", &self.name)
            .field("status", &self.status())
            .field("timer_count", &self.timers.len())
            .field("stream_count", &self.streams.len())
            .finish_non_exhaustive()
    }
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
        status_tx: watch::Sender<StatusValue>,
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
            _not_clone: NotClone,
            forwarders: JoinSet::new(),
            forwarder_kinds: HashMap::new(),
            last_forwarder_error: None,
            last_supervision_task_error: None,
            last_error: None,
            status_tx,
            system,
            name,
            supervision,
            turn_clock: TurnClock::default(),
            subtree: SubtreeHold::new(None),
            task_marker: runtime::TaskMarker::default(),
        }
    }

    /// The marker of this actor's task's own poll (see
    /// [`runtime::TaskMarker`]).
    pub(crate) fn task_marker(&self) -> runtime::TaskMarker {
        self.task_marker.clone()
    }

    /// Gives the context the actor's hold on its subtree token.
    pub(crate) fn set_subtree(&mut self, subtree: Arc<SubtreeHold>) {
        self.subtree = subtree;
    }

    /// Gives the context the actor's turn clock.
    pub(crate) fn set_turn_clock(&mut self, clock: TurnClock) {
        self.turn_clock = clock;
    }

    /// A handle to the actor's turn clock.
    pub(crate) fn turn_clock(&self) -> TurnClock {
        self.turn_clock.clone()
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

    /// Returns the most recent panic of a task the crate runs to supervise
    /// this actor's children (the task that stops a child or restarts it), if
    /// any. Such a panic never stops this actor and is never recorded by
    /// [`last_error`](Self::last_error); the child's own end is still
    /// reported through `on_child_stopped` with the reason it ended with.
    pub fn last_supervision_task_error(&self) -> Option<&ActorError> {
        self.last_supervision_task_error.as_ref()
    }

    /// Accounts for a death-plane task that failed instead of completing,
    /// and records its panic, if it panicked. Returns the id of the
    /// replacement task when the incarnation it follows had ended already.
    pub(crate) fn lost_death_task(&mut self, err: JoinError) -> Option<tokio::task::Id> {
        let (panic, replacement) = match self.supervision.as_mut() {
            Some(sup) => sup.lost_death_task(err),
            None => (None, None),
        };
        if let Some(panic) = panic {
            self.last_supervision_task_error = Some(panic);
        }
        replacement
    }

    /// Accounts for a restart-plane task that failed instead of completing,
    /// and records its panic, if it panicked. Returns the outcome to route
    /// when the attempt failed before it spawned an incarnation, and the id
    /// of the replacement task when the incarnation it follows had ended
    /// already.
    pub(crate) fn lost_restart_task(
        &mut self,
        err: JoinError,
    ) -> (Option<RestartOutcome>, Option<tokio::task::Id>) {
        let (outcome, panic, replacement) = match self.supervision.as_mut() {
            Some(sup) => sup.lost_restart_task(err),
            None => (None, None, None),
        };
        if let Some(panic) = panic {
            self.last_supervision_task_error = Some(panic);
        }
        (outcome, replacement)
    }

    /// Returns the current lifecycle status of the actor.
    pub fn status(&self) -> ActorStatus {
        self.status_tx.borrow().status
    }

    pub(crate) fn set_status(&mut self, status: ActorStatus) {
        // `send_if_modified`, not `send`: this must succeed even if every
        // `ActorHandle` (and therefore every watch receiver) has already been
        // dropped, which a plain `send` would treat as an error. The plane's
        // reason is never touched here.
        self.status_tx.send_if_modified(|value| {
            value.status = status;
            true
        });
    }

    // - Supervision --------------------------------------------------------

    pub(crate) fn supervision_mut(&mut self) -> Option<&mut SupervisionState> {
        self.supervision.as_mut()
    }

    pub(crate) fn supervision_ref(&self) -> Option<&SupervisionState> {
        self.supervision.as_ref()
    }

    /// Whether a final delivery of `on_child_stopped` is running: the
    /// supervisor is stopping, and its supervision calls are refused.
    fn supervisor_stopping(&self) -> bool {
        self.supervision
            .as_ref()
            .is_some_and(|sup| sup.final_delivery)
    }

    /// Takes the supervision state out of the context, leaving `None`
    /// behind. Used only by the run loop's Kill cascade-inversion path:
    /// dropping the returned value directly aborts every live child's
    /// runner task without this actor ever awaiting any of them (see the
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
        Option<(
            &mut JoinSet<RestartOutcome>,
            &mut JoinSet<DeathOutcome>,
            &Arc<Verdicts>,
        )>,
        &mut JoinSet<ForwarderEnd>,
    ) {
        let planes = self
            .supervision
            .as_mut()
            .map(|sup| (&mut sup.restart_set, &mut sup.death_set, &sup.verdicts));
        (planes, &mut self.forwarders)
    }

    /// Reaps one forwarder task's completion: removes its registration from
    /// `timers`/`streams` (a fired one-shot or a finished stream leaves zero
    /// registrations behind) and, if it ended in a panic, records it as
    /// [`last_forwarder_error`](Self::last_forwarder_error) and returns its
    /// payload. With the `tracing` feature every payload it records is
    /// logged once, naming the actor, whoever reaps it (the message loop
    /// during the run, or the end). A forwarder that panicked both before and
    /// after it saw the mailbox closed keeps the later panic; the earlier one
    /// is logged as discarded. A completion whose registration was already
    /// removed (an explicit `cancel_timer`/`cancel_stream` beat it here) is a
    /// harmless no-op.
    pub(crate) fn reap_forwarder(
        &mut self,
        outcome: Result<(tokio::task::Id, ForwarderEnd), JoinError>,
    ) -> Option<String> {
        let (task_id, panic) = match outcome {
            Ok((id, ForwarderEnd::Done)) => (id, None),
            Ok((id, ForwarderEnd::Panicked(payload))) => (id, Some(payload)),
            // Read as the run's wherever it is reaped here; the end
            // classifies an end drop itself before reaping.
            Ok((id, ForwarderEnd::Released { run, end })) => match (end, run) {
                (Some(end), Some(run)) => {
                    #[cfg(feature = "tracing")]
                    caught_log!(warn,
                        actor_id = %self.actor_id(),
                        panic = %run,
                        "a timer or stream of the actor panicked during the run and again at the close; the earlier panic is discarded"
                    );
                    #[cfg(not(feature = "tracing"))]
                    let _ = run;
                    (id, Some(end))
                }
                (end, run) => (id, end.or(run)),
            },
            Err(err) => {
                let id = err.id();
                let panic = if err.is_panic() {
                    match err.try_into_panic() {
                        Ok(payload) => Some(payload_into_string(payload)),
                        Err(_) => Some("panic payload unavailable".to_string()),
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
        if let Some(payload) = &panic {
            #[cfg(feature = "tracing")]
            caught_log!(warn,
                actor_id = %self.actor_id(),
                panic = %payload,
                "a timer or stream of the actor panicked"
            );
            self.last_forwarder_error = Some(ActorError::Panic(payload.clone()));
        }
        panic
    }

    /// Whether the timer or stream a forwarder belongs to is still
    /// registered, that is, neither cancelled nor reaped.
    fn is_registered(&self, kind: &ForwarderKind) -> bool {
        match kind {
            ForwarderKind::Timer(id) => self.timers.contains_key(id),
            ForwarderKind::Stream(id) => self.streams.contains_key(id),
        }
    }

    /// Spawns a forwarder task onto the forwarder plane, remembering which
    /// registration it belongs to so a later completion - success, or a
    /// caught-by-tokio panic - can find its way back to the right entry in
    /// `timers`/`streams`.
    fn spawn_forwarder<F>(&mut self, kind: ForwarderKind, fut: F, slot: PanicSlot)
    where
        F: std::future::Future<Output = Released> + Send + 'static,
    {
        let forwarder = Forwarder {
            future: Some(Box::pin(fut)),
            slot,
            owner: self.actor_id().clone(),
            _status: self.status_tx.clone(),
            _subtree: self.subtree.clone(),
        };
        let abort = self.forwarders.spawn_on(forwarder, &self.runtime);
        self.forwarder_kinds.insert(abort.id(), kind);
    }

    /// Ends every timer and stream forwarder and waits until each one's
    /// future, with the stream, message or factory it owns, is dropped.
    /// A forwarder whose timer or stream was cancelled during the run
    /// (`on_stopped` included) belongs to the run, and so does a panic
    /// raised by any forwarder's poll: an aborted forwarder is never polled
    /// again, so every poll began before the end. Both are reaped as the
    /// message loop reaps them (the last forwarder error, never the actor's
    /// end), so the end reads the same whichever task runs first. From the
    /// mailbox's close on, every forwarder releases what it owns when it
    /// next runs, in its own poll, and this abort drops what one that has
    /// not run still owns. Either way a drop panic is classified by
    /// registration: part of the end for a timer or stream still registered
    /// when the end began, the run's for one cancelled during the run. The
    /// first end panic is returned; a later one is logged, naming the actor,
    /// and discarded. A panic of the run reaped here is logged by
    /// `reap_forwarder`, as one the message loop reaps is.
    pub(crate) async fn end_forwarders(&mut self) -> Option<String> {
        let registered: HashSet<tokio::task::Id> = self
            .forwarder_kinds
            .iter()
            .filter(|(_, kind)| self.is_registered(kind))
            .map(|(id, _)| *id)
            .collect();
        self.forwarders.abort_all();
        let owner = self.actor_id().clone();
        let mut first = None;
        while let Some(joined) = self.forwarders.join_next_with_id().await {
            match joined {
                Err(err) if err.is_panic() && registered.contains(&err.id()) => {
                    let payload = match err.try_into_panic() {
                        Ok(payload) => payload_into_string(payload),
                        Err(_) => "panic payload unavailable".to_string(),
                    };
                    runtime::keep_first_of(&mut first, Some(payload), &owner);
                }
                // A drop the forwarder made once it had seen the mailbox
                // closed, for a timer or stream still registered: the end's.
                // Its drop made before the close stays the run's.
                Ok((
                    id,
                    ForwarderEnd::Released {
                        run,
                        end: Some(end),
                    },
                )) if registered.contains(&id) => {
                    runtime::keep_first_of(&mut first, Some(end), &owner);
                    self.reap_forwarder(Ok((id, ForwarderEnd::Released { run, end: None })));
                }
                other => {
                    self.reap_forwarder(other);
                }
            }
        }
        first
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
    /// runner and restart budget already give the supervisor full,
    /// asynchronous visibility into a child's init failure, and synchronously
    /// awaiting that ack from inside the parent's own callback would invite
    /// the same self-call deadlock documented on
    /// [`ActorHandle::send`](crate::actor::handle::ActorHandle::send).
    ///
    /// The spawn fails with [`SpawnError::SupervisorStopping`] when called
    /// inside [`on_child_stopped`](crate::actor::Actor::on_child_stopped)
    /// while the supervisor itself is stopping.
    ///
    /// A restart of a child begins only after the child's previous incarnation
    /// and every actor it spawned have ended, waiting without blocking the
    /// supervisor. A spawn under the id of a child that was deleted or removed
    /// waits likewise, inside the calling handler, until every actor the
    /// previous child spawned has ended, and fails with
    /// [`SpawnError::ChildUnresponsive`] if one of them never yields. Each wait
    /// is bounded by the longest chain of stop budgets in the subtree (a
    /// worker's kill grace, a supervisor child's `Shutdown` window plus its
    /// kill grace), each followed by a short settle time, plus one more settle
    /// time, and unbounded only while a supervisor child under
    /// [`Shutdown::Infinity`] runs in it or a budget is too large to represent.
    /// Past that bound, the wait gives no verdict on an actor before the abort
    /// the system owes it has had the settle time to end it: an abort that is
    /// late holds the wait, for at most the bound once more plus one more
    /// settle time.
    ///
    /// A child spawned while a group restart is pending starts at once; it is
    /// not part of that restart, though a retry after a member's restart fails
    /// includes the new child: a `OneForAll` retry stops every child and
    /// restarts each one that is not `Temporary` and was not released by
    /// `terminate_child`, and a `RestForOne` retry does the same from the
    /// child that failed onward.
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
    /// - [`SpawnError::ChildUnresponsive`]
    ///   if an actor of a deleted or removed child under this id, or one that
    ///   child spawned, did not end within the bound of that subtree.
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
            kill_grace: DEFAULT_KILL_GRACE,
            _child: std::marker::PhantomData,
        }
    }

    /// The wait of a spawn under the id of a removed child: until every actor
    /// the removed child's last incarnation spawned has ended. The refusals
    /// that need no wait are answered first and in their own order, so a
    /// refused call never reads, prunes or waits on the removed children; a
    /// spawn under an id nobody removed completes without waiting.
    async fn await_retired_subtree(&mut self, name: Option<&str>) -> Result<(), SpawnError> {
        let Some(name) = name else {
            return Ok(());
        };
        let id = ActorId::from(name);
        let Some(sup) = self.supervision.as_mut() else {
            return Err(SpawnError::NotASupervisor);
        };
        if sup.final_delivery {
            return Err(SpawnError::SupervisorStopping);
        }
        if sup.registry.get(&id).is_some() {
            return Err(SpawnError::DuplicateChild(id));
        }
        let Some(subtree) = sup.retired_subtree(&id) else {
            return Ok(());
        };
        match await_subtree(&subtree).await {
            rules::WaitAction::Escalate => Err(SpawnError::ChildUnresponsive(id)),
            _ => Ok(()),
        }
    }

    /// Internal spawn logic used by [`ChildSpawnBuilder`].
    #[allow(clippy::too_many_arguments)]
    fn spawn_child_internal<F, C>(
        &mut self,
        factory: F,
        name: Option<String>,
        restart_type: RestartType,
        shutdown: Shutdown,
        config: Option<runtime::ActorConfig>,
        start_timeout: Option<Duration>,
        kill_grace: Duration,
    ) -> Result<ActorHandle<C>, SpawnError>
    where
        F: Fn() -> C + Send + Sync + 'static,
        C: Actor,
    {
        if self.supervision.is_none() {
            return Err(SpawnError::NotASupervisor);
        }
        if self.supervisor_stopping() {
            return Err(SpawnError::SupervisorStopping);
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
        // What the child adds to the budget of the subtree it hangs from,
        // and whether its orderly stop has no bound: both are properties of
        // the spec, so every incarnation of the child attaches the same way.
        let subtree_own = rules::own_fin(is_supervisor, shutdown, kill_grace);
        // What bounds the child's stop once its supervisor is gone, and what
        // its link guard does to it then; both are properties of the spec
        // too, so every incarnation is stopped the same way.
        let own_stop = OwnStop {
            is_supervisor,
            shutdown,
            kill_grace,
        };
        let inherited = rules::inherited_stop(is_supervisor, shutdown, kill_grace);
        let subtree_infinity = is_supervisor && shutdown == Shutdown::Infinity;
        let subtree_parent = self.subtree.token();

        let (child_handle, join_handle, child_subtree) = runtime::spawn_actor(
            child_id.clone(),
            actor,
            child_config.clone(),
            name.clone(),
            self.system.clone(),
            None,
            false,
            kill_grace,
            Some(SubtreeLink {
                parent: subtree_parent.clone(),
                own: subtree_own,
                infinity: subtree_infinity,
            }),
        )?;

        // The parent spawns and owns the runner of the initial incarnation,
        // together with its link guard - constructed here, in the parent's
        // own synchronous code, before the runner task is spawned, so an
        // abort landing before the runner is ever polled still fires it.
        // The abort handle is taken first - the Kill escalation backstop.
        let abort = join_handle.abort_handle();
        let resolved_system = self.resolved_system();
        let guard_runtime = self.runtime_handle().clone();
        let reaper = resolved_system.reaper_handle(&guard_runtime);
        // Cloned before the initial guard consumes `reaper` (and the system)
        // by value: the restart closure below needs the same per-system
        // reaper feed and system handle for every future incarnation's own
        // link guard.
        let restart_reaper = reaper.clone();
        let restart_system = resolved_system.clone();
        let guard = ChildLinkGuard::new(
            child_handle.stop_lane(),
            abort.clone(),
            reaper,
            inherited,
            resolved_system,
            guard_runtime.clone(),
        );
        let verdicts = self
            .supervision
            .as_ref()
            .expect("supervision presence checked above")
            .verdicts
            .clone();
        // Every registration - this initial spawn included - draws its
        // incarnation token from the system's single monotonic counter, so
        // no two incarnations of any child, under any name, ever collide
        // (see `ActorSystem::next_incarnation`).
        let incarnation = self.resolved_system().next_incarnation();
        let (ctl, io) = slot_pair_for(verdicts, Some((child_id.clone(), incarnation)));
        io.publish(
            child_handle.stop_lane(),
            child_handle.status_rx(),
            own_stop,
            Some(child_subtree.clone()),
        );

        let system_clone = self.system.clone();

        let sup = self
            .supervision
            .as_mut()
            .expect("supervision presence checked above");
        let task = runtime::spawn_runner(
            &mut sup.death_set,
            child_id.clone(),
            incarnation,
            join_handle,
            child_handle.stop_lane(),
            io,
            guard,
            kill_grace,
        );
        sup.task_owner
            .insert(task.id(), (child_id.clone(), incarnation, TaskPlane::Death));

        let predecessor = ctl.predecessor(child_subtree);
        let child_state = ChildState {
            id: child_id.clone(),
            name: name.clone(),
            spec: ChildSpec {
                restart_type,
                shutdown,
                start_timeout,
                kill_grace,
                is_supervisor,
            },
            ctl,
            lifecycle: ChildLifecycle::Running(incarnation),
            origin: Origin::Initial,
            predecessor: Some(predecessor),
        };

        // The supervisor's id, which an attempt's log lines name.
        let restart_supervisor = self.actor_id().clone();
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
        // The kill grace every fresh incarnation is spawned, guarded and
        // laddered with, exactly like the initial spawn above.
        let restart_kill_grace = kill_grace;
        let restart_runtime = guard_runtime;

        sup.restart_fns.insert(
            child_id,
            Box::new(
                move |seq, mut io: SlotIo, predecessor: Option<Predecessor>| {
                    // The attempt holds the factory weakly: the spec's box
                    // holds its strong reference, so its drop runs where the
                    // spec is removed, unless the spec goes while the attempt
                    // builds; see `build_for_attempt`.
                    let factory = Arc::downgrade(&factory);
                    let supervisor = restart_supervisor.clone();
                    let system = system_clone.clone();
                    let child_id = restart_id.clone();
                    let name = restart_name.clone();
                    let config = restart_config.clone();
                    let start_timeout = restart_start_timeout;
                    let kill_grace = restart_kill_grace;
                    let runtime = restart_runtime.clone();
                    let reaper = restart_reaper.clone();
                    let guard_system = restart_system.clone();
                    let subtree_parent = subtree_parent.clone();
                    Box::pin(async move {
                        // The previous incarnation's whole subtree ends before
                        // anything is built: the wait runs here, in the restart
                        // plane, never in the supervisor's loop. A stop order or
                        // the group's cancel ends it, and a subtree that outlives
                        // its bound is reported, never started beside.
                        match await_predecessor(predecessor.as_ref(), &mut io).await {
                            rules::WaitAction::Cancel => {
                                return RestartOutcome::Cancelled {
                                    child_id,
                                    incarnation: seq,
                                    drop_panic: None,
                                };
                            }
                            rules::WaitAction::Escalate => {
                                return RestartOutcome::PredecessorUnresponsive {
                                    child_id,
                                    incarnation: seq,
                                };
                            }
                            rules::WaitAction::Proceed | rules::WaitAction::Keep => {}
                        }

                        // A stop order or the group's cancel written before the
                        // factory runs ends the attempt: nothing exists to stop,
                        // and nothing is built.
                        if cancelled_before_factory(&mut io) {
                            return RestartOutcome::Cancelled {
                                child_id,
                                incarnation: seq,
                                drop_panic: None,
                            };
                        }

                        let actor =
                            match build_for_attempt(&factory, &io, &supervisor, &child_id, seq) {
                                Ok(actor) => actor,
                                Err(outcome) => return *outcome,
                            };

                        let (ack_tx, ack_rx) = oneshot::channel();
                        let (handle, join, subtree) = match runtime::spawn_actor(
                            child_id.clone(),
                            actor,
                            config,
                            name,
                            system,
                            Some(ack_tx),
                            false,
                            kill_grace,
                            Some(SubtreeLink {
                                parent: subtree_parent,
                                own: subtree_own,
                                infinity: subtree_infinity,
                            }),
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
                        io.publish(
                            new_stop_lane.clone(),
                            handle.status_rx(),
                            own_stop,
                            Some(subtree),
                        );
                        let guard = ChildLinkGuard::new(
                            new_stop_lane.clone(),
                            abort.clone(),
                            reaper,
                            inherited,
                            guard_system,
                            runtime,
                        );

                        // From here the attempt's executor owns the incarnation:
                        // it awaits the same init contract the top-level spawn
                        // awaits (pre_start + on_started), bounded by the child's
                        // start_timeout if one was set (None waits indefinitely,
                        // OTP start_link parity), and honors a stop order at
                        // every moment of that wait.
                        run_attempt(Attempt {
                            child_id,
                            incarnation: seq,
                            join,
                            lane: new_stop_lane,
                            guard,
                            ack: ack_rx,
                            start_timeout,
                            grace: kill_grace,
                            io,
                        })
                        .await
                    })
                },
            ),
        );

        Ok(child_handle)
    }

    /// Returns introspection info for all supervised children.
    ///
    /// A child is alive while it has a started incarnation whose stop, if
    /// any, has not completed, and its restart is pending while an attempt is
    /// in flight, while its death awaits its restart decision, and while a
    /// pending group restart will restart it.
    pub fn children(&self) -> Vec<ChildInfo> {
        self.supervision
            .as_ref()
            .map_or(Vec::new(), |s| s.children_info())
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
    /// it. When the outcome is [`StopChildOutcome::Stopped`] the call returns
    /// once the child's fate cell is populated - which tokio guarantees only
    /// happens after every one of the child's own drops, its registry entry
    /// included, have already run - so by the time this resolves the child is
    /// fully gone and its name is free (`Ok` if it was already stopped and
    /// its death is decided, see the errors below). An
    /// [`StopChildOutcome::Absorbed`] call returns at once: the pending group
    /// restart still owns the child, which is alive or about to be restarted
    /// by that group, and its name is not free.
    ///
    /// A supervisor-child's stop recursively waits for that subtree's own
    /// teardown: the worst case is the sum of every level's [`Shutdown`]
    /// budget below it, and a [`Shutdown::Infinity`] descendant anywhere in
    /// that subtree makes the wait unbounded (accepted; escape via the
    /// escalation ladder on every non-`Infinity` policy in the chain). That
    /// wait covers the child's own orderly teardown of its children; it does
    /// not wait for actors of the subtree that outlive a `Kill` or an abort,
    /// which [`terminate_child`](Self::terminate_child) also waits for.
    ///
    /// A Permanent child's restart begins only after the child's previous
    /// incarnation and every actor it spawned have ended. That wait is bounded
    /// by the longest chain of stop budgets in the subtree (a worker's kill
    /// grace, a supervisor child's `Shutdown` window plus its kill grace), each
    /// followed by a short settle time, plus one more settle time, and
    /// unbounded only while a supervisor child under [`Shutdown::Infinity`]
    /// runs in it or a budget is too large to represent. Past that bound, the
    /// wait gives no verdict on an actor before the abort the system owes it
    /// has had the settle time to end it: an abort that is late holds the wait,
    /// for at most the bound once more plus one more settle time. If the
    /// subtree does not end within that wait, no new incarnation starts and
    /// the supervisor stops with [`SupervisionError::ChildUnresponsive`], so
    /// that its own supervisor decides.
    ///
    /// To stop a child WITHOUT any restart, use
    /// [`terminate_child`](Self::terminate_child).
    ///
    /// # Outcome
    /// [`StopChildOutcome::Stopped`] when the call completed a stop, and
    /// [`StopChildOutcome::Absorbed`] when the child is a `Permanent` member
    /// of a group restart that is still pending and has still to restart it:
    /// the call then changed nothing, the group stops the child, if it has
    /// not already, and restarts it. The members a pending group restart
    /// covers follow their restart type: a `Permanent` member is absorbed
    /// while the group has yet to restart it, also when it restarted the
    /// child once already and is restarting the whole group again, and is
    /// refused once the group restarted it for good or released it; a
    /// `Transient` or `Temporary` member, and such a child started while the
    /// group restart is pending, is stopped as
    /// [`terminate_child`](Self::terminate_child) stops it: it stays down
    /// through that group restart, a `Transient` child keeping its
    /// specification and a `Temporary` child removed.
    ///
    /// Cancel safety: the escalation belongs to the child's own runner.
    /// Abandoning this future at any point, for example with
    /// `tokio::time::timeout`, still stops the child under its `Shutdown`
    /// policy. Calling it again while that stop is in progress waits for the
    /// same stop, or returns at once where a pending group restart covers the
    /// child (`StopChildOutcome::Absorbed` for a `Permanent` member that
    /// restart has still to restart, `ChildRestarting` once it has restarted it
    /// for good or released it).
    ///
    /// To force a restart that charges the restart budget, kill the child:
    /// [`ActorSystem::kill_by_id`](crate::system::ActorSystem::kill_by_id),
    /// or [`stop_within`](crate::actor::handle::ActorHandle::stop_within)
    /// with [`StopReason::Kill`] on its handle. Its supervisor restarts it per
    /// its restart type, as Erlang/OTP does for `exit(Child, kill)`.
    ///
    /// # Errors
    /// - [`SupervisionError::NotASupervisor`] / [`SupervisionError::ChildNotFound`]
    /// - [`SupervisionError::ChildRestarting`] while the child has a restart
    ///   attempt in flight; while it has died and its death awaits its
    ///   restart decision (a crash a bounce met, or a death the supervisor
    ///   has not handled yet): the decision is made once, by the supervisor,
    ///   and a bounce neither makes it nor replaces it (OTP's
    ///   `terminate_child` flushes the exit and answers `ok` here; so does
    ///   [`terminate_child`](Self::terminate_child), which dismisses the
    ///   decision); for a `Permanent` child that a pending group restart
    ///   already restarted or released with
    ///   [`terminate_child`](Self::terminate_child), until that restart
    ///   completes; and for the member of a pending group restart whose own
    ///   restart attempt is in flight.
    /// - [`SupervisionError::ChildUnresponsive`] if the child survives even
    ///   the abort backstop (a non-yielding handler; see the crate docs).
    /// - [`SupervisionError::SupervisorStopping`] when called inside
    ///   [`on_child_stopped`](crate::actor::Actor::on_child_stopped) while the
    ///   supervisor itself is stopping.
    pub async fn stop_child(
        &mut self,
        child: impl Into<ActorId>,
    ) -> Result<StopChildOutcome, SupervisionError> {
        self.manual_stop_child(child.into(), ManualStop::Bounce)
            .await
    }

    /// Stops a supervised child WITHOUT restarting it - the OTP
    /// `terminate_child/2` equivalent. The child spec is kept (visible in
    /// [`children`](Self::children) with `is_alive == false`) so the child can
    /// later be revived with [`restart_child`](Self::restart_child) or removed
    /// with [`delete_child`](Self::delete_child); a Temporary child's spec is
    /// deleted immediately (OTP parity), as is a SimpleOneForOne child's. A
    /// crash that lands while this call is stopping the child (the child dies
    /// of `Failure` at roughly the same moment) is absorbed into this call's
    /// own completion rather than being reported as an independent failure
    /// (OTP unlink-and-flush parity).
    ///
    /// Succeeds for any known child: one already down, one whose restart
    /// decision is still owed (a crash a bounce met, or a death the
    /// supervisor has not handled yet) included. It dismisses an owed
    /// decision, so the child is not restarted and its death is reported once
    /// to [`on_child_stopped`](crate::actor::Actor::on_child_stopped) as
    /// [`SupervisionAction::Removed`](crate::SupervisionAction::Removed),
    /// cancels a pending restart, and returns once the child and every actor
    /// it spawned have ended. A second `terminate_child` of a child whose
    /// stop is already in progress waits for that stop, and one that finds a
    /// `stop_child` in progress turns it into a terminate: the child stays
    /// down.
    ///
    /// Returns only once the child is fully gone and its registered name is
    /// free: [`delete_child`](Self::delete_child) and a same-name
    /// [`spawn_child`](Self::spawn_child) succeed afterward, even from the
    /// very same handler invocation that awaited this call.
    ///
    /// Unlike [`stop_child`](Self::stop_child), this call also waits for the
    /// child's subtree: it returns once the child and every actor it spawned
    /// have ended, a child already stopped whose actors are still ending
    /// included. The wait is bounded by the longest chain of stop budgets in
    /// the subtree (a worker's kill grace, a supervisor child's `Shutdown`
    /// window plus its kill grace), each followed by a short settle time, plus
    /// one more settle time, and unbounded only while a supervisor child under
    /// [`Shutdown::Infinity`] runs in it or a budget is too large to represent.
    /// Past that bound, the wait gives no verdict on an actor before the abort
    /// the system owes it has had the settle time to end it: an abort that is
    /// late holds the wait, for at most the bound once more plus one more
    /// settle time. If an actor never yields, the call returns
    /// [`SupervisionError::ChildUnresponsive`] and the child stays stopped; a
    /// [`spawn_child`](Self::spawn_child) under the same id then waits for
    /// that subtree again.
    ///
    /// While a group restart is pending, a `terminate_child` keeps the child
    /// down through every retry of that group restart, a child started since
    /// included. A group restart stops its members one at a time; a
    /// `terminate_child` of a member the group has not reached yet stops that
    /// member at once, beside the member the group is stopping, and returns
    /// once it ended. A member under [`Shutdown::Infinity`] whose stop is in
    /// progress and that vetoed it is asked once more by the call, an ask the
    /// group restart then does not repeat.
    ///
    /// Blocking and escalation (including the recursive subtree-teardown
    /// latency note) match [`stop_child`](Self::stop_child). A child
    /// configured with [`Shutdown::Infinity`] makes this call wait without
    /// bound: Erlang/OTP reserves `infinity` for a child that is itself a
    /// supervisor, whose own subtree can legitimately take an unbounded time
    /// to unwind. A leaf actor configured with `Infinity` is a
    /// misconfiguration this crate does not guard against. A child still
    /// running its first initialization is stopped bounded by its `Shutdown`
    /// from the call; one whose initialization waits on this supervisor
    /// deadlocks this call unless that bound ends it (under `Infinity` nothing
    /// does: the hazard `pre_stop` already has inside a handler).
    ///
    /// Cancel safety: the escalation belongs to the child's own runner.
    /// Abandoning this future at any point, for example with
    /// `tokio::time::timeout`, still stops the child under its `Shutdown`
    /// policy, and calling it again while that stop is in progress waits for
    /// the same stop. The caller may bound its own wait and drop the future
    /// safely.
    ///
    /// # Errors
    /// - [`SupervisionError::NotASupervisor`] / [`SupervisionError::ChildNotFound`]
    /// - [`SupervisionError::ChildRestarting`] only while a restart attempt
    ///   is in flight, the front of a group restart's chain included.
    /// - [`SupervisionError::ChildUnresponsive`] if the child outlasts the
    ///   abort backstop, or its subtree outlasts the stop windows (the child
    ///   is left down).
    /// - [`SupervisionError::SupervisorStopping`] inside a final delivery,
    ///   that is when called inside
    ///   [`on_child_stopped`](crate::actor::Actor::on_child_stopped) while the
    ///   supervisor itself is stopping. An owed death is dismissed, not
    ///   refused.
    pub async fn terminate_child(
        &mut self,
        child: impl Into<ActorId>,
    ) -> Result<(), SupervisionError> {
        self.manual_stop_child(child.into(), ManualStop::Terminate)
            .await
            .map(|_| ())
    }

    /// Revives a child previously stopped with
    /// [`terminate_child`](Self::terminate_child) - the OTP `restart_child/2`
    /// equivalent. Initiates the restart from the stored child spec and
    /// returns; completion is observable via a name lookup or
    /// [`children`](Self::children). The restart begins only after the child's
    /// previous incarnation and every actor it spawned have ended, waiting
    /// without blocking the supervisor. The wait is bounded by the longest
    /// chain of stop budgets in the subtree (a worker's kill grace, a
    /// supervisor child's `Shutdown` window plus its kill grace), each followed
    /// by a short settle time, plus one more settle time, and unbounded only
    /// while a supervisor child under [`Shutdown::Infinity`] runs in it or a
    /// budget is too large to represent. Past that bound, the wait gives no
    /// verdict on an actor before the abort the system owes it has had the
    /// settle time to end it: an abort that is late holds the wait, for at most
    /// the bound once more plus one more settle time. If the subtree does not
    /// end within that wait, no new incarnation starts and the supervisor
    /// stops with [`SupervisionError::ChildUnresponsive`], so that its own
    /// supervisor decides.
    ///
    /// The call returns before the child initializes. If initialization then
    /// fails, `on_child_stopped` receives [`SupervisionAction::Removed`](crate::SupervisionAction::Removed) with
    /// the initialization error, the spec is kept, no restart budget is
    /// charged and no sibling is touched - unless a pending group restart has
    /// committed the child, which then restarts it in its slot.
    ///
    /// To force a restart of a running child, kill it (see
    /// [`stop_child`](Self::stop_child)): its supervisor restarts it per its
    /// restart type and charges the restart budget.
    ///
    /// # Errors
    /// - [`SupervisionError::NotASupervisor`] / [`SupervisionError::ChildNotFound`]
    /// - [`SupervisionError::ChildRunning`] if the child's task is running.
    /// - [`SupervisionError::ChildRestarting`] if a restart attempt is in
    ///   flight; if the child's stop is in progress, whether or not its task
    ///   has ended; if its death awaits its restart decision (a child whose
    ///   task has just ended included); or if it belongs to a pending group
    ///   restart, a member released by
    ///   [`terminate_child`](Self::terminate_child) included, until that
    ///   restart completes. A running child that group restart has already
    ///   restarted is refused the same way, whatever its membership.
    /// - [`SupervisionError::SupervisorStopping`] when called inside
    ///   [`on_child_stopped`](crate::actor::Actor::on_child_stopped) while the
    ///   supervisor itself is stopping.
    pub fn restart_child(&mut self, child: impl Into<ActorId>) -> Result<(), SupervisionError> {
        if self.supervisor_stopping() {
            return Err(SupervisionError::SupervisorStopping);
        }
        let id = child.into();
        let seq = self.resolved_system().next_incarnation();
        let sup = self
            .supervision
            .as_mut()
            .ok_or(SupervisionError::NotASupervisor)?;
        manual_reply(sup.manual_answer(ManualOp::RestartChild, &id), &id)?;
        sup.initiate(&id, seq, Origin::Manual);
        Ok(())
    }

    /// Removes a stopped child's spec - the OTP `delete_child/2` equivalent.
    /// The child must not be running or restarting; use
    /// [`terminate_child`](Self::terminate_child) first.
    ///
    /// A [`spawn_child`](Self::spawn_child) under the same id waits, inside the
    /// calling handler, until every actor the deleted child spawned has ended,
    /// and fails with [`SpawnError::ChildUnresponsive`] if one of them never
    /// yields. The wait is bounded by the longest chain of stop budgets in the
    /// subtree (a worker's kill grace, a supervisor child's `Shutdown` window
    /// plus its kill grace), each followed by a short settle time, plus one
    /// more settle time, and unbounded only while a supervisor child under
    /// [`Shutdown::Infinity`] runs in it or a budget is too large to represent.
    /// Past that bound, the wait gives no verdict on an actor before the abort
    /// the system owes it has had the settle time to end it: an abort that is
    /// late holds the wait, for at most the bound once more plus one more
    /// settle time.
    ///
    /// # Errors
    /// The same set as [`restart_child`](Self::restart_child), except that a
    /// member released by [`terminate_child`](Self::terminate_child) from a
    /// pending group restart is answered by its own state: `Ok` once it is
    /// down, [`SupervisionError::ChildRestarting`] while its stop is in
    /// progress, [`SupervisionError::ChildRunning`] while it runs.
    pub fn delete_child(&mut self, child: impl Into<ActorId>) -> Result<(), SupervisionError> {
        if self.supervisor_stopping() {
            return Err(SupervisionError::SupervisorStopping);
        }
        let id = child.into();
        let sup = self
            .supervision
            .as_mut()
            .ok_or(SupervisionError::NotASupervisor)?;
        manual_reply(sup.manual_answer(ManualOp::DeleteChild, &id), &id)?;
        let factory = sup.remove_spec(&id);
        // The factory is user code: dropped as the call's last step, a panic
        // in its drop is a panic of the callback that called it.
        drop(factory);
        Ok(())
    }

    /// Shared machinery for stop_child / terminate_child.
    ///
    /// The decision of the call is `rules::manual_commit`; this executes it.
    /// The commit is synchronous: the ledger moves to `Stopping{kind}`, the
    /// first signal is raised and the stop order is written to the child's
    /// runner with ZERO awaits between them, so a caller that drops this
    /// future immediately after issuing it has still made the stop happen.
    /// From there the child's own runner escalates the stop under its
    /// `Shutdown` policy (Kill, then abort) and records the end; this call
    /// only waits on the runner's cell (populated only after every one of the
    /// child's drops, its registry entry included, have already run) and
    /// classifies the result. Dropping this future at any await changes
    /// nothing about how the child is stopped, and the run loop completes the
    /// classification when the child's death arrives. A second call on a
    /// child whose manual stop is in progress waits for that same stop.
    ///
    /// A member of an in-flight `OneForAll`/`RestForOne` group is answered by
    /// its membership: `stop_child` of a `Permanent` member is absorbed or
    /// refused, and `terminate_child` records that the group must leave the
    /// child down and, when the group awaits the child's end, waits for it. A
    /// member the group's stop queue has not reached has no stop in progress,
    /// so the call orders it itself; a member whose stop is in progress is
    /// joined, never given a second ladder, and a vetoing `Infinity` member
    /// is asked once more when nobody asked it since.
    async fn manual_stop_child(
        &mut self,
        id: ActorId,
        kind: ManualStop,
    ) -> Result<StopChildOutcome, SupervisionError> {
        let (outcome, effective) = self.manual_stop_decided(id.clone(), kind).await?;
        // A `stop_child` handled as the child's `terminate_child` waits for
        // the child's subtree too.
        if effective == ManualStop::Terminate {
            self.await_child_subtree(&id).await?;
        }
        Ok(outcome)
    }

    /// Waits for the subtree of the last incarnation of `id` to end: the
    /// registered child's, or the removed child's that a start under the id
    /// would wait for. The wait is bounded by the budget that subtree carries
    /// and gives up with [`SupervisionError::ChildUnresponsive`] when a
    /// descendant outlives it.
    async fn await_child_subtree(&mut self, id: &ActorId) -> Result<(), SupervisionError> {
        let Some(subtree) = self
            .supervision
            .as_mut()
            .and_then(|sup| sup.predecessor_of(id))
        else {
            return Ok(());
        };
        if rules::successor_wait_step(subtree.is_gone(), false, false) == rules::WaitAction::Proceed
        {
            return Ok(());
        }
        match await_subtree(&subtree).await {
            rules::WaitAction::Escalate => Err(SupervisionError::ChildUnresponsive(id.clone())),
            _ => Ok(()),
        }
    }

    /// The commit and the wait on the child's own end of
    /// [`manual_stop_child`](Self::manual_stop_child).
    async fn manual_stop_decided(
        &mut self,
        id: ActorId,
        kind: ManualStop,
    ) -> Result<(StopChildOutcome, ManualStop), SupervisionError> {
        let sup = self
            .supervision
            .as_ref()
            .ok_or(SupervisionError::NotASupervisor)?;
        if sup.final_delivery {
            return Err(SupervisionError::SupervisorStopping);
        }
        let facts = sup.manual_facts(&id);
        let policy = facts.policy;
        // The incarnation the call concerns: the one a Running or Stopping
        // child holds (the effects that need it are only decided for those).
        let incarnation = facts
            .life
            .and_then(|life| life.occupant())
            .unwrap_or_default();
        let commit = manual_commit(kind, &facts);
        let kind = rules::effective_kind(kind, &facts);
        let done = |outcome| Ok((outcome, kind));

        if let Some(to) = commit.to {
            if let Some(child) = self
                .supervision_mut()
                .and_then(|sup| sup.registry.get_mut(&id))
            {
                let _ = child.transition(to);
            }
        }
        let mut raised = false;
        // The factories the call removes, dropped after its last write (see
        // `remove_spec`): a panic in a drop is a panic of the calling callback.
        let mut removed: Vec<RestartFn> = Vec::new();
        for effect in commit.effects {
            match effect {
                CommitEffect::RaiseFirst => {
                    raised = self.with_child(&id, |child| child.ctl.raise_first(policy));
                }
                CommitEffect::WriteOrder => {
                    self.with_child(&id, |child| child.ctl.write_order(policy, raised));
                }
                CommitEffect::RecordOverride => {
                    if let Some(sup) = self.supervision_mut() {
                        sup.record_override(&id);
                    }
                }
                CommitEffect::LeaveRestartOrder => {
                    // The released member is taken out of the restart set at
                    // once, so nothing the group does later (the chain, a
                    // retry) can recapture a member the caller released.
                    if let Some(sup) = self.supervision_mut() {
                        sup.leave_restart_set(&id);
                    }
                }
                CommitEffect::Reask => self.with_child(&id, |child| child.ctl.ask_again()),
                CommitEffect::ReaskOnce => self.with_child(&id, |child| {
                    child.ctl.ask_again();
                    child.ctl.mark_reasked();
                }),
                CommitEffect::Dismiss => {
                    if let Some(sup) = self.supervision_mut() {
                        removed.extend(sup.dismiss_owed_evaluation(&id));
                    }
                }
                CommitEffect::RewriteSnapshot => {
                    if let Some(sup) = self.supervision_mut() {
                        sup.rewrite_restart_snapshot(&id);
                    }
                }
                CommitEffect::Classify => {
                    let reason = self.with_child(&id, |child| child.ctl.end_reason());
                    removed.extend(self.finish_manual_stop(&id, incarnation, kind, reason));
                }
                CommitEffect::Join => {
                    let mut cell = self.with_child(&id, |child| child.ctl.watch());
                    let joined = match await_cell(&mut cell, incarnation).await {
                        CellOutcome::Fate(fate) => {
                            removed.extend(self.finish_manual_stop(
                                &id,
                                incarnation,
                                kind,
                                fate.reason,
                            ));
                            done(StopChildOutcome::Stopped)
                        }
                        CellOutcome::Unresponsive => Err(SupervisionError::ChildUnresponsive(id)),
                        // No manual stop reaches an attempt (a child with a
                        // restart in flight answers `ChildRestarting`), so
                        // there is nothing to classify.
                        CellOutcome::Cancelled | CellOutcome::NoIncarnation => {
                            done(StopChildOutcome::Stopped)
                        }
                    };
                    drop(removed);
                    return joined;
                }
                CommitEffect::Answer(reply) => {
                    drop(removed);
                    return manual_reply(reply, &id).map(|outcome| (outcome, kind));
                }
            }
        }
        drop(removed);
        done(StopChildOutcome::Stopped)
    }

    /// Reads one fact of a registered child. Only called for effects whose
    /// decision named a child, so the child exists.
    fn with_child<R>(&self, id: &ActorId, read: impl FnOnce(&ChildState) -> R) -> R {
        let child = self
            .supervision
            .as_ref()
            .and_then(|sup| sup.registry.get(id))
            .expect("a manual stop effect names a registered child");
        read(child)
    }

    /// Classifies a manual stop whose child ended, when the ledger still
    /// holds the occupant that was stopped: a stop in progress keeps the kind
    /// it was committed with (the in-flight kind decides the disposition),
    /// and an occupant a terminate override concerns takes the call's kind.
    /// An occupant the ledger has already moved past was classified by the
    /// run loop.
    #[must_use = "the factory is dropped by the caller, after its last write"]
    fn finish_manual_stop(
        &mut self,
        id: &ActorId,
        incarnation: u64,
        kind: ManualStop,
        reason: StopReason,
    ) -> Option<RestartFn> {
        let classify_as = self
            .supervision_ref()
            .and_then(|sup| sup.registry.get(id))
            .and_then(|child| match child.lifecycle {
                ChildLifecycle::Stopping {
                    incarnation: n,
                    kind: in_flight,
                } if n == incarnation => Some(in_flight),
                ChildLifecycle::Running(n) if n == incarnation => Some(kind),
                _ => None,
            });
        classify_as.and_then(|classify_as| {
            self.classify_manual_completion(id, incarnation, classify_as, reason)
        })
    }

    /// Classifies a manual stop's REAL, observed disposition once its fate
    /// cell resolves, as [`rules::classify_manual`] decides it, and either
    /// builds the `on_child_stopped` snapshot the run loop's death arm will
    /// deliver, or - for a `Bounce` racing a genuine `Failure` - records the
    /// end as owing its restart decision, so the death is instead evaluated
    /// as an ordinary crash (budget charge, strategy, restarts), exactly like
    /// any other failure. The ended incarnation is recorded down before a
    /// restart of the child begins.
    ///
    /// Called from two places: `manual_stop_child` itself, once its own
    /// await on the fate cell resolves, and - for cancel-safety - the run
    /// loop's death arm (`handle_child_stopped`), as a fallback for a caller
    /// that dropped its `terminate_child`/`stop_child` future right after the
    /// synchronous commit and before ever observing the fate: the ledger is
    /// still `Stopping{kind}` when the death arm gets there, so it finishes
    /// the classification itself instead of ever treating an un-classified
    /// manual stop as a plain, strategy-evaluated crash.
    ///
    /// Returns the factory of a spec it removed, once every write here is
    /// done, for the caller to drop after its own last write.
    #[must_use = "the factory is dropped by the caller, after its last write"]
    pub(crate) fn classify_manual_completion(
        &mut self,
        id: &ActorId,
        incarnation: u64,
        kind: ManualStop,
        reason: StopReason,
    ) -> Option<RestartFn> {
        let sup = self.supervision_mut()?;
        let pool = matches!(sup.config.strategy, RestartStrategy::SimpleOneForOne);
        let (restart_type, child_name) = sup
            .registry
            .get(id)
            .map(|child| (child.spec.restart_type, child.name.clone()))?;
        // A member the pending group restarts, and not released by a
        // `terminate_child`: its end belongs to that restart. A member the
        // group only stops (a `Temporary` one) is not in the set.
        let in_restart_set = sup.in_restart_set(id);
        let outcome = rules::classify_manual(kind, &reason, restart_type, pool, in_restart_set);

        // The end of the ledger's occupant is recorded first: a bounce that
        // met a crash owes its restart decision, any other completion is
        // kept for the run loop's snapshot, and a spec that is not kept is
        // removed.
        let mut factory = None;
        if outcome.remove {
            factory = sup.remove_spec(id);
        } else if let Some(child) = sup.registry.get_mut(id) {
            let _ = child.transition(ChildLifecycle::Down {
                incarnation,
                pending: outcome.pending,
            });
        }
        // A bounce that met a crash is evaluated as the crash it was: no
        // snapshot is made.
        let Some(action) = outcome.action else {
            return factory;
        };
        sup.pending_manual_events.insert(
            (id.clone(), incarnation),
            ChildEvent {
                child_id: id.clone(),
                child_name,
                reason,
                action,
            },
        );
        if outcome.initiate {
            // Budget-FREE restart: "manual stop is not a failure". The child
            // is down before the attempt begins.
            let seq = self.resolved_system().next_incarnation();
            if let Some(sup) = self.supervision_mut() {
                sup.initiate(id, seq, Origin::Bounce);
            }
        }
        factory
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
    /// registered until the builder is awaited. A zero `interval` is
    /// rejected with [`TimerError::ZeroInterval`] at that point, before
    /// anything is spawned. An `interval` beyond 30 years is treated as 30
    /// years, which in practice never fires.
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
        let claimed = Arc::new(AtomicBool::new(false));
        let fut = oneshot_forward(handle, message, when, cancel_clone, claimed.clone());
        self.spawn_forwarder(ForwarderKind::Timer(id), fut, PanicSlot::default());
        self.timers.insert(
            id,
            TimerRegistration {
                token,
                claimed: Some(claimed),
            },
        );
        Ok(id)
    }

    /// Cancels a specific timer by its ID.
    ///
    /// For a one-shot timer, `Ok` means its message was not put in the
    /// mailbox and never will be. `Err(TimerError::NotFound)` means the timer
    /// fired, so its message is in the mailbox or is being put there at that
    /// instant (a message queued right after the answer may arrive first, and
    /// an actor that stops first drops it with its other queued messages), or
    /// that it was already cancelled or never existed. OTP's `cancel_timer`
    /// likewise returns `false` for an expired timer without saying whether
    /// its message has arrived.
    ///
    /// For a recurring timer, `Ok` means no later tick is sent; a tick
    /// already sent, or being sent at that instant, arrives.
    pub fn cancel_timer(&mut self, id: RecurringId) -> Result<(), TimerError> {
        match self.timers.remove(&id) {
            Some(entry) if entry.claim() => {
                entry.token.cancel();
                Ok(())
            }
            _ => Err(TimerError::NotFound),
        }
    }

    /// Cancels all active timers: no later message is sent by any of them.
    /// A one-shot timer that fired keeps its message in the mailbox, or puts
    /// it there at that instant, as [`cancel_timer`](Self::cancel_timer)
    /// reads; a recurring tick being sent at that instant still arrives.
    pub fn cancel_all_timers(&mut self) {
        for entry in self.timers.values() {
            if entry.claim() {
                entry.token.cancel();
            }
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
        let slot = PanicSlot::default();
        let fut = stream_forward::<A, S>(stream, handle, cancel_clone, slot.clone());
        self.spawn_forwarder(ForwarderKind::Stream(id), fut, slot);
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
    kill_grace: Duration,
    _child: std::marker::PhantomData<C>,
}

impl<'ctx, A: Actor, F, C: Actor> std::fmt::Debug for ChildSpawnBuilder<'ctx, A, F, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSpawnBuilder")
            .field("name", &self.name)
            .field("restart_type", &self.restart_type)
            .field("shutdown", &self.shutdown)
            .field("start_timeout", &self.start_timeout)
            .field("kill_grace", &self.kill_grace)
            .finish_non_exhaustive()
    }
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
    ///
    /// When the child is itself a supervisor, its `Shutdown` must cover its
    /// own children: at least the sum of their stop budgets (each child's
    /// `Shutdown`, plus its kill grace, plus the short wait that confirms an
    /// abort), or the largest of them for a `SimpleOneForOne` pool. A shorter
    /// window escalates the child to `Kill` and its remaining children take
    /// the killed-supervisor path. Erlang/OTP asks for `infinity` here: "If
    /// the child process is another supervisor, the shutdown time must be set
    /// to infinity to give the subtree ample time to shut down."
    pub fn shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Sets how long this child is given, after a `Kill` is raised on it, to
    /// reach its next turn boundary before its task is aborted. Default:
    /// 120 ms. Kept across every restart of the child.
    ///
    /// `Duration::ZERO` aborts the task as soon as the `Kill` is raised, so a
    /// callback in progress is cut at its next `.await` and never observes
    /// the `Kill` itself. The grace applies to every kill of this child:
    /// [`ActorSystem::kill`](crate::system::ActorSystem::kill),
    /// [`ActorSystem::kill_by_id`](crate::system::ActorSystem::kill_by_id),
    /// [`ActorHandle::stop`](crate::actor::handle::ActorHandle::stop) with
    /// [`StopReason::Kill`], the kill step of
    /// [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within)
    /// and of system shutdown, its supervisor's stop of it, and the kill a
    /// worker receives when its own supervisor is killed. An actor that is
    /// itself a supervisor stops in order when its supervisor is killed,
    /// bounded by its own [`Shutdown`], and the
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

    /// Overrides the default [`ActorConfig`](runtime::ActorConfig) for the child.
    pub fn with_config(mut self, config: runtime::ActorConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Bounds the supervisor's wait for this child's initialization to complete
    /// during a restart. `None` (default) waits indefinitely, matching
    /// Erlang/OTP supervisor semantics; setting a bound is a deliberate
    /// deviation that trades strict parity for a supervisor that can never be
    /// stalled by one child's hung init. Initial spawning never awaits
    /// initialization (see [`spawn_child`](ActorContext::spawn_child)), so this
    /// bound applies only to restarts. A bound beyond 30 years is treated as 30
    /// years.
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
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'ctx>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let ChildSpawnBuilder {
                ctx,
                factory,
                name,
                restart_type,
                shutdown,
                config,
                start_timeout,
                kill_grace,
                _child,
            } = self;
            ctx.await_retired_subtree(name.as_deref()).await?;
            ctx.spawn_child_internal(
                factory,
                name,
                restart_type,
                shutdown,
                config,
                start_timeout,
                kill_grace,
            )
        })
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

impl<'ctx, A: Actor> std::fmt::Debug for ScheduleBuilder<'ctx, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleBuilder").finish_non_exhaustive()
    }
}

impl<'ctx, A: Actor> ScheduleBuilder<'ctx, A> {
    /// Schedule a one-shot message at a specific [`Instant`]. An instant in
    /// the past fires at once; one more than 30 years away is treated as 30
    /// years away, which in practice never fires.
    pub fn at(self, when: Instant) -> OneShotSchedule<'ctx, A> {
        let now = Instant::now();
        let when = if when > now {
            runtime::saturating_deadline(now, when - now)
        } else {
            when
        };
        OneShotSchedule {
            ctx: self.ctx,
            message: self.message,
            when,
        }
    }

    /// Schedule a one-shot message after a [`Duration`]. A delay beyond 30
    /// years is treated as 30 years, which in practice never fires.
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
    /// `Clone`. A zero `interval` is rejected with
    /// [`TimerError::ZeroInterval`] when the schedule is awaited, before
    /// anything is spawned. An `interval` beyond 30 years is treated as 30
    /// years, which in practice never fires.
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

impl<'ctx, A: Actor> std::fmt::Debug for OneShotSchedule<'ctx, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneShotSchedule")
            .field("when", &self.when)
            .finish_non_exhaustive()
    }
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

impl<'ctx, A: Actor> std::fmt::Debug for RecurringScheduleBuilder<'ctx, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecurringScheduleBuilder")
            .field("interval", &self.interval)
            .field("miss_policy", &self.miss_policy)
            .finish_non_exhaustive()
    }
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
        // A zero interval would fire again on the very same clock reading
        // forever, with no `.await` in that path to ever yield or observe
        // cancellation, so it is rejected here, before an id is minted or
        // anything is spawned.
        if self.interval.is_zero() {
            return std::future::ready(Err(TimerError::ZeroInterval));
        }
        let id = self.ctx.id_gen.next();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.ctx.self_handle.clone();
        let slot = PanicSlot::default();
        let fut = recurring_loop(
            handle,
            self.factory,
            self.interval,
            self.miss_policy,
            cancel_clone,
            slot.clone(),
        );
        self.ctx
            .spawn_forwarder(ForwarderKind::Timer(id), fut, slot);
        self.ctx.timers.insert(
            id,
            TimerRegistration {
                token,
                claimed: None,
            },
        );
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
        // A context dropped before the terminal write ends an incarnation
        // that was dropped from outside: the reason is `Kill`, or a panic
        // when this task's own poll is unwinding (see
        // `runtime::TaskMarker`). It writes first only for a task dropped
        // before its first poll; on every other path the task's abort guard
        // or its seal wrote the reason already, and a written reason stays.
        StatusValue::write_reason(&self.status_tx, self.task_marker.abort_reason(), false);
    }
}

// ---------------------------------------------------------------------------
// Internal types and helpers
// ---------------------------------------------------------------------------

struct TimerRegistration {
    token: CancellationToken,
    /// For a one-shot timer, set once by whichever comes first: its firing,
    /// which then puts its message in the mailbox, or its cancel. `None` for
    /// a recurring timer.
    claimed: Option<Arc<AtomicBool>>,
}

impl TimerRegistration {
    /// Claims the timer for a cancel: always for a recurring timer, and for
    /// a one-shot timer only when its firing has not claimed it first.
    fn claim(&self) -> bool {
        self.claimed
            .as_ref()
            .is_none_or(|claimed| !claimed.swap(true, Ordering::SeqCst))
    }
}

/// A timer or stream forwarder's future, together with the slot its
/// [`ReleaseGuard`] parks a panic in and the end signals of the actor it
/// belongs to: a sender of its status plane and its hold on its subtree.
/// Its drop drops the future first, inside a catch, so however the forwarder
/// ends (its future completes, it is aborted before its first poll or while
/// suspended) the actor's status plane and its subtree read as ended only
/// once the stream, message or factory the future owns is gone.
///
/// A panic parked in the slot is the task's panic when the future itself did
/// not panic: a poll turns it into [`ForwarderEnd::Panicked`], and a drop
/// raises it again for whoever drops the task, which tokio always does
/// inside a catch. When the future panicked too, its own panic is the
/// task's, and the parked one is logged, naming the actor.
struct Forwarder<F> {
    future: Option<Pin<Box<F>>>,
    slot: PanicSlot,
    owner: ActorId,
    _status: watch::Sender<StatusValue>,
    _subtree: Arc<SubtreeHold>,
}

impl<F> Drop for Forwarder<F> {
    fn drop(&mut self) {
        let future = self.future.take();
        let own = catch_sync(move || drop(future)).err();
        let parked = take_parked(&self.slot);
        match (own, parked) {
            (Some(own), parked) => {
                if let Some(parked) = parked {
                    runtime::log_discarded(&payload_into_string(parked), &self.owner);
                }
                std::panic::resume_unwind(own);
            }
            (None, Some(parked)) => std::panic::resume_unwind(parked),
            (None, None) => {}
        }
    }
}

/// How a forwarder task ended.
pub(crate) enum ForwarderEnd {
    /// Its future completed, releasing nothing whose drop panicked.
    Done,
    /// A poll of its future panicked, during the run: an aborted forwarder
    /// is never polled again.
    Panicked(String),
    /// Its future completed after catching panics in the drops it performed
    /// itself. `run` is a drop made before it saw the mailbox closed; `end`
    /// is a drop made once it had seen the mailbox closed, which the end
    /// classifies by registration as it classifies an aborted forwarder's
    /// drop.
    Released {
        run: Option<String>,
        end: Option<String>,
    },
}

/// The drops a forwarder performed itself and whose panics it caught.
#[derive(Default)]
struct Released {
    /// A drop before the forwarder saw the mailbox closed.
    run: Option<String>,
    /// A drop once the forwarder had seen the mailbox closed.
    end: Option<String>,
}

impl Released {
    /// Drops what a forwarder owns once it has seen the mailbox closed.
    fn at_the_close<T>(held: T) -> Self {
        Released {
            run: None,
            end: release(held),
        }
    }

    /// As [`at_the_close`](Self::at_the_close) for two items, each dropped
    /// in its own catch; the first panic is kept, and a second is logged,
    /// naming the actor `owner`, and discarded.
    fn both_at_the_close<T, U>(first: T, second: U, owner: &ActorId) -> Self {
        let mut end = release(first);
        runtime::keep_first_of(&mut end, release(second), owner);
        Released { run: None, end }
    }
}

/// The build of a restart attempt: the actor its factory makes, or the
/// outcome the attempt ends with.
///
/// The factory is reached through the spec's weak reference; the upgrade runs
/// no user code. A spec already removed leaves nothing to build from: the
/// attempt is cancelled, and the cancel is recorded in the cell for the wait
/// on it. The build runs in its own catch and only borrows the factory, so no
/// user drop runs inside its unwind; a panicking factory surfaces as
/// `FactoryFailed` and charges the budget when the event re-enters strategy
/// evaluation.
///
/// The spec's box holds the factory unless the teardown or a `Kill` removed
/// the spec during the build. The attempt's reference is then the last one,
/// and the attempt starts no incarnation, whether or not the factory's drop
/// panics: the factory drops here, in its own catch, and a panic of that
/// drop is logged once and carried out on the cancelled outcome, the
/// supervisor's as if the spec's own removal had raised it. Whatever was
/// built drops in its own catch, a panic of the build or of that drop is
/// logged once, and the cancel is recorded in the cell. An attempt whose
/// build finished before the removal starts its incarnation as before: the
/// teardown stops it, or under `Kill` its link guard raises `Kill` once the
/// aborted attempt reaches the first await that returns `Pending`.
fn build_for_attempt<F, A>(
    factory: &std::sync::Weak<F>,
    io: &SlotIo,
    supervisor: &ActorId,
    child_id: &ActorId,
    incarnation: u64,
) -> Result<A, Box<RestartOutcome>>
where
    F: Fn() -> A,
{
    let Some(factory) = factory.upgrade() else {
        io.mark_cancelled();
        return Err(Box::new(RestartOutcome::Cancelled {
            child_id: child_id.clone(),
            incarnation,
            drop_panic: None,
        }));
    };
    let built = catch_sync(|| factory());
    let Some(last) = Arc::into_inner(factory) else {
        return built.map_err(|payload| {
            let msg = payload_into_string(payload);
            Box::new(RestartOutcome::Failed {
                child_id: child_id.clone(),
                incarnation,
                reason: StopReason::Failure(SupervisionError::FactoryFailed(msg).into()),
            })
        });
    };
    let drop_panic = release(last);
    #[cfg(feature = "tracing")]
    if let Some(payload) = &drop_panic {
        caught_log!(warn,
            supervisor = %supervisor,
            child = %child_id,
            incarnation,
            panic = %payload,
            "a restart attempt held the last reference to its child's factory, whose spec was removed during the build, and the factory's drop panicked; the panic is the supervisor's"
        );
    }
    let other = match built {
        Ok(actor) => release(actor),
        Err(payload) => Some(payload_into_string(payload)),
    };
    #[cfg(feature = "tracing")]
    if let Some(payload) = &other {
        caught_log!(warn,
            supervisor = %supervisor,
            child = %child_id,
            incarnation,
            panic = %payload,
            "a restart attempt's build, or the drop of what it built, panicked after its child's spec was removed; the attempt is cancelled"
        );
    }
    #[cfg(not(feature = "tracing"))]
    let _ = (other, supervisor);
    io.mark_cancelled();
    Err(Box::new(RestartOutcome::Cancelled {
        child_id: child_id.clone(),
        incarnation,
        drop_panic,
    }))
}

/// The panic a forwarder's [`ReleaseGuard`] parks for its [`Forwarder`],
/// which reports it as the task's panic.
type PanicSlot = Arc<std::sync::Mutex<Option<PanicPayload>>>;

/// Takes the panic parked in `slot`, if any.
fn take_parked(slot: &PanicSlot) -> Option<PanicPayload> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

/// Holds a value a timer or stream forwarder owns for its whole life: a
/// recurring timer's factory or a stream. Every path that ends the
/// forwarder takes the value out and releases it itself; this guard drops it
/// only when the forwarder's future is dropped with it still held (an
/// abort, or a return that leaves it). Its drop runs inside a catch and never
/// unwinds: a panic there is parked in the forwarder's slot, where the
/// [`Forwarder`] reads it as the task's panic, unless a panic is parked
/// already, in which case it is logged, naming the actor, and discarded. The
/// guard is the first local of its forwarder's body, so it drops after the
/// tick, item or send in flight: a panic of theirs is the task's panic, and
/// a panic of the guard's value beside it is the one logged.
struct ReleaseGuard<T> {
    value: Option<T>,
    owner: ActorId,
    slot: PanicSlot,
}

impl<T> ReleaseGuard<T> {
    fn new(value: T, owner: ActorId, slot: PanicSlot) -> Self {
        Self {
            value: Some(value),
            owner,
            slot,
        }
    }

    fn get_mut(&mut self) -> &mut T {
        self.value
            .as_mut()
            .expect("the value is held until a path takes it")
    }

    /// Takes the value out, for a path that releases or returns it.
    fn take(&mut self) -> T {
        self.value
            .take()
            .expect("the value is held until a path takes it")
    }
}

impl<T> Drop for ReleaseGuard<T> {
    fn drop(&mut self) {
        let Some(value) = self.value.take() else {
            return;
        };
        let Err(payload) = catch_sync(move || drop(value)) else {
            return;
        };
        let second = {
            let mut slot = self
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(payload);
                None
            } else {
                Some(payload)
            }
        };
        if let Some(payload) = second {
            runtime::log_discarded(&payload_into_string(payload), &self.owner);
        }
    }
}

/// Holds the panic of a stream that ended during the run, from the
/// stream's drop until the forwarder reports it with its `Finished`. A
/// forwarder that ends before reporting it (a panicking `From` conversion,
/// or an abort while the `Finished` waits for room) logs it here, naming
/// the actor, so it is never lost unrecorded. Like every log line of the
/// crate it is written inside a catch, which matters here: this drop can run
/// while another panic unwinds.
struct RunPanic {
    payload: Option<String>,
    owner: ActorId,
}

impl RunPanic {
    /// Takes the panic out, for the forwarder's own report.
    fn take(&mut self) -> Option<String> {
        self.payload.take()
    }
}

impl Drop for RunPanic {
    fn drop(&mut self) {
        #[cfg(feature = "tracing")]
        if let Some(payload) = self.payload.take() {
            caught_log!(warn,
                actor_id = %self.owner,
                panic = %payload,
                "a stream that ended during the run panicked when dropped, and its forwarder ended before reporting that panic"
            );
        }
        #[cfg(not(feature = "tracing"))]
        let _ = &self.owner;
    }
}

/// Drops `held` now, catching a panic of its drop.
fn release<T>(held: T) -> Option<String> {
    catch_sync(move || drop(held))
        .err()
        .map(payload_into_string)
}

impl<F: Future<Output = Released>> Future for Forwarder<F> {
    type Output = ForwarderEnd;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<ForwarderEnd> {
        let this = &mut *self;
        let polled = catch_sync(|| {
            this.future
                .as_mut()
                .expect("a forwarder is not polled after it ended")
                .as_mut()
                .poll(cx)
        });
        // Taken at once, before any conversion or log: a later unwind in
        // this poll finds the slot empty.
        let parked = take_parked(&this.slot);
        match (polled, parked) {
            (Err(own), parked) => {
                if let Some(parked) = parked {
                    runtime::log_discarded(&payload_into_string(parked), &this.owner);
                }
                std::task::Poll::Ready(ForwarderEnd::Panicked(payload_into_string(own)))
            }
            (Ok(std::task::Poll::Ready(_)), Some(parked)) => {
                std::task::Poll::Ready(ForwarderEnd::Panicked(payload_into_string(parked)))
            }
            (Ok(std::task::Poll::Pending), Some(parked)) => {
                *this
                    .slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(parked);
                std::task::Poll::Pending
            }
            (
                Ok(std::task::Poll::Ready(Released {
                    run: None,
                    end: None,
                })),
                None,
            ) => std::task::Poll::Ready(ForwarderEnd::Done),
            (Ok(std::task::Poll::Ready(Released { run, end })), None) => {
                std::task::Poll::Ready(ForwarderEnd::Released { run, end })
            }
            (Ok(std::task::Poll::Pending), None) => std::task::Poll::Pending,
        }
    }
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

async fn oneshot_forward<A: Actor>(
    handle: ActorHandle<A>,
    message: A::Message,
    when: Instant,
    token: CancellationToken,
    claimed: Arc<AtomicBool>,
) -> Released {
    tokio::select! {
        biased;
        _ = token.cancelled() => return Released::default(),
        // The actor stopped before the timer fired; once the mailbox is
        // closed the firing is never taken.
        () = handle.mailbox_close() => return Released::at_the_close(message),
        _ = time::sleep_until(when) => {}
    }
    // The deadline passed: the firing waits for a slot in the mailbox, and a
    // cancel that comes first ends it.
    let permit = tokio::select! {
        biased;
        _ = token.cancelled() => return Released::default(),
        permit = handle.reserve_forward() => permit,
    };
    let Some(permit) = permit else {
        return Released::at_the_close(message);
    };
    // With the slot held, nothing awaits until the message is sent or
    // dropped: the timer is claimed here, by the firing, or it was claimed
    // by a cancel first, which then read `Ok`. A message the cancel claimed
    // drops in this poll, as one cancelled before the deadline does.
    if !claimed.swap(true, Ordering::SeqCst) {
        permit.send(message);
    } else {
        drop(permit);
        drop(message);
    }
    Released::default()
}

async fn recurring_loop<A: Actor>(
    handle: ActorHandle<A>,
    factory: Box<MessageFactory<A>>,
    interval: Duration,
    miss_policy: MissPolicy,
    token: CancellationToken,
    slot: PanicSlot,
) -> Released {
    // The first local of the body: when the future is dropped (the end's
    // abort), the tick and the send in flight, which live in inner scopes,
    // drop before it, so the factory's drop comes last. Keep it first.
    let mut factory = ReleaseGuard::new(factory, handle.id().clone(), slot);
    let mut next = runtime::saturating_deadline(Instant::now(), interval);
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => return Released::default(),
            // The actor stopped; once the mailbox is closed the factory never
            // runs again.
            () = handle.mailbox_close() => return Released::at_the_close(factory.take()),
            _ = time::sleep_until(next) => {
                let tick = (factory.get_mut().as_mut())();
                // The tick waits for a slot in the mailbox and is never moved
                // into the wait: a cancel that comes first ends the timer, and
                // the tick and the factory drop each in its own catch, in this
                // poll, as the run's.
                let permit = tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        let mut run = release(tick);
                        runtime::keep_first_of(&mut run, release(factory.take()), handle.id());
                        return Released { run, end: None };
                    }
                    permit = handle.reserve_forward() => permit,
                };
                let Some(permit) = permit else {
                    return Released::both_at_the_close(tick, factory.take(), handle.id());
                };
                permit.send(tick);
                match adjust_next(&mut next, interval, miss_policy, &token, &handle, factory.get_mut()) {
                    Adjusted::Next => {}
                    Adjusted::Refused(tick) => {
                        return Released::both_at_the_close(tick, factory.take(), handle.id());
                    }
                    Adjusted::Panicked(payload) => {
                        let mut run = Some(payload);
                        runtime::keep_first_of(&mut run, release(factory.take()), handle.id());
                        return Released { run, end: None };
                    }
                }
            }
        }
    }
}

async fn stream_forward<A, S>(
    stream: S,
    handle: ActorHandle<A>,
    token: CancellationToken,
    slot: PanicSlot,
) -> Released
where
    A: Actor,
    S: Stream + Send + Unpin + 'static,
    S::Item: Send + 'static,
    A::Message: From<StreamEvent<S::Item>>,
{
    // The first local of the body: when the future is dropped (the end's
    // abort), the item and the send in flight, which live in inner scopes,
    // drop before it, so the stream's drop comes last. Keep it first.
    let mut stream = ReleaseGuard::new(stream, handle.id().clone(), slot);
    let closed = loop {
        tokio::select! {
            biased;
            // A registration cancelled during the run: the stream is dropped
            // when this returns, in this poll.
            _ = token.cancelled() => return Released::default(),
            // The actor stopped; once the mailbox is closed the stream is
            // never polled again.
            () = handle.mailbox_close() => break true,
            item = StreamExt::next(stream.get_mut()) => {
                match item {
                    Some(value) => {
                        let msg: A::Message = StreamEvent::Data(value).into();
                        if let Err(msg) = handle.forward(msg).await {
                            return Released::both_at_the_close(msg, stream.take(), handle.id());
                        }
                    }
                    None => break false,
                }
            }
        }
    };
    if closed {
        return Released::at_the_close(stream.take());
    }
    // The close read just before the stream's drop is the order point.
    if handle.mailbox_closed() {
        return Released::at_the_close(stream.take());
    }
    // The stream ended during the run: it is dropped here, before its
    // `Finished` is sent.
    let mut run = RunPanic {
        payload: release(stream.take()),
        owner: handle.id().clone(),
    };
    let msg: A::Message = StreamEvent::Finished.into();
    match handle.forward(msg).await {
        Ok(()) => Released {
            run: run.take(),
            end: None,
        },
        Err(msg) => Released {
            run: run.take(),
            end: release(msg),
        },
    }
}

/// What [`adjust_next`] ended with.
enum Adjusted<M> {
    /// The timer goes on to its next tick.
    Next,
    /// The mailbox is closed: the tick that could not be sent is handed back.
    Refused(M),
    /// A `CatchUp` tick the full mailbox had no room for panicked when
    /// dropped, in its own catch: the timer ends with that panic.
    Panicked(String),
}

/// Moves `next` past the tick just delivered. Under `CatchUp` the missed
/// ticks are sent at once; a tick the full mailbox has no room for is
/// dropped, in its own catch, and the burst ends.
fn adjust_next<A: Actor>(
    next: &mut Instant,
    interval: Duration,
    miss_policy: MissPolicy,
    token: &CancellationToken,
    handle: &ActorHandle<A>,
    factory: &mut Box<MessageFactory<A>>,
) -> Adjusted<A::Message> {
    let now = Instant::now();
    match miss_policy {
        MissPolicy::Skip => {
            // The first grid point after `now`, computed in constant time
            // instead of a loop that visits every missed tick: `periods` is
            // the smallest `k` with `*next + k * interval > now`, derived
            // directly from how far behind `next` has fallen. A loop here
            // alone could never terminate for a zero interval, which is why
            // a zero interval is rejected before a timer is ever registered
            // (see `RecurringScheduleBuilder::into_future`), making
            // `interval` non-zero on every call.
            let behind = now.saturating_duration_since(*next);
            let periods = behind.as_nanos() / interval.as_nanos() + 1;
            *next = match u32::try_from(periods)
                .ok()
                .and_then(|periods| interval.checked_mul(periods))
            {
                Some(advance) => runtime::saturating_deadline(*next, advance),
                None => runtime::saturating_deadline(now, interval),
            };
        }
        MissPolicy::Delay => {
            *next = runtime::saturating_deadline(now, interval);
        }
        MissPolicy::CatchUp => {
            *next = runtime::saturating_deadline(*next, interval);
            while *next <= now {
                if token.is_cancelled() {
                    return Adjusted::Next;
                }
                let msg = (factory.as_mut())();
                match handle.try_forward(msg) {
                    Ok(()) => {}
                    // The mailbox has no room: this tick is skipped.
                    Err(mpsc::error::TrySendError::Full(tick)) => {
                        if let Some(payload) = release(tick) {
                            return Adjusted::Panicked(payload);
                        }
                        break;
                    }
                    Err(mpsc::error::TrySendError::Closed(msg)) => return Adjusted::Refused(msg),
                }
                *next = runtime::saturating_deadline(*next, interval);
            }
        }
    }
    Adjusted::Next
}

#[cfg(test)]
#[path = "forwarder_tests.rs"]
mod forwarder_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::supervision::{SupervisionConfig, SupervisionState};
    use crate::types::StopLane;

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
        let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
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
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

    /// A context dropped outside its task's own poll writes `Kill`, even
    /// during an unwind of another code path; a written reason stays.
    async fn the_context_drop_writes_the_reason_of_how_its_task_ended() {
        let ctx = dummy_ctx();
        let plane = ctx.self_handle().status_rx();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = ctx;
            panic!("an unwind that is not the task's own poll");
        }));
        assert!(unwound.is_err());
        assert_eq!(plane.borrow().reason, Some(StopReason::Kill));

        let ctx = dummy_ctx();
        let plane = ctx.self_handle().status_rx();
        drop(ctx);
        assert_eq!(plane.borrow().reason, Some(StopReason::Kill));

        let ctx = dummy_ctx();
        let plane = ctx.self_handle().status_rx();
        StatusValue::write_reason(&ctx.status_tx, StopReason::Graceful, false);
        drop(ctx);
        assert_eq!(plane.borrow().reason, Some(StopReason::Graceful));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_context_drop_writes_the_reason_of_how_its_task_ended_current_thread() {
        the_context_drop_writes_the_reason_of_how_its_task_ended().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_context_drop_writes_the_reason_of_how_its_task_ended_multi_thread() {
        the_context_drop_writes_the_reason_of_how_its_task_ended().await;
    }

    /// What a [`Timed`] actor is asked.
    enum TimedMsg {
        /// Schedules a one-shot timer due at once, waits until its forwarder
        /// has ended (it put its message in the mailbox), and cancels it,
        /// all in this one turn: the loop cannot reap the forwarder first.
        CancelAfterTheFiring,
        /// Fills the mailbox, schedules a one-shot timer due at once, lets
        /// its forwarder reach the full mailbox, and cancels it.
        CancelWhileTheSendWaits,
        /// Fills the one free slot of the mailbox.
        Filler,
        /// The timer's message.
        Fired,
        /// Answers once every message queued before it was handled.
        Ping,
    }

    struct Timed {
        fired: Arc<std::sync::atomic::AtomicUsize>,
        answers: Arc<std::sync::Mutex<Vec<Result<(), TimerError>>>>,
    }

    impl Actor for Timed {
        type Message = TimedMsg;
        type Response = ();

        async fn handle(
            &mut self,
            msg: TimedMsg,
            ctx: &mut ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            match msg {
                TimedMsg::CancelAfterTheFiring => {
                    let id = ctx.schedule(TimedMsg::Fired).after(Duration::ZERO).await?;
                    let joined = ctx
                        .forwarders
                        .join_next_with_id()
                        .await
                        .expect("the timer's forwarder ends");
                    let answer = ctx.cancel_timer(id);
                    ctx.reap_forwarder(joined);
                    self.answers.lock().unwrap().push(answer);
                }
                TimedMsg::CancelWhileTheSendWaits => {
                    if ctx.self_handle().try_notify(TimedMsg::Filler).is_err() {
                        panic!("the mailbox has one free slot");
                    }
                    let id = ctx.schedule(TimedMsg::Fired).after(Duration::ZERO).await?;
                    // The forwarder runs while this turn yields: its deadline
                    // has passed and the mailbox is full.
                    for _ in 0..16 {
                        tokio::task::yield_now().await;
                    }
                    let answer = ctx.cancel_timer(id);
                    self.answers.lock().unwrap().push(answer);
                }
                TimedMsg::Fired => {
                    self.fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                TimedMsg::Filler | TimedMsg::Ping => {}
            }
            Ok(())
        }
    }

    /// Runs `msg` on a fresh [`Timed`] actor whose mailbox holds one
    /// message, then a `Ping` behind everything the turn queued; returns
    /// the cancel's answer and how many times the timer's message arrived.
    async fn cancel_in_a_turn(msg: TimedMsg) -> (Vec<Result<(), TimerError>>, usize) {
        use crate::actor::ActorExt;
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let answers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let actor = Timed {
            fired: fired.clone(),
            answers: answers.clone(),
        }
        .spawn()
        .with_config(runtime::ActorConfig::default().with_mailbox_capacity(1))
        .await
        .expect("the actor starts");
        time::timeout(Duration::from_secs(10), actor.send(msg))
            .await
            .expect("the turn ends")
            .expect("the turn succeeds");
        time::timeout(Duration::from_secs(10), actor.send(TimedMsg::Ping))
            .await
            .expect("the ping is answered")
            .expect("the ping succeeds");
        let answers = answers.lock().unwrap().clone();
        let fired = fired.load(std::sync::atomic::Ordering::SeqCst);
        let _ = actor.stop(StopReason::Graceful).await;
        (answers, fired)
    }

    /// A one-shot timer whose forwarder put its message in the mailbox is
    /// not cancelled, even before the loop reaps the forwarder: the cancel
    /// reads `NotFound`, and the message arrives once.
    async fn a_cancel_after_the_firing_reads_not_found() {
        let (answers, fired) = cancel_in_a_turn(TimedMsg::CancelAfterTheFiring).await;
        assert!(
            matches!(answers.as_slice(), [Err(TimerError::NotFound)]),
            "{answers:?}"
        );
        assert_eq!(fired, 1, "the message arrives once");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_cancel_after_the_firing_reads_not_found_current_thread() {
        a_cancel_after_the_firing_reads_not_found().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancel_after_the_firing_reads_not_found_multi_thread() {
        a_cancel_after_the_firing_reads_not_found().await;
    }

    /// A one-shot timer whose send waits on a full mailbox is cancelled: the
    /// cancel reads `Ok`, and the message never arrives.
    async fn a_cancel_while_the_send_waits_reads_ok() {
        let (answers, fired) = cancel_in_a_turn(TimedMsg::CancelWhileTheSendWaits).await;
        assert!(matches!(answers.as_slice(), [Ok(())]), "{answers:?}");
        assert_eq!(fired, 0, "the message never arrives");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_cancel_while_the_send_waits_reads_ok_current_thread() {
        a_cancel_while_the_send_waits_reads_ok().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancel_while_the_send_waits_reads_ok_multi_thread() {
        a_cancel_while_the_send_waits_reads_ok().await;
    }

    /// The text of the panic a dropped factory's capture raises.
    const CAPTURE_PANIC: &str = "the factory's capture panics when dropped";
    /// The text of the panic a dropped built value raises.
    const VALUE_PANIC: &str = "the built value panics when dropped";

    /// Panics with its text when dropped outside an unwind, and counts the
    /// panics it raised.
    struct PanicOnDrop(&'static str, Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                panic!("{}", self.0);
            }
        }
    }

    type TestFactory<T> = Box<dyn Fn() -> T + Send + Sync>;

    /// The spec's strong reference to a factory, which the factory releases
    /// during its own call, as a teardown that removes the spec while the
    /// attempt builds does.
    type SpecHold<T> = Arc<std::sync::Mutex<Option<Arc<TestFactory<T>>>>>;

    /// A factory whose capture panics when dropped and whose spec is removed
    /// during its call, before `build` runs; returns the spec's hold and the
    /// attempt's weak reference.
    fn removed_during_build<T: 'static>(
        build: impl Fn() -> T + Send + Sync + 'static,
        panics: &Arc<std::sync::atomic::AtomicUsize>,
    ) -> (SpecHold<T>, std::sync::Weak<TestFactory<T>>) {
        removed_during_build_with(build, Some(PanicOnDrop(CAPTURE_PANIC, panics.clone())))
    }

    /// As [`removed_during_build`], with a capture whose drop panics only
    /// when one is given.
    fn removed_during_build_with<T: 'static>(
        build: impl Fn() -> T + Send + Sync + 'static,
        capture: Option<PanicOnDrop>,
    ) -> (SpecHold<T>, std::sync::Weak<TestFactory<T>>) {
        let hold: SpecHold<T> = Arc::default();
        let inner = hold.clone();
        let factory: TestFactory<T> = Box::new(move || {
            let _ = &capture;
            let spec = inner.lock().unwrap().take();
            drop(spec);
            build()
        });
        let factory = Arc::new(factory);
        let weak = Arc::downgrade(&factory);
        *hold.lock().unwrap() = Some(factory);
        (hold, weak)
    }

    /// Counts its drops.
    struct Counted(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The outcome a cell reads once its attempt's executor is gone.
    async fn cell_after(ctl: &crate::actor::supervision::SlotCtl, io: SlotIo) -> CellOutcome {
        drop(io);
        let mut watch = ctl.watch();
        time::timeout(Duration::from_secs(10), await_cell(&mut watch, 2))
            .await
            .expect("the cell settles")
    }

    /// An attempt whose child's spec was removed before it built cancels,
    /// and the cancel is recorded in its cell, as at the other cancel points.
    #[tokio::test]
    async fn an_attempt_whose_spec_is_gone_cancels_in_its_cell() {
        let factory: Arc<TestFactory<u32>> = Arc::new(Box::new(|| 7));
        let weak = Arc::downgrade(&factory);
        drop(factory);
        let (ctl, io) = crate::actor::supervision::slot_pair(Default::default());
        let built = build_for_attempt(&weak, &io, &ActorId::from("sup"), &ActorId::from("c"), 2)
            .map_err(|outcome| *outcome);
        assert!(matches!(
            built,
            Err(RestartOutcome::Cancelled {
                incarnation: 2,
                drop_panic: None,
                ..
            })
        ));
        assert!(matches!(cell_after(&ctl, io).await, CellOutcome::Cancelled));
    }

    /// A spec removed during the build leaves the attempt the factory's last
    /// reference: the attempt starts nothing even when no drop panics. The
    /// value it built drops once, in the attempt, and the cancel is recorded
    /// in the cell.
    async fn a_spec_removed_during_the_build_cancels_the_attempt_without_a_panic() {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = drops.clone();
        let (hold, weak) = removed_during_build_with(move || Counted(counted.clone()), None);
        let (ctl, io) = crate::actor::supervision::slot_pair(Default::default());
        let built = build_for_attempt(&weak, &io, &ActorId::from("sup"), &ActorId::from("c"), 2)
            .map_err(|outcome| *outcome);
        assert!(matches!(
            built,
            Err(RestartOutcome::Cancelled {
                incarnation: 2,
                drop_panic: None,
                ..
            })
        ));
        assert!(hold.lock().unwrap().is_none());
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(cell_after(&ctl, io).await, CellOutcome::Cancelled));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_spec_removed_during_the_build_cancels_the_attempt_without_a_panic_current_thread() {
        a_spec_removed_during_the_build_cancels_the_attempt_without_a_panic().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_spec_removed_during_the_build_cancels_the_attempt_without_a_panic_multi_thread() {
        a_spec_removed_during_the_build_cancels_the_attempt_without_a_panic().await;
    }

    /// A build that panics after the spec went cancels the attempt too: the
    /// panic is logged, not charged as a factory failure.
    async fn a_build_panic_after_the_spec_went_cancels_the_attempt() {
        #[cfg(feature = "tracing")]
        let logged = crate::actor::supervision::lost_runner::capture::install();
        let child = format!("build-panic-{}", uuid::Uuid::new_v4());
        let supervisor = format!("{child}-supervisor");
        let (_hold, weak) =
            removed_during_build_with(|| -> u32 { panic!("the build panics") }, None);
        let (ctl, io) = crate::actor::supervision::slot_pair(Default::default());
        let built = build_for_attempt(
            &weak,
            &io,
            &ActorId::from(supervisor.clone()),
            &ActorId::from(child.clone()),
            2,
        )
        .map_err(|outcome| *outcome);
        assert!(matches!(
            built,
            Err(RestartOutcome::Cancelled {
                drop_panic: None,
                ..
            })
        ));
        assert!(matches!(cell_after(&ctl, io).await, CellOutcome::Cancelled));
        // The build's panic is logged once, naming the supervisor and the
        // child.
        #[cfg(feature = "tracing")]
        {
            let (by_supervisor, by_child) = (
                format!("supervisor={supervisor} "),
                format!("child={child} "),
            );
            let lines: Vec<String> = logged
                .lock()
                .unwrap()
                .iter()
                .filter(|line| {
                    line.contains(&by_supervisor)
                        && line.contains(&by_child)
                        && line.contains("the build panics")
                })
                .cloned()
                .collect();
            assert_eq!(lines.len(), 1, "one line: {lines:?}");
        }
        #[cfg(not(feature = "tracing"))]
        let _ = (child, supervisor);
    }

    /// A forwarder that panicked before it saw the mailbox closed and again
    /// after keeps the later panic as its error; with the `tracing` feature
    /// each panic is logged once, naming the actor: the earlier one as
    /// discarded, the later one as stored.
    #[tokio::test]
    async fn a_forwarder_that_panicked_before_and_after_the_close_keeps_the_later() {
        #[cfg(feature = "tracing")]
        let logged = crate::actor::supervision::lost_runner::capture::install();
        let mut ctx = dummy_ctx();
        let id = tokio::spawn(async {}).id();
        let token = uuid::Uuid::new_v4();
        let (run, end) = (format!("run {token}"), format!("end {token}"));
        let kept = ctx.reap_forwarder(Ok((
            id,
            ForwarderEnd::Released {
                run: Some(run.clone()),
                end: Some(end.clone()),
            },
        )));
        assert_eq!(kept.as_deref(), Some(end.as_str()));
        assert!(matches!(
            ctx.last_forwarder_error(),
            Some(ActorError::Panic(text)) if *text == end
        ));
        #[cfg(feature = "tracing")]
        {
            let lines: Vec<String> = logged
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains(&token.to_string()))
                .cloned()
                .collect();
            assert_eq!(lines.len(), 2, "{lines:?}");
            assert!(lines.iter().all(|line| line.contains("actor_id=dummy ")));
            assert!(lines.iter().any(|line| line.contains(&run)));
            assert!(lines.iter().any(|line| line.contains(&end)));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_build_panic_after_the_spec_went_cancels_the_attempt_current_thread() {
        a_build_panic_after_the_spec_went_cancels_the_attempt().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_build_panic_after_the_spec_went_cancels_the_attempt_multi_thread() {
        a_build_panic_after_the_spec_went_cancels_the_attempt().await;
    }

    /// The factory's drop panic is logged once where it happens, naming the
    /// supervisor and the child, and still carried out on the outcome: under `Kill` the outcome
    /// is dropped unread, and the log line is its only record.
    #[cfg(feature = "tracing")]
    #[tokio::test]
    async fn an_attempt_logs_the_factory_drop_panic_at_its_source() {
        let logged = crate::actor::supervision::lost_runner::capture::install();
        let child = format!("logged-at-the-source-{}", uuid::Uuid::new_v4());
        let panics = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_hold, weak) = removed_during_build(|| 7u32, &panics);
        let (_ctl, io) = crate::actor::supervision::slot_pair(Default::default());
        let supervisor = format!("{child}-supervisor");
        let built = build_for_attempt(
            &weak,
            &io,
            &ActorId::from(supervisor.clone()),
            &ActorId::from(child.clone()),
            2,
        )
        .map_err(|outcome| *outcome);
        assert!(matches!(
            built,
            Err(RestartOutcome::Cancelled {
                drop_panic: Some(_),
                ..
            })
        ));
        let (by_supervisor, by_child) = (
            format!("supervisor={supervisor} "),
            format!("child={child} "),
        );
        let lines: Vec<String> = logged
            .lock()
            .unwrap()
            .iter()
            .filter(|line| {
                line.contains(&by_supervisor)
                    && line.contains(&by_child)
                    && line.contains(CAPTURE_PANIC)
            })
            .cloned()
            .collect();
        assert_eq!(lines.len(), 1, "one line: {lines:?}");
    }

    /// While the spec holds the factory, the attempt's reference is not the
    /// last one: the build's value, or its panic as `FactoryFailed`, is the
    /// outcome, and the factory lives on with the spec.
    #[tokio::test]
    async fn an_attempt_whose_spec_holds_the_factory_builds_or_fails() {
        let factory: Arc<TestFactory<u32>> = Arc::new(Box::new(|| 7));
        let (_ctl, io) = crate::actor::supervision::slot_pair(Default::default());
        let built = build_for_attempt(
            &Arc::downgrade(&factory),
            &io,
            &ActorId::from("sup"),
            &ActorId::from("c"),
            2,
        )
        .map_err(|outcome| *outcome);
        assert!(matches!(built, Ok(7)));
        let failing: Arc<TestFactory<u32>> = Arc::new(Box::new(|| panic!("the factory fails")));
        let built = build_for_attempt(
            &Arc::downgrade(&failing),
            &io,
            &ActorId::from("sup"),
            &ActorId::from("c"),
            2,
        )
        .map_err(|outcome| *outcome);
        match built {
            Err(RestartOutcome::Failed {
                reason:
                    StopReason::Failure(ActorError::Supervision(SupervisionError::FactoryFailed(msg))),
                ..
            }) => assert_eq!(msg, "the factory fails"),
            _ => panic!("expected FactoryFailed"),
        }
        assert_eq!(Arc::strong_count(&factory), 1, "the spec's reference stays");
        assert_eq!(Arc::strong_count(&failing), 1, "the spec's reference stays");
    }

    /// A spec removed while the attempt builds leaves the attempt the
    /// factory's last reference: the factory drops in the attempt's own
    /// catch, and its panic is carried out on the cancelled outcome, with the
    /// cancel recorded in the cell.
    #[tokio::test]
    async fn a_factory_whose_spec_goes_during_the_build_drops_in_the_attempt_with_its_panic() {
        let panics = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (hold, weak) = removed_during_build(|| 7u32, &panics);
        let (ctl, io) = crate::actor::supervision::slot_pair(Default::default());
        let built = build_for_attempt(&weak, &io, &ActorId::from("sup"), &ActorId::from("c"), 2)
            .map_err(|outcome| *outcome);
        match built {
            Err(RestartOutcome::Cancelled {
                incarnation: 2,
                drop_panic: Some(payload),
                ..
            }) => assert_eq!(payload, CAPTURE_PANIC),
            _ => panic!("expected a cancel carrying the factory's drop panic"),
        }
        assert!(hold.lock().unwrap().is_none());
        assert_eq!(panics.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(cell_after(&ctl, io).await, CellOutcome::Cancelled));
    }

    /// The factory's drop and the built value's drop both panic, or the build
    /// panics and the factory's drop panics: each panic is caught on its own,
    /// never inside the unwinding of another, and the factory's is the one
    /// the outcome carries.
    #[tokio::test]
    async fn two_panics_of_an_attempt_whose_spec_went_are_each_caught_on_their_own() {
        let panics = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = panics.clone();
        let (_hold, weak) =
            removed_during_build(move || PanicOnDrop(VALUE_PANIC, counted.clone()), &panics);
        let (ctl, io) = crate::actor::supervision::slot_pair(Default::default());
        let built = build_for_attempt(&weak, &io, &ActorId::from("sup"), &ActorId::from("c"), 2)
            .map_err(|outcome| *outcome);
        match built {
            Err(RestartOutcome::Cancelled {
                drop_panic: Some(payload),
                ..
            }) => assert_eq!(payload, CAPTURE_PANIC),
            _ => panic!("expected a cancel carrying the factory's drop panic"),
        }
        assert_eq!(
            panics.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the factory's drop and the value's drop both ran outside any unwind"
        );
        assert!(matches!(cell_after(&ctl, io).await, CellOutcome::Cancelled));

        let panics = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_hold, weak) = removed_during_build(|| -> u32 { panic!("the build panics") }, &panics);
        let (ctl, io) = crate::actor::supervision::slot_pair(Default::default());
        let built = build_for_attempt(&weak, &io, &ActorId::from("sup"), &ActorId::from("c"), 2)
            .map_err(|outcome| *outcome);
        match built {
            Err(RestartOutcome::Cancelled {
                drop_panic: Some(payload),
                ..
            }) => assert_eq!(payload, CAPTURE_PANIC),
            _ => panic!("expected a cancel carrying the factory's drop panic"),
        }
        assert_eq!(
            panics.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the factory's drop ran outside the build's unwind"
        );
        assert!(matches!(cell_after(&ctl, io).await, CellOutcome::Cancelled));
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

    #[tokio::test(start_paused = true)]
    async fn skip_catch_up_lands_on_the_first_grid_point_after_now_in_constant_time() {
        for interval in [Duration::from_millis(1), Duration::from_nanos(1)] {
            let ctx = dummy_ctx();
            let handle = ctx.self_handle.clone();
            let token = CancellationToken::new();
            let mut factory: Box<MessageFactory<Dummy>> = Box::new(|| ());

            // Paused virtual time, advanced forward instead of subtracted
            // from `Instant::now()`: subtracting a large `Duration` from an
            // `Instant` panics once the instant is within that duration of
            // the platform's own start-of-time (observed on Windows, whose
            // `Instant` is an unsigned offset from process start).
            let old_next = Instant::now();
            time::advance(Duration::from_secs(3600)).await;
            let now = Instant::now();
            let mut next = old_next;

            // Wall time, not the paused virtual clock, which would report
            // zero elapsed regardless of how long `adjust_next` actually ran.
            let started = std::time::Instant::now();
            let adjusted = adjust_next(
                &mut next,
                interval,
                MissPolicy::Skip,
                &token,
                &handle,
                &mut factory,
            );
            assert!(matches!(adjusted, Adjusted::Next), "Skip sends nothing");
            let elapsed = started.elapsed();

            assert!(
                next > now,
                "the adjusted deadline must be strictly after `now`, interval={interval:?}"
            );
            let advanced = next.duration_since(old_next);
            assert_eq!(
                advanced.as_nanos() % interval.as_nanos(),
                0,
                "the adjusted deadline must land on an exact multiple of the \
                 interval from the original `next`, interval={interval:?}"
            );
            assert!(
                elapsed < Duration::from_millis(100),
                "adjust_next must run in constant time regardless of how far \
                 behind `next` has fallen, interval={interval:?}, took {elapsed:?}"
            );
        }
    }
}
