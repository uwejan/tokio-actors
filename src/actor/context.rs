//! Actor execution context providing timers, streams, and supervision hooks.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use futures_core::Stream;
use tokio_stream::StreamExt;

use tokio::task::{JoinError, JoinSet};

use crate::actor::alias::{Alias, AliasOptions, AliasShared};
use crate::actor::handle::ActorHandle;
use crate::actor::link::ExitPush;
use crate::actor::link::{self as link, LinkTable, TrapCell, TrapMap};
use crate::actor::monitor::{self, IntoMonitorTarget, MapFn, MonitorShared, MonitorState};
use crate::actor::panic::{catch_sync, contain_waker_panic, payload_into_string, PanicPayload};
use crate::actor::pg_monitor::GroupTable;
use crate::actor::request;
use crate::actor::subtree::{SubtreeHold, SubtreeLink};
use crate::actor::supervision::{
    await_cell, await_predecessor, await_subtree, cancelled_before_factory, manual_commit, rules,
    run_attempt, slot_pair_for, Attempt, AttemptTicket, AutoShutdown, CellOutcome, CellWatch,
    ChildLifecycle, ChildLinkGuard, ChildSpec, ChildState, CommitEffect, DeathOutcome, ManualOp,
    ManualReply, ManualStop, Origin, OwnStop, Predecessor, RestartFn, RestartOutcome, SlotIo,
    SupervisionState, TaskPlane, Verdicts, DEFAULT_KILL_GRACE,
};
use crate::actor::sys::{DebugEntry, DebugOptions, SysOptions};
use crate::actor::sys_debug::DebugState;
use crate::actor::sys_loop::LoopSys;
use crate::actor::turn::{IdleKind, TurnState};
use crate::actor::{runtime, Actor};
use crate::error::{
    ActorError, LinkError, MonitorError, RequestError, SpawnError, StopError, StreamError,
    SupervisionError, TimerError,
};
use crate::system::{raise_kill_declared, ActorSystem, KillKey};
use crate::telemetry::reports;
use crate::telemetry::{
    Birth, Delivery, EventKind, Filter, PublishTo, SubscriptionId, SubscriptionNotice,
    TelemetryEvent,
};
use crate::time_probe::{require_caller_timers, require_runtime_timers};
use crate::types::{
    ActorId, ActorStatus, ChildEvent, ChildInfo, Down, LinkExit, MissPolicy, MonitorId, PeerRef,
    RaiseResult, RecurringId, RecurringIdGenerator, RequestId, RequestReply, RestartStrategy,
    RestartType, Shutdown, SignalTag, StatusTx, StatusValue, StopChildOutcome, StopLane,
    StopReason, StreamEvent, StreamId, TurnClock,
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

/// What a manual stop's commit decided: the answer of the call, the kind the
/// end is classified with, and the ticket for the wait on the child's end
/// where the call waits for it.
pub(crate) struct Committed {
    /// What `stop_child` answers: `Stopped` for a committed stop.
    pub(crate) outcome: StopChildOutcome,
    /// The kind the stop is classified with when the child's end is seen.
    pub(crate) effective: ManualStop,
    /// The wait on the child's end, where the call waits for it.
    pub(crate) join: Option<JoinTicket>,
}

/// The wait a committed manual stop owes: the child's fate cell and the
/// incarnation the stop concerns.
pub(crate) struct JoinTicket {
    /// The cell that resolves when the incarnation ended.
    pub(crate) cell: CellWatch,
    /// The incarnation the stop concerns.
    pub(crate) incarnation: u64,
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
    /// The timers, streams and their forwarders, and the errors they and
    /// the supervision tasks recorded: created on first use, so an actor
    /// that never uses them holds one empty pointer.
    extras: Option<Box<Extras>>,
    /// The monitors this actor holds, created with the first one: an actor
    /// that never monitors holds one empty pointer. Their tasks are
    /// forwarders of `extras`.
    monitors: Option<Box<MonitorState<A>>>,
    #[allow(dead_code)]
    _not_clone: NotClone,
    last_error: Option<ActorError>,
    /// Runtime status plane (`process_info`-style, see the `handle` module
    /// docs): the status and the final reason. Every write goes through
    /// `send_if_modified`, which succeeds even when no reader is left and
    /// leaves a reason already written as it is.
    status_tx: StatusTx,
    /// Set while this actor's task is polled: tells this task's own unwind
    /// from an abort or an unrelated unwind when the context drops.
    task_marker: runtime::TaskMarker,
    // Supervision fields
    system: Option<Arc<ActorSystem>>,
    name: Option<Arc<str>>,
    supervision: Option<Box<SupervisionState>>,
    /// The actor's turn clock, bracketing the callbacks the run loop runs
    /// through the context.
    turn_clock: TurnClock,
    /// The actor's hold on its subtree token: the actors it spawns attach to
    /// it, and it is released only when the actor has ended.
    subtree: Arc<SubtreeHold>,
}

/// The parts of an [`ActorContext`] that only an actor with timers, streams
/// or supervision-task failures uses, boxed on first use. The field order
/// keeps the drop order the context had.
struct Extras {
    timers: HashMap<RecurringId, TimerRegistration>,
    streams: HashMap<StreamId, StreamRegistration>,
    /// The event subscriptions of the actor, by id, each with the token that
    /// ends its forwarder.
    subscriptions: HashMap<SubscriptionId, CancellationToken>,
    id_gen: RecurringIdGenerator,
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
    /// The forwarder ends counted so far, created with the first forwarder:
    /// each forwarder counts once, at the first of its ready poll and its
    /// drop, so before its task's entry in `forwarders` can be ready. Shared
    /// with every forwarder task, which writes it.
    forwarders_done: Option<Arc<AtomicU64>>,
    /// The entries of `forwarders` the run loop has taken.
    forwarders_taken: u64,
    /// Set when a forwarder is spawned and when one is reaped, cleared when
    /// the run loop's select polls the forwarder plane: until then the plane
    /// may hold no waker of the task, so the fast path does not skip it.
    forwarders_unpolled: bool,
    /// The debug state of the `sys` bundle: statistics, log, trace and the
    /// installed functions. It sits here, created with the first use of the
    /// bundle, so an actor that uses none of it holds no field for it.
    debug: Option<Box<DebugState>>,
}

impl Extras {
    fn new() -> Self {
        Self {
            timers: HashMap::new(),
            streams: HashMap::new(),
            subscriptions: HashMap::new(),
            id_gen: RecurringIdGenerator::default(),
            forwarders: JoinSet::new(),
            forwarder_kinds: HashMap::new(),
            last_forwarder_error: None,
            last_supervision_task_error: None,
            forwarders_done: None,
            forwarders_taken: 0,
            forwarders_unpolled: false,
            debug: None,
        }
    }
}

/// What a context without extras reads.
fn no_extras() -> &'static Extras {
    static EMPTY: std::sync::OnceLock<Extras> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Extras::new)
}

impl<A: Actor> std::fmt::Debug for ActorContext<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorContext")
            .field("actor_id", &self.actor_id)
            .field("name", &self.name)
            .field("status", &self.status())
            .field("timer_count", &self.ex().timers.len())
            .field("stream_count", &self.ex().streams.len())
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
        status_tx: StatusTx,
        system: Option<Arc<ActorSystem>>,
        name: Option<Arc<str>>,
        supervision: Option<SupervisionState>,
        turn_clock: TurnClock,
        subtree: Arc<SubtreeHold>,
    ) -> Self {
        Self {
            actor_id,
            self_handle: handle,
            runtime,
            extras: None,
            monitors: None,
            _not_clone: NotClone,
            last_error: None,
            status_tx,
            system,
            name,
            supervision: supervision.map(Box::new),
            turn_clock,
            subtree,
            task_marker: runtime::TaskMarker::default(),
        }
    }

    /// Forgets an earlier [`stop`](Self::stop): called by the run loop right
    /// before a handler runs.
    pub(crate) fn clear_self_stop(&mut self) {
        self.stop_lane().abort_record().clear_self_stop();
        if let Some(state) = self.monitors.as_deref_mut() {
            if let Some(turn) = state.turn_mut() {
                turn.end_spell();
                // A message set by a callback that does not run the chain
                // (`on_child_stopped`, a vetoing `pre_stop`) is not carried to
                // this turn.
                turn.drop_continuation();
            }
        }
    }

    /// Whether a receive timeout or an idle length is set.
    #[inline]
    pub(crate) fn idle_armed(&self) -> bool {
        self.monitors
            .as_deref()
            .is_some_and(|state| state.turn().is_some_and(TurnState::armed))
    }

    /// The nearest threshold of the current idle spell that has not fired,
    /// starting the spell when `quiet` says the loop is about to wait.
    pub(crate) fn idle_next(&mut self, now: Instant, quiet: bool) -> Option<(Instant, IdleKind)> {
        self.monitors
            .as_deref_mut()
            .and_then(|state| state.turn_mut()?.idle_next(now, quiet))
    }

    /// Records that a threshold fired in the current spell.
    pub(crate) fn mark_fired(&mut self, kind: IdleKind) {
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.mark_fired(kind);
        }
    }

    /// Marks a fire of the idle spell as running or done: the turns it causes
    /// do not end the spell.
    pub(crate) fn set_inline(&mut self, on: bool) {
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.set_inline(on);
        }
    }

    /// Makes the message of the receive timeout, inside a catch.
    pub(crate) fn make_receive_message(&self) -> Option<Result<A::Message, String>> {
        self.monitors
            .as_deref()
            .and_then(|state| state.turn()?.make_receive())
    }

    /// Whether a message waits to be handled next.
    #[inline]
    pub(crate) fn has_continuation(&self) -> bool {
        self.monitors
            .as_deref()
            .is_some_and(|state| state.turn().is_some_and(TurnState::has_continuation))
    }

    /// Takes the message to handle next.
    pub(crate) fn take_continuation(&mut self) -> Option<A::Message> {
        self.monitors
            .as_deref_mut()
            .and_then(|state| state.turn_mut()?.take_continuation())
    }

    /// Drops the message to handle next, its drop caught.
    pub(crate) fn drop_continuation(&mut self) {
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.drop_continuation();
        }
    }

    /// The aliases this actor made that still accept messages.
    pub(crate) fn alias_count(&self) -> usize {
        self.monitors
            .as_deref()
            .map_or(0, |state| state.turn().map_or(0, TurnState::alias_count))
    }

    /// The turn state, made with the first use.
    fn turn_mut(&mut self) -> &mut TurnState<A> {
        let owner = self.owner();
        self.monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)))
            .turn_made()
    }

    /// Whether the handler that just returned stopped its own actor; reads
    /// the flag and leaves it.
    pub(crate) fn self_stop_set(&self) -> bool {
        self.stop_lane().abort_record().self_stop_set()
    }

    /// Whether the handler that just returned stopped its own actor; reads
    /// the flag once and clears it.
    pub(crate) fn take_self_stop(&mut self) -> bool {
        self.stop_lane().abort_record().take_self_stop()
    }

    /// The extras, or an empty set when none were created.
    fn ex(&self) -> &Extras {
        match self.extras.as_deref() {
            Some(extras) => extras,
            None => no_extras(),
        }
    }

    /// The extras, created on first use.
    fn ex_mut(&mut self) -> &mut Extras {
        self.extras.get_or_insert_with(|| Box::new(Extras::new()))
    }

    /// Starts the incarnation's debug state, at spawn.
    pub(crate) fn set_debug(&mut self, debug: DebugState) {
        self.ex_mut().debug = Some(Box::new(debug));
    }

    /// The `sys` bundle's view of this incarnation, for serving one call.
    pub(crate) fn sys_loop(&mut self) -> LoopSys<'_> {
        let record = self.self_handle.sys_record();
        let extras = self.extras.get_or_insert_with(|| Box::new(Extras::new()));
        LoopSys::new(record, &mut extras.debug)
    }

    /// Whether any debug option is on.
    #[inline]
    pub(crate) fn debug_on(&self) -> bool {
        matches!(self.extras.as_deref(), Some(extras) if extras.debug.is_some())
    }

    /// The debug state, while any debug option is on.
    #[inline]
    pub(crate) fn debug_mut(&mut self) -> Option<&mut DebugState> {
        self.extras.as_deref_mut()?.debug.as_deref_mut()
    }

    /// The log's entries, while the log is on.
    pub(crate) fn debug_ring(&self) -> Option<&[DebugEntry]> {
        self.extras
            .as_deref()?
            .debug
            .as_deref()
            .and_then(DebugState::ring_entries)
    }

    /// The marker of this actor's task's own poll (see
    /// [`runtime::TaskMarker`]).
    pub(crate) fn task_marker(&self) -> runtime::TaskMarker {
        self.task_marker.clone()
    }

    /// A handle to the actor's turn clock.
    pub(crate) fn turn_clock(&self) -> TurnClock {
        self.turn_clock.clone()
    }

    /// Sets a label for this actor, shown in a system snapshot.
    pub fn set_label(&self, label: impl Into<String>) {
        crate::telemetry::set_label(
            self.stop_lane().abort_record(),
            self.system_in_hand(),
            &self.actor_id,
            Arc::from(label.into()),
        );
    }

    // - Identity & status --------------------------------------------------

    /// Returns the unique identifier of this actor.
    pub fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    /// This incarnation as the owner of the drops its log lines name: its
    /// id, with the sensitivity its record carries.
    pub(crate) fn owner(&self) -> runtime::Owner {
        runtime::Owner::of(&self.actor_id, self.stop_lane().abort_record())
    }

    /// Returns the registered name of this actor, if any.
    pub fn actor_name(&self) -> Option<&str> {
        self.name.as_deref()
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

    /// The actor's own handle, borrowed, for a read that needs no clone of it.
    pub(crate) fn self_handle_ref(&self) -> &ActorHandle<A> {
        &self.self_handle
    }

    /// Stops this actor with `reason`, from inside one of its callbacks.
    ///
    /// The request goes onto the actor's own stop lane exactly as
    /// [`ActorHandle::stop`](crate::actor::handle::ActorHandle::stop) writes
    /// it, so one reason gets one outcome by either route: a
    /// [`Graceful`](StopReason::Graceful) or [`Shutdown`](StopReason::Shutdown)
    /// request passes the actor's own `pre_stop`, which may veto it, a pending
    /// request of a stronger reason (a parent's
    /// [`ParentRequest`](StopReason::ParentRequest), a failure) is never
    /// replaced by this one, and the actor ends after the callback returns,
    /// ahead of any queued message.
    ///
    /// When called from `handle` while a `send` is waiting for its reply, the
    /// reply the handler returns is held and goes out once the actor's
    /// `on_stopped` has returned and its name is free, before its reason is
    /// written, as the reply of `{stop, Reason, Reply, State}` follows the
    /// `terminate/2` call in an Erlang/OTP `gen_server`. If the stop is
    /// vetoed by `pre_stop`, the actor keeps running and the reply is sent at
    /// once. A `Kill` that ends the actor first, or an abort, gives the caller
    /// `AskError::ResponseDropped` with `Kill`.
    ///
    /// The reply is sent after the actor's children have stopped, as Erlang/OTP
    /// replies after `terminate/2`, which for a supervisor stops its children
    /// first. A child that awaits this reply inside its own callback therefore
    /// waits within its own `Shutdown` bound before it is stopped, and without
    /// a bound under `Shutdown::Infinity`, as in OTP.
    ///
    /// Erlang/OTP: `{stop, Reason, State}` and `{stop, Reason, Reply, State}`
    /// returned from a callback; `StopReason::Shutdown` is the
    /// `{shutdown, Term}` reason.
    ///
    /// # Errors
    /// - [`StopError::KillRefused`] if `reason` is [`StopReason::Kill`]; a
    ///   `Kill` is raised through a handle or the system, which arm its
    ///   backstop. Nothing is written.
    /// - [`StopError::Closed`] if the stop lane takes no such request any
    ///   more: the actor's task has ended or the lane is sealed. A stop made
    ///   from `on_stopped` returns `Ok` and changes nothing, since the lane
    ///   narrows only after the hook returns. Nothing is written on an
    ///   error.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_actors::{
    ///     actor::{Actor, ActorExt, context::ActorContext},
    ///     ActorResult, StopReason,
    /// };
    ///
    /// struct Job;
    ///
    /// enum Msg {
    ///     Finish,
    /// }
    ///
    /// impl Actor for Job {
    ///     type Message = Msg;
    ///     type Response = &'static str;
    ///
    ///     async fn handle(
    ///         &mut self,
    ///         msg: Msg,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<&'static str> {
    ///         match msg {
    ///             Msg::Finish => {
    ///                 ctx.stop(StopReason::Shutdown("finished".into()))
    ///                     .expect("the message loop is running");
    ///                 Ok("done")
    ///             }
    ///         }
    ///     }
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let job = Job.spawn().await.unwrap();
    /// assert_eq!(job.send(Msg::Finish).await.unwrap(), "done");
    /// assert_eq!(
    ///     job.wait_stopped().await,
    ///     StopReason::Shutdown("finished".into())
    /// );
    /// # }
    /// ```
    pub fn stop(&mut self, reason: StopReason) -> Result<(), StopError> {
        if matches!(reason, StopReason::Kill) {
            return Err(StopError::KillRefused);
        }
        // The actor's own stop, never a `Kill`: refused above, so no
        // deadline is owed and no abort is armed.
        let raised = crate::system::raise_noted(
            self.system_in_hand(),
            self.stop_lane(),
            reason,
            crate::telemetry::StopOrigin::Requested,
        );
        if raised == RaiseResult::Refused {
            return Err(StopError::Closed);
        }
        self.stop_lane().abort_record().mark_self_stop();
        Ok(())
    }

    /// Ends the supervisor's start phase, in which a strict start keeps the
    /// acknowledgment of each child spawned. Tells whether the phase was on.
    pub(crate) fn end_init_phase(&mut self) -> bool {
        self.supervision
            .as_deref_mut()
            .is_some_and(|sup| std::mem::take(&mut sup.init_phase))
    }

    /// The actor's own stop lane, borrowed from its handle: the run loop reads
    /// and narrows it at its start and its end without cloning the handle.
    pub(crate) fn stop_lane(&self) -> &StopLane {
        self.self_handle.stop_lane_ref()
    }

    /// The identity fields of this actor's log statements.
    #[cfg(feature = "tracing")]
    pub(crate) fn log_fields(&self) -> crate::telemetry::LogFields<'static> {
        crate::telemetry::log_fields(self.stop_lane().abort_record(), self.system_in_hand())
    }

    /// Records a failure that occurred during message processing.
    pub fn record_failure(&mut self, error: ActorError) {
        self.last_error = Some(error);
    }

    /// Returns the last error recorded by this actor, if any.
    pub fn last_error(&self) -> Option<&ActorError> {
        self.last_error.as_ref()
    }

    /// Returns the most recent panic from a poll of a timer or stream
    /// forwarder task, if any. Such a panic never stops this actor - the
    /// message loop keeps running - so this is the only way to observe one
    /// after the fact.
    pub fn last_forwarder_error(&self) -> Option<&ActorError> {
        self.ex().last_forwarder_error.as_ref()
    }

    /// Returns the most recent panic of a task the crate runs to supervise
    /// this actor's children (the task that stops a child or restarts it), if
    /// any. Such a panic never stops this actor and is never recorded by
    /// [`last_error`](Self::last_error); the child's own end is still
    /// reported through `on_child_stopped` with the reason it ended with.
    pub fn last_supervision_task_error(&self) -> Option<&ActorError> {
        self.ex().last_supervision_task_error.as_ref()
    }

    /// Accounts for a death-plane task that failed instead of completing,
    /// and records its panic, if it panicked. Returns the id of the
    /// replacement task when the incarnation it follows had ended already.
    pub(crate) fn lost_death_task(&mut self, err: JoinError) -> Option<tokio::task::Id> {
        let (panic, replacement) = match self.supervision.as_deref_mut() {
            Some(sup) => sup.lost_death_task(err),
            None => (None, None),
        };
        if let Some(panic) = panic {
            self.ex_mut().last_supervision_task_error = Some(panic);
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
        let (outcome, panic, replacement) = match self.supervision.as_deref_mut() {
            Some(sup) => sup.lost_restart_task(err),
            None => (None, None, None),
        };
        if let Some(panic) = panic {
            self.ex_mut().last_supervision_task_error = Some(panic);
        }
        (outcome, replacement)
    }

    /// Returns the current lifecycle status of the actor.
    pub fn status(&self) -> ActorStatus {
        self.status_tx.status()
    }

    pub(crate) fn set_status(&mut self, status: ActorStatus) {
        // This write must succeed even if every `ActorHandle` (and therefore
        // every reader of the plane) has already been dropped. The plane's
        // reason is never touched here.
        self.status_tx.send_if_modified(|value| {
            value.status = status;
            true
        });
    }

    // - Supervision --------------------------------------------------------

    /// Starts a restart attempt of the child `child_id` as the incarnation
    /// `seq` (see [`SupervisionState::initiate`]) and, when the supervisor's
    /// system counts attempts, keeps the attempt's ticket until its outcome
    /// is taken. Returns whether the attempt started.
    pub(crate) fn initiate_attempt(
        &mut self,
        child_id: &ActorId,
        seq: u64,
        origin: Origin,
    ) -> bool {
        let ticket = AttemptTicket::begin(
            self.self_handle.stop_lane_ref().abort_record(),
            self.system.as_deref(),
            &self.actor_id,
        );
        let Some(sup) = self.supervision.as_deref_mut() else {
            return false;
        };
        let started = sup.initiate(child_id, seq, origin);
        match (started, ticket) {
            (true, Some(ticket)) => sup.note_attempt(child_id, seq, ticket),
            // A refused start is no attempt: its count falls with no event.
            (false, Some(ticket)) => ticket.discard(),
            (_, None) => {}
        }
        started
    }

    pub(crate) fn supervision_mut(&mut self) -> Option<&mut SupervisionState> {
        self.supervision.as_deref_mut()
    }

    pub(crate) fn supervision_ref(&self) -> Option<&SupervisionState> {
        self.supervision.as_deref()
    }

    /// Whether a final delivery of `on_child_stopped` is running: the
    /// supervisor is stopping, and its supervision calls are refused.
    fn supervisor_stopping(&self) -> bool {
        self.supervision
            .as_deref()
            .is_some_and(|sup| sup.final_delivery)
    }

    /// Takes the supervision state out of the context, leaving `None`
    /// behind. Used only by the run loop's Kill cascade-inversion path:
    /// dropping the returned value directly aborts every live child's
    /// runner task without this actor ever awaiting any of them (see the
    /// `runtime` module docs).
    pub(crate) fn take_supervision(&mut self) -> Option<SupervisionState> {
        self.supervision.take().map(|b| *b)
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
        Option<&mut JoinSet<ForwarderEnd>>,
    ) {
        let planes = self
            .supervision
            .as_mut()
            .map(|sup| (&mut sup.restart_set, &mut sup.death_set, &sup.verdicts));
        (
            planes,
            self.extras
                .as_deref_mut()
                .map(|extras| &mut extras.forwarders),
        )
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
    /// Whether no entry of the forwarder plane can be ready and the plane
    /// holds the task's waker wherever the select's poll would leave one:
    /// every forwarder end counted so far has had its entry taken, and the
    /// select has polled the plane since the last spawn and the last reap.
    pub(crate) fn forwarders_idle(&self) -> bool {
        let Some(extras) = self.extras.as_deref() else {
            return true;
        };
        if extras.forwarders_unpolled {
            return false;
        }
        match &extras.forwarders_done {
            None => true,
            Some(done) => {
                let done = done.load(Ordering::Acquire);
                debug_assert!(
                    extras.forwarders_taken <= done,
                    "a forwarder entry was taken before its end was counted"
                );
                done == extras.forwarders_taken
            }
        }
    }

    /// The run loop's select is polling the forwarder plane.
    pub(crate) fn note_forwarders_polled(&mut self) {
        if let Some(extras) = self.extras.as_deref_mut() {
            extras.forwarders_unpolled = false;
        }
    }

    pub(crate) fn reap_forwarder(
        &mut self,
        outcome: Result<(tokio::task::Id, ForwarderEnd), JoinError>,
    ) -> Option<String> {
        {
            let extras = self.ex_mut();
            extras.forwarders_taken += 1;
            extras.forwarders_unpolled = true;
        }
        let (task_id, panic) = match outcome {
            Ok((id, ForwarderEnd::Done)) => (id, None),
            Ok((id, ForwarderEnd::Panicked(payload))) => (id, Some(payload)),
            // Read as the run's wherever it is reaped here; the end
            // classifies an end drop itself before reaping.
            Ok((id, ForwarderEnd::Released { run, end })) => match (end, run) {
                (Some(end), Some(run)) => {
                    #[cfg(feature = "tracing")]
                    let fields = self.log_fields();
                    #[cfg(feature = "tracing")]
                    caught_log!(warn,
                        actor_id = %self.actor_id(),
                        actor_name = fields.name(),
                        incarnation = fields.incarnation(),
                        system = fields.system(),
                        panic = %fields.panic_text(&run),
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
        if let Some(kind) = self.ex_mut().forwarder_kinds.remove(&task_id) {
            match kind {
                ForwarderKind::Timer(id) => {
                    self.ex_mut().timers.remove(&id);
                }
                ForwarderKind::Stream(id) => {
                    self.ex_mut().streams.remove(&id);
                }
                // A monitor stays registered after its task ends, until its
                // notice is dispatched or the monitor is revoked, so that a
                // revoke can still reach a notice that is queued. Only a
                // task that ended by a panic has no notice left to wait for.
                ForwarderKind::Monitor(id) => {
                    if panic.is_some() {
                        if let Some(state) = self.monitors.as_deref_mut() {
                            state.remove(id);
                        }
                    }
                }
                // A link stays in the table after its task ends only while
                // its exit is queued or the stop-time stage owes it.
                ForwarderKind::Link(id) => {
                    if let Some(state) = self.monitors.as_deref_mut() {
                        state.links().reaped(id, panic.is_some());
                    }
                }
                // A group monitor stays registered after its task ends, until
                // it is revoked or the actor ends; only a task that ended by
                // a panic leaves nothing to deliver.
                ForwarderKind::Group(id) => {
                    if panic.is_some() {
                        if let Some(state) = self.monitors.as_deref_mut() {
                            state.groups.remove(id);
                        }
                    }
                }
                // A request stays in its table after its task ends, until
                // its reply is dispatched or it is cancelled, so that a
                // cancel can still reach a reply that is queued. Only a task
                // that ended by a panic has no reply left to wait for.
                ForwarderKind::Request(id) => {
                    if panic.is_some() {
                        if let Some(state) = self.monitors.as_deref_mut() {
                            state.requests.remove(id);
                        }
                    }
                }
                ForwarderKind::Subscription(id) => {
                    self.ex_mut().subscriptions.remove(&id);
                }
            }
        }
        if let Some(payload) = &panic {
            #[cfg(feature = "tracing")]
            let fields = self.log_fields();
            #[cfg(feature = "tracing")]
            caught_log!(warn,
                actor_id = %self.actor_id(),
                actor_name = fields.name(),
                incarnation = fields.incarnation(),
                system = fields.system(),
                panic = %fields.panic_text(payload),
                "a timer or stream of the actor panicked"
            );
            self.ex_mut().last_forwarder_error = Some(ActorError::Panic(payload.clone()));
        }
        panic
    }

    /// Whether the timer or stream a forwarder belongs to is still
    /// registered, that is, neither cancelled nor reaped.
    fn is_registered(&self, kind: &ForwarderKind) -> bool {
        match kind {
            ForwarderKind::Timer(id) => self.ex().timers.contains_key(id),
            ForwarderKind::Stream(id) => self.ex().streams.contains_key(id),
            ForwarderKind::Monitor(id) => self
                .monitors
                .as_deref()
                .is_some_and(|state| state.contains(*id)),
            ForwarderKind::Link(id) => self
                .monitors
                .as_deref()
                .is_some_and(|state| state.holds_link(*id)),
            ForwarderKind::Group(id) => self
                .monitors
                .as_deref()
                .is_some_and(|state| state.groups.contains(*id)),
            ForwarderKind::Request(id) => self
                .monitors
                .as_deref()
                .is_some_and(|state| state.requests.contains(*id)),
            ForwarderKind::Subscription(id) => self.ex().subscriptions.contains_key(id),
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
        let done = self
            .ex_mut()
            .forwarders_done
            .get_or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        let forwarder = Forwarder {
            done,
            counted: false,
            future: Some(Box::pin(fut)),
            slot,
            owner: self.owner(),
            _status: self.status_tx.clone(),
            _subtree: self.subtree.clone(),
        };
        let runtime = self.runtime.clone();
        let extras = self.ex_mut();
        let abort = extras.forwarders.spawn_on(forwarder, &runtime);
        extras.forwarders_unpolled = true;
        extras.forwarder_kinds.insert(abort.id(), kind);
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
        self.extras.as_ref()?;
        let registered: HashSet<tokio::task::Id> = self
            .ex()
            .forwarder_kinds
            .iter()
            .filter(|(_, kind)| self.is_registered(kind))
            .map(|(id, _)| *id)
            .collect();
        self.ex_mut().forwarders.abort_all();
        let owner = self.owner();
        let mut first = None;
        while let Some(joined) = self.ex_mut().forwarders.join_next_with_id().await {
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

    /// The system the actor was spawned with, if the spawn named one.
    pub(crate) fn system_in_hand(&self) -> Option<&ActorSystem> {
        self.system.as_deref()
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
    /// this does not await the child's `pre_start`/`on_started` ack by
    /// default: the runner and restart budget already give the supervisor
    /// full, asynchronous visibility into a child's init failure, and
    /// synchronously awaiting that ack from inside the parent's own callback
    /// would invite the same self-call deadlock documented on
    /// [`ActorHandle::send`](crate::actor::handle::ActorHandle::send).
    ///
    /// By default, a child whose first `pre_start` or `on_started` fails ends
    /// with `StopReason::Failure` and is restarted under the budget like any
    /// crash (a `Temporary` child is removed), while the supervisor's own start
    /// succeeds, where an Erlang/OTP supervisor whose child fails to start
    /// stops the children already started and fails its own start. To start in
    /// order, spawn each child in `on_started` and await its `get_status()`,
    /// which the child answers only after its `pre_start` and `on_started`; a
    /// child that calls back into the supervisor and awaits the answer during
    /// its initialization then waits forever, as an Erlang/OTP child does when
    /// its call to the supervisor has an infinite timeout, and a
    /// `start_timeout` on the supervisor's own spawn ends the start with
    /// [`SpawnError::StartTimeout`]. With `?` on each `get_status()` (each
    /// error mapped with `ActorError::user(e.to_string())`), the supervisor's
    /// `on_started` returns the error, its awaited spawn returns
    /// `SpawnError::Init`, and its teardown stops the children already started.
    /// [`with_strict_start`](crate::actor::supervision::SupervisionConfig::with_strict_start)
    /// does the same wait for every child spawned while the supervisor starts,
    /// and [`await_init`](ChildSpawnBuilder::await_init) awaits one child's
    /// initialization.
    ///
    /// Because this start is not awaited, a `Kill` of the child that lands
    /// before its task first runs ends that incarnation before any start hook
    /// begins, and the child then follows its restart type like after any
    /// other `Kill`.
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
    /// kill grace, and for a worker that has called `trap_exit` its own
    /// `Shutdown` window as well), each followed by a short settle time, plus
    /// one more settle time, and unbounded only while a supervisor child under
    /// [`Shutdown::Infinity`] runs in it, or a worker under `Infinity` that
    /// traps exit signals, or a budget is too large to represent. Past that
    /// bound, the wait gives no verdict on an actor before the abort the system
    /// owes it has had the settle time to end it: an abort that is late holds
    /// the wait, for at most the bound once more plus one more settle time.
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
    ///   if the supervisor holds a child spec under this name, whatever the
    ///   child is doing: it runs (one whose death the supervisor has not
    ///   handled yet included), it is being restarted, or it is stopped and
    ///   its spec is kept until [`delete_child`](Self::delete_child) removes
    ///   it ([`terminate_child`](Self::terminate_child) says which stopped
    ///   children keep their spec). The factory is never called. A call made
    ///   inside [`on_child_stopped`](crate::actor::Actor::on_child_stopped)
    ///   while the supervisor itself is stopping is answered
    ///   [`SpawnError::SupervisorStopping`] first. Erlang/OTP answers
    ///   `{already_started, Pid}` for a running child, or one whose exit the
    ///   supervisor has not processed yet, and `already_present` for the
    ///   others, a child being restarted included; this call answers
    ///   `DuplicateChild` for all of them, and [`children`](Self::children)
    ///   lists the specs the supervisor holds.
    /// - [`SpawnError::ChildUnresponsive`]
    ///   if an actor of a deleted or removed child under this id, or one that
    ///   child spawned, did not end within the bound of that subtree.
    /// - [`SpawnError::InvalidSupervision`]
    ///   if the child is significant under [`AutoShutdown::Never`] or with
    ///   restart type `Permanent`, or its own supervision sets an
    ///   [`AutoShutdown`] mode under `SimpleOneForOne`. The factory is never
    ///   called.
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
            runtime: None,
            significant: false,
            await_init: false,
            sys: SysOptions::default(),
            _child: std::marker::PhantomData,
        }
    }

    /// The refusals of a spawn that need no wait and change nothing, in the
    /// order every spawn answers them: not a supervisor, the supervisor is
    /// stopping, the name is taken. An unnamed spawn has no name to take.
    fn refuse_spawn(&self, id: Option<&ActorId>) -> Result<(), SpawnError> {
        let Some(sup) = self.supervision.as_deref() else {
            return Err(SpawnError::NotASupervisor);
        };
        if sup.final_delivery {
            return Err(SpawnError::SupervisorStopping);
        }
        match id {
            Some(id) if sup.registry.get(id).is_some() => {
                Err(SpawnError::DuplicateChild(id.clone()))
            }
            _ => Ok(()),
        }
    }

    /// The refusals of a spawn that come from combining options Erlang/OTP
    /// refuses, answered before any factory runs: a significant child under
    /// `AutoShutdown::Never`, a significant `Permanent` child, and a child
    /// that is itself a supervisor with `auto_shutdown` set under
    /// `SimpleOneForOne`. A call that another refusal answers first (not a
    /// supervisor, stopping) passes here and meets that refusal next. The
    /// refusal of the child's own configuration raises the spawn-refused
    /// event, as the same refusal of a root spawn does; the other two come
    /// from options with no root counterpart and raise none.
    fn refuse_setup(
        &self,
        name: Option<&str>,
        significant: bool,
        restart_type: RestartType,
        child_config: Option<&runtime::ActorConfig>,
    ) -> Result<(), SpawnError> {
        let Some(sup) = self.supervision.as_deref() else {
            return Ok(());
        };
        if sup.final_delivery {
            return Ok(());
        }
        if let Some(config) = child_config {
            runtime::supervision_setup_error(config)
                .map_err(|err| runtime::refused(self.system.as_ref(), name, err))?;
        }
        if significant {
            if sup.config.auto_shutdown == AutoShutdown::Never {
                return Err(SpawnError::InvalidSupervision(
                    "significant child under auto_shutdown never",
                ));
            }
            if restart_type == RestartType::Permanent {
                return Err(SpawnError::InvalidSupervision(
                    "significant child with restart permanent",
                ));
            }
        }
        Ok(())
    }

    /// The wait of a spawn under the id of a removed child: until every actor
    /// the removed child's last incarnation spawned has ended. The refusals
    /// that need no wait are answered first and in their own order, so a
    /// refused call never reads, prunes or waits on the removed children; a
    /// spawn under an id nobody removed completes without waiting.
    async fn await_retired_subtree(&mut self, name: Option<&Arc<str>>) -> Result<(), SpawnError> {
        let Some(name) = name else {
            return Ok(());
        };
        let id = ActorId::from_shared(Arc::clone(name));
        self.refuse_spawn(Some(&id))?;
        let Some(sup) = self.supervision.as_deref_mut() else {
            return Err(SpawnError::NotASupervisor);
        };
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
        name: Option<Arc<str>>,
        restart_type: RestartType,
        shutdown: Shutdown,
        config: Option<runtime::ActorConfig>,
        start_timeout: Option<Duration>,
        kill_grace: Duration,
        placement: Option<Handle>,
        flags: ChildFlags,
        sys: SysOptions,
    ) -> Result<(ActorHandle<C>, Option<InitWait>), SpawnError>
    where
        F: Fn() -> C + Send + Sync + 'static,
        C: Actor,
    {
        // A spec the supervisor holds under this id, whatever its child is
        // doing (running, being restarted, or stopped with the spec kept),
        // rejects the spawn before the factory ever runs: OTP answers
        // `already_started` or `already_present` there. `delete_child` clears
        // the spec so the id can be reused.
        let named_id = name
            .as_ref()
            .map(|name| ActorId::from_shared(Arc::clone(name)));
        let sensitive = sys.effective_sensitive::<C>();
        let child_id: ActorId = match named_id.as_ref() {
            Some(named) => named.clone(),
            None => uuid::Uuid::new_v4().to_string().into(),
        };
        if let Err(err) = self.refuse_spawn(named_id.as_ref()) {
            report_refused(&self.actor_id, named_id.as_ref(), &err, sensitive);
            return Err(err);
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
        let subtree_trap_extra = rules::trap_extra(is_supervisor, shutdown);
        let subtree_trap_infinity = rules::trap_infinity(is_supervisor, shutdown);
        let subtree_parent = self.subtree.token();

        // The runtime every incarnation of this child runs on, its link guard
        // and its reaper too: the one the builder placed it on, else the
        // supervisor's. Resolved once, so a restart lands where the first
        // incarnation did.
        let guard_runtime = placement.unwrap_or_else(|| self.runtime_handle().clone());
        // The first start of a child is acknowledged to nobody, except when
        // the call awaits it or the supervisor's strict start waits for it:
        // then the child's acknowledgment goes to a receiver kept for that.
        let strict = !flags.await_init && self.supervision.as_deref().is_some_and(|s| s.init_phase);
        let (ack_tx, mut ack_rx) = if flags.await_init || strict {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        // Every registration - this initial spawn included - draws its
        // incarnation token from the system's single monotonic counter, so
        // no two incarnations of any child, under any name, ever collide
        // (see `ActorSystem::next_incarnation`).
        let incarnation = self.resolved_system().next_incarnation();
        let (child_handle, join_handle, child_subtree) = runtime::spawn_actor_in(
            child_id.clone(),
            actor,
            child_config.clone(),
            name.clone(),
            self.system.clone(),
            ack_tx,
            false,
            kill_grace,
            Some(SubtreeLink {
                parent: subtree_parent.clone(),
                own: subtree_own,
                infinity: subtree_infinity,
                trap_extra: subtree_trap_extra,
                trap_infinity: subtree_trap_infinity,
            }),
            Some(&guard_runtime),
            Birth {
                incarnation: Some(incarnation),
                parent: Some((
                    &self.actor_id,
                    self.stop_lane().abort_record().incarnation().number(),
                )),
                publish: None,
            },
            sys,
        )
        .inspect_err(|err| {
            reports::start_error(
                &self.actor_id,
                &child_id,
                Some(incarnation),
                || StopReason::Failure(ActorError::Spawn(err.clone())),
                sensitive,
            );
        })?;

        // The parent spawns and owns the runner of the initial incarnation,
        // together with its link guard - constructed here, in the parent's
        // own synchronous code, before the runner task is spawned, so an
        // abort landing before the runner is ever polled still fires it.
        // The abort handle is taken first - the Kill escalation backstop.
        let abort = join_handle.abort_handle();
        let resolved_system = self.resolved_system();
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
        // Named before the first runner exists, so every runner lands its
        // death on the supervisor's record.
        sup.registry
            .adopt_owner(self.self_handle.stop_lane_ref().abort_record());
        let owner = sup.registry.owner_record();
        runtime::claim_death(
            owner.as_ref(),
            &child_id,
            incarnation,
            child_handle.stop_lane_ref().abort_record(),
        );
        let task = runtime::spawn_runner(
            &mut sup.death_set,
            child_id.clone(),
            incarnation,
            join_handle,
            None,
            child_handle.stop_lane(),
            io,
            guard,
            kill_grace,
            owner,
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
                significant: flags.significant,
                // The spec records whether the child is sensitive by its
                // type or by the spawn, which the supervisor's reports read.
                sys: SysOptions { sensitive, ..sys },
            },
            ctl,
            lifecycle: ChildLifecycle::Running(incarnation),
            origin: Origin::Initial,
            predecessor: Some(predecessor),
        };
        let wait = if flags.await_init {
            ack_rx.take().map(|ack| InitWait {
                ack,
                child_id: child_id.clone(),
                incarnation,
                lane: child_handle.stop_lane(),
                abort: abort.clone(),
            })
        } else {
            None
        };

        // The supervisor's id, which an attempt's log lines name, and its
        // incarnation number, which every restarted incarnation's registry
        // entry names with the id.
        let restart_supervisor = self.actor_id().clone();
        let restart_supervisor_seq = self.stop_lane().abort_record().incarnation().number();
        let sup = self
            .supervision
            .as_mut()
            .expect("supervision presence checked above");
        sup.registry
            .adopt_owner(self.self_handle.stop_lane_ref().abort_record());
        sup.registry.register(child_state);
        if strict {
            if let Some(ack) = ack_rx {
                sup.push_strict_ack(child_id.clone(), incarnation, ack);
            }
        }

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
        // The `sys` options every fresh incarnation is spawned with, as the
        // initial spawn was.
        let restart_sys = sys;
        let restart_sensitive = sensitive;
        let restart_runtime = guard_runtime;

        reports::progress(
            &self.actor_id,
            &child_id,
            incarnation,
            reports::Progress::Started,
        );
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
                    let sys = restart_sys;
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

                        let actor = match build_for_attempt_of(
                            &factory,
                            &io,
                            &supervisor,
                            &child_id,
                            seq,
                            restart_sensitive,
                        ) {
                            Ok(actor) => actor,
                            Err(outcome) => return *outcome,
                        };

                        let (ack_tx, ack_rx) = oneshot::channel();
                        let (handle, join, _) = match runtime::spawn_actor_in(
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
                                trap_extra: subtree_trap_extra,
                                trap_infinity: subtree_trap_infinity,
                            }),
                            Some(&runtime),
                            Birth {
                                incarnation: Some(seq),
                                parent: Some((&supervisor, restart_supervisor_seq)),
                                publish: Some(PublishTo {
                                    io: &io,
                                    own: &own_stop,
                                }),
                            },
                            sys,
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

                        // The spawn published the incarnation to the slot before
                        // its task could run. A test holds the attempt here, so
                        // the incarnation can run to its end before the attempt
                        // goes on.
                        #[cfg(test)]
                        runtime::end_hooks::wait_hold(runtime::end_hooks::SPAWNED, &child_id).await;

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

        Ok((child_handle, wait))
    }

    /// The wait of a spawn with `await_init`, after the child is registered:
    /// the child's initialization, bounded by `start_timeout` when one is set.
    /// A child that did not start is discarded: its start is cut when the bound
    /// passed, its spec is removed and its death report is silenced, so the
    /// supervisor neither calls `on_child_stopped` nor charges its budget.
    async fn finish_await_init<C: Actor>(
        &mut self,
        handle: ActorHandle<C>,
        wait: InitWait,
        start_timeout: Option<Duration>,
    ) -> Result<ActorHandle<C>, SpawnError> {
        let InitWait {
            mut ack,
            child_id,
            incarnation,
            lane,
            abort,
        } = wait;
        let end = match start_timeout {
            None => init_end((&mut ack).await),
            Some(bound) => {
                let deadline = runtime::saturating_deadline(Instant::now(), bound);
                bounded_ack(&mut ack, time::sleep_until(deadline), || {
                    crate::actor::start_bound::cut_start(&lane, &abort);
                })
                .await
            }
        };
        let error = match end {
            InitEnd::Started => return Ok(handle),
            InitEnd::Failed(err) => SpawnError::Init(Box::new(err)),
            InitEnd::TimedOut => SpawnError::StartTimeout,
            InitEnd::Closed => {
                let mut status = handle.status_rx();
                match crate::system::wait_final_reason(&mut status).await {
                    StopReason::Failure(err) => SpawnError::Init(Box::new(err)),
                    _ => SpawnError::Killed,
                }
            }
        };
        // The child keeps no spec and owes no report: its end is dropped when
        // it arrives. A start under the same id waits for the failed
        // incarnation's subtree, which the removal keeps under the id.
        let mut factory = None;
        if let Some(sup) = self.supervision_mut() {
            sup.silenced.push((child_id.clone(), incarnation));
            factory = sup.remove_spec(&child_id);
        }
        // The factory is user code: dropped as the call's last step, a panic
        // in its drop is a panic of the callback that called it.
        drop(factory);
        Err(error)
    }

    /// Returns introspection info for all supervised children. To count the
    /// children (all of them, and the live ones through `is_alive`, as
    /// Erlang/OTP's `supervisor:count_children` does for specs and active) or
    /// to find one by id, as a scan of `supervisor:which_children` does, use
    /// this list; its entries also carry the name, restart type and `Shutdown`,
    /// and whether the child is itself a supervisor (`is_supervisor`).
    ///
    /// A child is alive while it has a started incarnation whose stop, if
    /// any, has not completed, and its restart is pending while an attempt is
    /// in flight, while its death awaits its restart decision, and while a
    /// pending group restart will restart it.
    pub fn children(&self) -> Vec<ChildInfo> {
        self.supervision
            .as_deref()
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
    /// grace, a supervisor child's `Shutdown` window plus its kill grace, and
    /// for a worker that has called `trap_exit` its own `Shutdown` window as
    /// well), each followed by a short settle time, plus one more settle time,
    /// and unbounded only while a supervisor child under [`Shutdown::Infinity`]
    /// runs in it, or a worker under `Infinity` that traps exit signals, or a
    /// budget is too large to represent. Past that bound, the wait gives no
    /// verdict on an actor before the abort the system owes it has had the
    /// settle time to end it: an abort that is late holds the wait, for at most
    /// the bound once more plus one more settle time. If the subtree does not
    /// end within that wait, no new incarnation starts and the supervisor stops
    /// with [`SupervisionError::ChildUnresponsive`], so that its own supervisor
    /// decides.
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
    /// for good or released it). A child that then outlasts its stop carries
    /// the Unresponsive verdict with no caller left to report it, and the
    /// supervisor stops with [`SupervisionError::ChildUnresponsive`] for it.
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
    /// free. Where the spec is kept, [`delete_child`](Self::delete_child)
    /// succeeds afterward, even from the very same handler invocation that
    /// awaited this call, and a same-name [`spawn_child`](Self::spawn_child)
    /// made after the `delete_child` succeeds; made before it, `spawn_child`
    /// is refused with [`SpawnError::DuplicateChild`] (OTP `already_present`).
    /// Where the call removes the spec (a `Temporary` child, or a child of a
    /// `SimpleOneForOne` supervisor), a same-name `spawn_child` succeeds at
    /// once and `delete_child` answers [`SupervisionError::ChildNotFound`].
    ///
    /// Unlike [`stop_child`](Self::stop_child), this call also waits for the
    /// child's subtree: it returns once the child and every actor it spawned
    /// have ended, a child already stopped whose actors are still ending
    /// included. The wait is bounded by the longest chain of stop budgets in
    /// the subtree (a worker's kill grace, a supervisor child's `Shutdown`
    /// window plus its kill grace, and for a worker that has called `trap_exit`
    /// its own `Shutdown` window as well), each followed by a short settle
    /// time, plus one more settle time, and unbounded only while a supervisor
    /// child under [`Shutdown::Infinity`] runs in it, or a worker under
    /// `Infinity` that traps exit signals, or a budget is too large to
    /// represent. Past that bound, the wait gives no verdict on an actor before
    /// the abort the system owes it has had the settle time to end it: an abort
    /// that is late holds the wait, for at most the bound once more plus one
    /// more settle time. If an actor never yields, the call returns
    /// [`SupervisionError::ChildUnresponsive`] and the child stays stopped;
    /// once its spec is gone, a [`spawn_child`](Self::spawn_child) under the
    /// same id waits for that subtree again.
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
    /// latency note) match [`stop_child`](Self::stop_child). A child configured
    /// with [`Shutdown::Infinity`] makes this call wait without bound:
    /// Erlang/OTP asks for `infinity` on a child that is itself a supervisor,
    /// whose own subtree can legitimately take an unbounded time to unwind, and
    /// allows it on a worker with a warning. This crate accepts it on any
    /// child, and a leaf actor configured with `Infinity` is a risk it does not
    /// guard against. A child still running its first initialization is stopped
    /// bounded by its `Shutdown` from the call; one whose initialization waits
    /// on this supervisor deadlocks this call unless that bound ends it (under
    /// `Infinity` nothing does: the hazard `pre_stop` already has inside a
    /// handler).
    ///
    /// Cancel safety: the escalation belongs to the child's own runner.
    /// Abandoning this future at any point, for example with
    /// `tokio::time::timeout`, still stops the child under its `Shutdown`
    /// policy, and calling it again while that stop is in progress waits for
    /// the same stop. The caller may bound its own wait and drop the future
    /// safely; a child that then outlasts its stop carries the Unresponsive
    /// verdict with no caller left to report it, and the supervisor stops with
    /// [`SupervisionError::ChildUnresponsive`] for it.
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
    /// supervisor child's `Shutdown` window plus its kill grace, and for a
    /// worker that has called `trap_exit` its own `Shutdown` window as well),
    /// each followed by a short settle time, plus one more settle time, and
    /// unbounded only while a supervisor child under [`Shutdown::Infinity`]
    /// runs in it, or a worker under `Infinity` that traps exit signals, or a
    /// budget is too large to represent. Past that bound, the wait gives no
    /// verdict on an actor before the abort the system owes it has had the
    /// settle time to end it: an abort that is late holds the wait, for at most
    /// the bound once more plus one more settle time. If the subtree does not
    /// end within that wait, no new incarnation starts and the supervisor stops
    /// with [`SupervisionError::ChildUnresponsive`], so that its own supervisor
    /// decides.
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
        self.initiate_attempt(&id, seq, Origin::Manual);
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
    /// plus its kill grace, and for a worker that has called `trap_exit` its
    /// own `Shutdown` window as well), each followed by a short settle time,
    /// plus one more settle time, and unbounded only while a supervisor child
    /// under [`Shutdown::Infinity`] runs in it, or a worker under `Infinity`
    /// that traps exit signals, or a budget is too large to represent. Past
    /// that bound, the wait gives no verdict on an actor before the abort the
    /// system owes it has had the settle time to end it: an abort that is late
    /// holds the wait, for at most the bound once more plus one more settle
    /// time.
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

    /// Stops a supervised child exactly as [`stop_child`](Self::stop_child)
    /// commits the stop, and returns at once: the commit without the wait
    /// for the child's end.
    ///
    /// The supervisor's record of the child moves, the first signal is raised
    /// and the stop order is written before this call returns, and the child's
    /// own runner escalates the stop under its `Shutdown` policy from there.
    /// The supervisor's death arm classifies the end when it arrives and reports
    /// it once to
    /// [`on_child_stopped`](crate::actor::Actor::on_child_stopped), as it
    /// does for a caller that dropped its `stop_child` future after the
    /// commit. The returned outcome is the one `stop_child` answers:
    /// [`StopChildOutcome::Stopped`] means the stop is committed, not that
    /// the child has ended, and [`StopChildOutcome::Absorbed`] means a
    /// pending group restart covers the child.
    ///
    /// The child's slot stays taken until the child and every actor it
    /// spawned have ended: [`delete_child`](Self::delete_child) refuses
    /// while the stop is in progress, and a [`spawn_child`](Self::spawn_child)
    /// under the same id is refused with `DuplicateChild` while the
    /// supervisor holds the spec (the spec of a `Temporary` child, and of any
    /// child of a [`SimpleOneForOne`](crate::RestartStrategy::SimpleOneForOne)
    /// supervisor, goes when the supervisor classifies its end, the spec of
    /// any other child stays until [`delete_child`](Self::delete_child)); once
    /// the spec is gone, a spawn under the id waits for the subtree. A
    /// supervisor never waits for a child this way, which Erlang/OTP's
    /// `terminate_child` does.
    ///
    /// A child that outlasts its whole stop carries the Unresponsive verdict
    /// and no caller waits for it, so this call cannot return it: the
    /// supervisor stops with [`SupervisionError::ChildUnresponsive`] for the
    /// child, as it does when a group restart meets such a child, and its
    /// own supervisor decides.
    ///
    /// # Errors
    /// The set of [`stop_child`](Self::stop_child), except
    /// [`SupervisionError::ChildUnresponsive`], which the supervisor's own
    /// stop reports as above.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, StopChildOutcome,
    /// };
    ///
    /// struct Worker;
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = bool;
    ///     async fn handle(&mut self, _: (), ctx: &mut ActorContext<Self>) -> ActorResult<bool> {
    ///         let child = ctx.spawn_child(|| Worker).named("worker").await?;
    ///         // Returns before the child has ended.
    ///         let outcome = ctx.begin_stop_child(child.id().clone())?;
    ///         Ok(outcome == StopChildOutcome::Stopped)
    ///     }
    /// }
    ///
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().supervisor().await?;
    /// assert!(boss.send(()).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn begin_stop_child(
        &mut self,
        child: impl Into<ActorId>,
    ) -> Result<StopChildOutcome, SupervisionError> {
        self.manual_stop_commit(child.into(), ManualStop::Bounce)
            .map(|committed| committed.outcome)
    }

    /// Stops a supervised child without restarting it, as
    /// [`terminate_child`](Self::terminate_child) commits the stop, and
    /// returns at once: the commit without the wait for the child's end.
    ///
    /// The spec is kept or removed as `terminate_child` keeps or removes it,
    /// once the supervisor's death arm classifies the end, which reaches
    /// [`on_child_stopped`](crate::actor::Actor::on_child_stopped) once as
    /// [`SupervisionAction::Removed`](crate::SupervisionAction::Removed).
    /// The child's slot stays taken until the child and every actor it
    /// spawned have ended, and a [`spawn_child`](Self::spawn_child) under the
    /// same id is refused or waits as for
    /// [`begin_stop_child`](Self::begin_stop_child).
    /// A `significant` child stopped this way never shuts its supervisor down,
    /// as the supervisor ordered its end.
    ///
    /// A child that outlasts its whole stop ends the supervisor with
    /// [`SupervisionError::ChildUnresponsive`], as for
    /// [`begin_stop_child`](Self::begin_stop_child).
    ///
    /// # Errors
    /// The set of [`terminate_child`](Self::terminate_child), except
    /// [`SupervisionError::ChildUnresponsive`], which the supervisor's own
    /// stop reports.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// struct Worker;
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let child = ctx.spawn_child(|| Worker).named("worker").await?;
    ///         ctx.begin_terminate_child(child.id().clone())?;
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().supervisor().await?;
    /// boss.send(()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn begin_terminate_child(
        &mut self,
        child: impl Into<ActorId>,
    ) -> Result<(), SupervisionError> {
        self.manual_stop_commit(child.into(), ManualStop::Terminate)
            .map(|_| ())
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
        let committed = self.manual_stop_commit(id.clone(), kind)?;
        let effective = committed.effective;
        let Some(JoinTicket {
            mut cell,
            incarnation,
        }) = committed.join
        else {
            return Ok((committed.outcome, effective));
        };
        match await_cell(&mut cell, incarnation).await {
            CellOutcome::Fate(fate) => {
                drop(self.finish_manual_stop(&id, incarnation, effective, fate.reason));
                Ok((StopChildOutcome::Stopped, effective))
            }
            CellOutcome::Unresponsive => {
                cell.mark_reported();
                Err(SupervisionError::ChildUnresponsive(id))
            }
            // No manual stop reaches an attempt (a child with a restart in
            // flight answers `ChildRestarting`), so there is nothing to
            // classify.
            CellOutcome::Cancelled | CellOutcome::NoIncarnation => {
                Ok((StopChildOutcome::Stopped, effective))
            }
        }
    }

    /// The synchronous half of a manual stop: every effect of the call's
    /// commit up to the wait on the child's end, with no await.
    ///
    /// The ledger moves to `Stopping{kind}`, the first signal is raised and
    /// the stop order is written to the child's runner, so a caller that
    /// stops here has still made the stop happen. `join` is the ticket for
    /// the wait that follows, present exactly where the awaited calls wait
    /// for the child's fate cell. A caller that holds no ticket leaves the
    /// classification of the end to the supervisor's death arm, which
    /// finishes it for any stop still `Stopping` when the death arrives.
    pub(crate) fn manual_stop_commit(
        &mut self,
        id: ActorId,
        kind: ManualStop,
    ) -> Result<Committed, SupervisionError> {
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
        let done = |outcome, join| {
            Ok(Committed {
                outcome,
                effective: kind,
                join,
            })
        };

        if let Some(to) = commit.to {
            if let Some(child) = self
                .supervision_mut()
                .and_then(|sup| sup.registry.get_mut(&id))
            {
                child.transition_logged(to);
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
                    let cell = self.with_child(&id, |child| child.ctl.watch());
                    drop(removed);
                    return done(
                        StopChildOutcome::Stopped,
                        Some(JoinTicket { cell, incarnation }),
                    );
                }
                CommitEffect::Answer(reply) => {
                    drop(removed);
                    return manual_reply(reply, &id).map(|outcome| Committed {
                        outcome,
                        effective: kind,
                        join: None,
                    });
                }
            }
        }
        drop(removed);
        done(StopChildOutcome::Stopped, None)
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

    /// Finishes the classification of a manual stop whose child ended, as the
    /// caller of an awaited stop does once the child's fate cell resolves.
    /// A stop the supervisor's death arm classified already is left as it is.
    pub(crate) fn settle_manual_stop(
        &mut self,
        id: &ActorId,
        incarnation: u64,
        kind: ManualStop,
        reason: StopReason,
    ) {
        // The factory of a spec the classification removed is dropped as the
        // call's last step: a panic in its drop is a panic of the caller.
        drop(self.finish_manual_stop(id, incarnation, kind, reason));
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
        let (restart_type, child_name) = sup.registry.get(id).map(|child| {
            (
                child.spec.restart_type,
                child.name.as_deref().map(str::to_string),
            )
        })?;
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
            child.transition_logged(ChildLifecycle::Down {
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
            self.initiate_attempt(id, seq, Origin::Bounce);
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
    /// # Panics
    /// Panics, before it registers anything, when this actor's runtime was
    /// built without the Tokio time driver.
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
        let message = require_timers_holding(
            "ActorContext::schedule",
            &self.runtime,
            message,
            self.owner(),
        );
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
    /// years, which in practice never fires. The first tick is due one
    /// interval after the call that registers the timer (the final `.await`);
    /// the miss policy governs the ticks after it.
    ///
    /// # Panics
    /// Panics, before it registers anything, when this actor's runtime was
    /// built without the Tokio time driver.
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
        let factory = require_timers_holding(
            "ActorContext::every_with",
            &self.runtime,
            factory,
            self.owner(),
        );
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
        let id = self.ex_mut().id_gen.next();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.self_handle.clone();
        let claimed = Arc::new(AtomicBool::new(false));
        let fut = oneshot_forward(handle, message, when, cancel_clone, claimed.clone());
        self.spawn_forwarder(ForwarderKind::Timer(id), fut, PanicSlot::default());
        self.ex_mut().timers.insert(
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
        match self.ex_mut().timers.remove(&id) {
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
        if let Some(extras) = self.extras.as_deref_mut() {
            for entry in extras.timers.values() {
                if entry.claim() {
                    entry.token.cancel();
                }
            }
            extras.timers.clear();
        }
    }

    /// Returns the number of active timers.
    pub fn active_timer_count(&self) -> usize {
        self.ex().timers.len()
    }

    // - Streams ------------------------------------------------------------

    /// Attaches an external stream to this actor's mailbox.
    ///
    /// Like a recurring [`schedule`](Self::schedule), the forwarder task
    /// holds an internal self-reference (see [`self_handle`](Self::self_handle))
    /// that never extends the actor's lifetime: it is torn down when the
    /// actor stops.
    ///
    /// A one-item stream also gives a request that does not block the turn,
    /// as Erlang/OTP's `send_request` does: clone the peer's handle and attach
    /// `Box::pin(futures::stream::once(async move { h.send(msg).await }))`,
    /// the reply mapped into the actor's message through `From<StreamEvent<_>>`
    /// (the `futures` crate is the caller's). An actor can likewise stand in
    /// for Erlang/OTP's `supervisor_bridge`: its `on_started` spawns the task
    /// and attaches `futures::stream::once(task)` over the task's `JoinHandle`
    /// as a one-item stream, its `handle` turns the task's end into an `Err`,
    /// so the actor stops with `StopReason::Failure` (a `supervisor_bridge`
    /// stops with the subsystem's own exit reason), and its `on_stopped`
    /// aborts the task.
    pub fn add_stream<S>(&mut self, stream: S) -> StreamId
    where
        S: Stream + Send + Unpin + 'static,
        S::Item: Send + 'static,
        A::Message: From<StreamEvent<S::Item>>,
    {
        let id = self.ex_mut().id_gen.next_stream_id();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.self_handle.clone();
        let slot = PanicSlot::default();
        let fut = stream_forward::<A, S>(stream, handle, cancel_clone, slot.clone());
        self.spawn_forwarder(ForwarderKind::Stream(id), fut, slot);
        self.ex_mut()
            .streams
            .insert(id, StreamRegistration { token });
        id
    }

    /// Cancels a specific stream by its ID.
    ///
    /// Once the call returns, nothing more from the stream is put in the
    /// mailbox, except an item that a worker was already putting there at
    /// that instant (multi-thread runtimes); items already in the mailbox are
    /// handled as usual. An item that waits for room in a full mailbox is
    /// dropped by the stream's forwarder in its next poll instead of being
    /// delivered, and so is a `Finished` that waits; neither is dropped inside
    /// the call.
    pub fn cancel_stream(&mut self, id: StreamId) -> Result<(), StreamError> {
        match self.ex_mut().streams.remove(&id) {
            Some(entry) => {
                entry.token.cancel();
                Ok(())
            }
            None => Err(StreamError::NotFound),
        }
    }

    /// Cancels all active streams, each as [`cancel_stream`](Self::cancel_stream)
    /// describes.
    pub fn cancel_all_streams(&mut self) {
        if let Some(extras) = self.extras.as_deref_mut() {
            for entry in extras.streams.values() {
                entry.token.cancel();
            }
            extras.streams.clear();
        }
    }

    /// Returns the number of active streams.
    pub fn active_stream_count(&self) -> usize {
        self.ex().streams.len()
    }

    // - Event subscriptions ------------------------------------------------

    /// Subscribes this actor to the telemetry events of its system that
    /// `filter` selects, each handed to `map` as a [`Delivery`] and turned
    /// into a message of this actor's own type.
    ///
    /// The events reach `map` through a queue the crate keeps for this
    /// subscription, so the code that raises an event never waits for this
    /// actor and never runs `map`: when the queue (`Filter::queue`, 64 by
    /// default and at most 4096) is full the event is dropped and counted,
    /// and the next [`Delivery`] carries the count in `missed_before`. When
    /// the queue runs empty with drops not yet reported, `map` receives a
    /// [`SubscriptionNotice::Overflow`] notice with the count. `map` runs on
    /// a task of this actor's forwarder plane, in the order of the queue, and
    /// each message it returns waits for room in this actor's mailbox like a
    /// stream item.
    ///
    /// The events this actor itself is the subject of are never delivered to
    /// it. The subscription belongs to this incarnation of the actor: it ends
    /// when the actor stops or restarts, or at
    /// [`unsubscribe`](Self::unsubscribe).
    ///
    /// A `map` that panics ends the subscription: it is called once more with
    /// a [`SubscriptionNotice::MapPanicked`] notice, the panic is logged
    /// (with the `tracing` feature) naming the actor, and the actor goes on.
    /// A subscription from a context that is not part of a system's
    /// registry receives nothing.
    pub fn subscribe<F>(&mut self, filter: Filter, map: F) -> SubscriptionId
    where
        F: FnMut(Delivery) -> A::Message + Send + 'static,
    {
        let Some(mode) = self.stop_lane().abort_record().mode().cloned() else {
            return SubscriptionId(0);
        };
        let watcher = crate::telemetry::supervisor_ref(
            &mode,
            self.stop_lane().abort_record(),
            &self.actor_id,
        );
        let id = crate::telemetry::subscription::next_id(&mode);
        let (queue, rx) = crate::telemetry::subscription::SubQueue::new(filter.queue);
        let kinds = filter.kinds;
        let token = CancellationToken::new();
        let guard = crate::telemetry::subscription::SubscriptionEntryGuard::new(mode.clone(), id);
        crate::telemetry::subscription::register(
            &mode,
            crate::telemetry::subscription::SubEntry {
                id,
                watcher,
                filter,
                queue: queue.clone(),
            },
        );
        crate::telemetry::subscription::start_pump_for(&mode, kinds);
        let slot = PanicSlot::default();
        let fut = subscription_forward::<A, F>(
            guard,
            rx,
            queue,
            map,
            self.self_handle.clone(),
            token.clone(),
            slot.clone(),
        );
        self.spawn_forwarder(ForwarderKind::Subscription(id), fut, slot);
        self.ex_mut().subscriptions.insert(id, token);
        id
    }

    /// Ends the subscription `id`: once the call returns nothing more is put
    /// in the mailbox for it, except a message that a worker was already
    /// putting there at that instant (multi-thread runtimes). Returns `false`
    /// for an id this actor does not hold.
    pub fn unsubscribe(&mut self, id: SubscriptionId) -> bool {
        let Some(token) = self.ex_mut().subscriptions.remove(&id) else {
            return false;
        };
        token.cancel();
        crate::telemetry::with_mode(
            self.stop_lane().abort_record(),
            self.system_in_hand(),
            |mode| {
                crate::telemetry::subscription::unregister(mode, id);
            },
        );
        true
    }

    // - Monitors -----------------------------------------------------------

    /// Watches an actor: when it ends, `map` turns a [`Down`] into one of this
    /// actor's messages, which arrives through `handle` like any other
    /// message. The target is a handle, an [`ActorId`] or a name (see
    /// [`IntoMonitorTarget`]); an id or a name is resolved once, when the call is made, in this
    /// actor's system.
    ///
    /// The notice is sent once. It names the run of the target the monitor
    /// was bound to ([`Down::incarnation`]), so a supervised restart of the
    /// target fires the monitor with the old run's reason and spends it; watch
    /// the new run with a new monitor. A target that is gone when the call is
    /// made, or that matches no actor, gives a notice with
    /// [`DownReason::NoProc`](crate::types::DownReason::NoProc) at once. Making
    /// several monitors of one target is not an error; each is independent.
    ///
    /// The notice is sent after the target's registered name is free and after
    /// every message the target sent this actor before its reason was final,
    /// because it takes an ordinary mailbox slot behind them. A message that
    /// the target's own value sends from its `Drop`, after the target ended,
    /// may reach this actor after the notice. It may arrive
    /// before the target's own children have ended;
    /// [`SubtreeObserver::wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended)
    /// is the wait for the whole subtree.
    ///
    /// `map` runs on a task of the crate, concurrently with this actor, and
    /// must not block. A panic in it fails this actor with that panic as its
    /// reason. It may be called for a monitor that is revoked at that moment,
    /// and its result is then dropped.
    ///
    /// A notice that is still owed when this actor stops is delivered once,
    /// through `handle`, before `on_stopped`, with the status reading
    /// [`ActorStatus::Stopping`]; none is
    /// delivered when the stop is a `Kill`. A notice already queued in the
    /// mailbox when the stop begins is delivered with those, in the order it
    /// was queued; the other queued messages are dropped as at any stop. A
    /// monitor made while this actor is stopping returns
    /// [`MonitorError::Stopping`].
    ///
    /// Erlang/OTP: `erlang:monitor(process, Target)`, with the `{'DOWN', ...}`
    /// message mapped to a caller-chosen message instead of a fixed tuple.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult, Down, DownReason, StopReason,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Watch(ActorHandle<Worker>),
    ///     Gone(Down),
    /// }
    ///
    /// struct Watcher {
    ///     seen: mpsc::UnboundedSender<Down>,
    /// }
    ///
    /// impl Actor for Watcher {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         match msg {
    ///             Msg::Watch(worker) => {
    ///                 ctx.monitor(&worker, Msg::Gone).expect("the watcher is running");
    ///             }
    ///             Msg::Gone(down) => {
    ///                 let _ = self.seen.send(down);
    ///             }
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let (seen, mut downs) = mpsc::unbounded_channel();
    /// let worker = Worker.spawn().await?;
    /// let watcher = Watcher { seen }.spawn().await?;
    /// watcher.send(Msg::Watch(worker.clone())).await?;
    ///
    /// worker.stop(StopReason::Graceful).await?;
    /// let down = downs.recv().await.expect("the monitor fires");
    /// assert_eq!(down.incarnation, Some(worker.incarnation()));
    /// assert_eq!(down.reason, DownReason::Stopped(StopReason::Graceful));
    /// # Ok(())
    /// # }
    /// ```
    pub fn monitor<T, F>(&mut self, target: T, map: F) -> Result<MonitorId, MonitorError>
    where
        T: IntoMonitorTarget,
        F: Fn(Down) -> A::Message + Send + Sync + 'static,
    {
        self.register_monitor(target, map, false, false)
    }

    /// As [`monitor`](Self::monitor), with the notice entering through the
    /// priority lane, ahead of the ordinary messages queued behind it. An
    /// actor without a priority lane takes it through the mailbox, as
    /// `monitor` does.
    ///
    /// Erlang/OTP: `erlang:monitor(process, Target, [priority])` (OTP 28).
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, runtime::ActorConfig, Actor, ActorExt},
    ///     ActorHandle, ActorResult, Down, StopReason,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Watch(ActorHandle<Worker>),
    ///     Gone(Down),
    /// }
    ///
    /// struct Watcher {
    ///     seen: mpsc::UnboundedSender<Down>,
    /// }
    ///
    /// impl Actor for Watcher {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         match msg {
    ///             Msg::Watch(worker) => {
    ///                 ctx.monitor_priority(&worker, Msg::Gone).expect("the watcher is running");
    ///             }
    ///             Msg::Gone(down) => {
    ///                 let _ = self.seen.send(down);
    ///             }
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let (seen, mut downs) = mpsc::unbounded_channel();
    /// let worker = Worker.spawn().await?;
    /// let watcher = Watcher { seen }
    ///     .spawn()
    ///     .with_config(ActorConfig::default().with_priority_lane(4))
    ///     .await?;
    /// watcher.send(Msg::Watch(worker.clone())).await?;
    ///
    /// worker.stop(StopReason::Graceful).await?;
    /// assert!(downs.recv().await.is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn monitor_priority<T, F>(&mut self, target: T, map: F) -> Result<MonitorId, MonitorError>
    where
        T: IntoMonitorTarget,
        F: Fn(Down) -> A::Message + Send + Sync + 'static,
    {
        self.register_monitor(target, map, true, false)
    }

    /// Revokes a monitor. `true` means no notice for it will arrive; `false`
    /// means a notice has been queued already, or is being queued, and will
    /// arrive. An id this actor did not make, one that is unknown, and one
    /// that has fired and been dispatched all return `false`. A notice that
    /// was queued when a plain `demonitor` returned `false` is handled like
    /// any other: through `handle` when its turn comes, or, if the actor stops
    /// first, once at the stop with the notices the actor is owed.
    ///
    /// Erlang/OTP: `erlang:demonitor(Ref, [info])`, which returns `false` when
    /// a `DOWN` has probably been placed in the queue already. The plain
    /// `demonitor/1` never returns `false`, and demonitoring a reference of
    /// another process is an error there; here it returns `false`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Down,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Watcher(tokio_actors::ActorHandle<Worker>);
    ///
    /// impl Actor for Watcher {
    ///     type Message = Option<Down>;
    ///     type Response = bool;
    ///     async fn handle(
    ///         &mut self,
    ///         _: Option<Down>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<bool> {
    ///         let id = ctx.monitor(&self.0, Some).expect("the watcher is running");
    ///         // The worker is still running, so the revoke is in time.
    ///         Ok(ctx.demonitor(id))
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let worker = Worker.spawn().await?;
    /// let watcher = Watcher(worker).spawn().await?;
    /// assert!(watcher.send(None).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn demonitor(&mut self, id: MonitorId) -> bool {
        if let Some(state) = self.monitors.as_deref_mut() {
            if state.contains(id) {
                return state.demonitor(id, false);
            }
        }
        self.monitors
            .as_deref_mut()
            .and_then(|state| state.groups.demonitor(id, false))
            .unwrap_or(false)
    }

    /// As [`demonitor`](Self::demonitor), and a notice that is queued but not
    /// yet handled is dropped, so it never reaches `handle`. `true` means the
    /// revoke was in time and no notice existed; `false` means one had been
    /// queued and this call dropped it.
    ///
    /// Erlang/OTP: `erlang:demonitor(Ref, [flush, info])`, which removes a
    /// `DOWN` message from the queue and returns `false` when it had to.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Down,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Watcher(tokio_actors::ActorHandle<Worker>);
    ///
    /// impl Actor for Watcher {
    ///     type Message = Option<Down>;
    ///     type Response = bool;
    ///     async fn handle(
    ///         &mut self,
    ///         _: Option<Down>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<bool> {
    ///         let id = ctx.monitor(&self.0, Some).expect("the watcher is running");
    ///         // Nothing is queued, so the revoke is in time.
    ///         Ok(ctx.demonitor_flush(id))
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let worker = Worker.spawn().await?;
    /// let watcher = Watcher(worker).spawn().await?;
    /// assert!(watcher.send(None).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn demonitor_flush(&mut self, id: MonitorId) -> bool {
        if let Some(state) = self.monitors.as_deref_mut() {
            if state.contains(id) {
                return state.demonitor(id, true);
            }
        }
        self.monitors
            .as_deref_mut()
            .and_then(|state| state.groups.demonitor(id, true))
            .unwrap_or(false)
    }

    /// The tasks on the forwarder plane, ended ones the run loop has not yet
    /// taken included.
    #[cfg(test)]
    pub(crate) fn forwarder_tasks(&self) -> usize {
        self.ex().forwarders.len()
    }

    /// Whether the end of this run has anything to look at that only a monitor,
    /// a link, a request or a signal sender leaves: a monitor or a link entry
    /// held now (a late link entry included), the word's push-or-pair bit (set
    /// by every sender of a signal and every pair, before it reads the seal),
    /// or the signal cell, which a map function's panic is kept in
    /// (`keep_map_panic` makes it) and which the first monitor or link makes.
    /// A map function runs only in a task or a delivery that state started,
    /// and the end reads this after the last moment one can run. The receive
    /// timeout, the idle hook, a continuation and an alias run no map that
    /// the end collects, so an actor that uses only them, or only `stop`,
    /// skips the checks.
    #[inline]
    pub(crate) fn has_end_state(&self) -> bool {
        let record = self.stop_lane().abort_record();
        self.monitors
            .as_deref()
            .is_some_and(|state| state.len() != 0 || !state.links.is_empty())
            || record.has_signal_state()
            || record.has_signal_cell()
    }

    /// The monitors this actor holds that have not been delivered or revoked.
    pub(crate) fn monitor_count(&self) -> usize {
        self.monitors.as_deref().map_or(0, MonitorState::len)
    }

    /// The monitor state, when this actor ever made a monitor.
    pub(crate) fn monitors_mut(&mut self) -> Option<&mut MonitorState<A>> {
        self.monitors.as_deref_mut()
    }

    /// Removes the monitor whose notice was just dispatched.
    pub(crate) fn finish_monitor(&mut self, id: MonitorId) {
        if let Some(state) = self.monitors.as_deref_mut() {
            state.finish(id);
        }
    }

    /// A signal was just dispatched: the notice of a death monitor or a link
    /// ends its entry, a group change is counted as handled, the reply of a
    /// request leaves the count. Ids are unique across the four kinds, so at
    /// most one of the calls finds the entry.
    pub(crate) fn finish_signal(&mut self, tag: &SignalTag) {
        self.finish_monitor(tag.shared.id());
        if let Some(state) = self.monitors.as_deref_mut() {
            state.groups.finish(tag);
            state.requests.finish(tag);
        }
    }

    /// The requests this actor made that have not been answered or
    /// cancelled.
    pub(crate) fn request_count(&self) -> usize {
        self.monitors
            .as_deref()
            .map_or(0, |state| state.requests.len())
    }

    /// The group and scope monitors this actor holds that have not been
    /// revoked.
    pub(crate) fn group_monitor_count(&self) -> usize {
        self.monitors
            .as_deref()
            .map_or(0, |state| state.groups.len())
    }

    /// The table of group and scope monitors.
    pub(crate) fn groups_mut(&mut self) -> &mut GroupTable<A> {
        let owner = self.owner();
        &mut self
            .monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)))
            .groups
    }

    /// Starts the entry task of a group or scope monitor on the forwarder
    /// plane.
    pub(crate) fn spawn_group_forwarder<F>(&mut self, id: MonitorId, fut: F)
    where
        F: std::future::Future<Output = Released> + Send + 'static,
    {
        self.spawn_forwarder(ForwarderKind::Group(id), fut, PanicSlot::default());
    }

    /// The monitor of a spawn this actor has just made: as
    /// `monitor`, except that a run which has already ended
    /// gives its own reason, where `monitor` gives `NoProc`.
    pub(crate) fn monitor_spawned<B, F>(
        &mut self,
        target: &ActorHandle<B>,
        map: F,
    ) -> Result<MonitorId, MonitorError>
    where
        B: Actor,
        F: Fn(Down) -> A::Message + Send + Sync + 'static,
    {
        self.register_monitor(target, map, false, true)
    }

    /// The link of a spawn this actor has just made: as `link`, except that a
    /// run which has already ended delivers its own reason through the link
    /// rules, where `link` gives `NoProc`.
    pub(crate) fn link_spawned<B: Actor>(
        &mut self,
        target: &ActorHandle<B>,
    ) -> Result<(), LinkError> {
        link::link(self, target, false, true)
    }

    /// Makes a monitor and starts its task. `spawned` is true for the monitor
    /// of a run this actor has just spawned.
    fn register_monitor<T, F>(
        &mut self,
        target: T,
        map: F,
        priority: bool,
        spawned: bool,
    ) -> Result<MonitorId, MonitorError>
    where
        T: IntoMonitorTarget,
        F: Fn(Down) -> A::Message + Send + Sync + 'static,
    {
        if matches!(self.status(), ActorStatus::Stopping | ActorStatus::Stopped) {
            return Err(MonitorError::Stopping);
        }
        let resolved = monitor::resolve_target(&target, &self.resolved_system());
        let id = MonitorId::next();
        let map: Arc<MapFn<A::Message>> = Arc::new(map);
        let owner = self.owner();
        let (entry, task, token) = if spawned {
            monitor::new_spawned_entry::<A>(id, resolved, map, owner.clone())
        } else {
            monitor::new_entry::<A>(id, resolved, priority, map, owner.clone())
        };
        let handle = self.self_handle.clone();
        let state = self
            .monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)));
        state.bind(handle.stop_lane_ref().abort_record());
        state.insert(id, entry);
        let fut = monitor::monitor_forward(handle, task, token);
        self.spawn_forwarder(ForwarderKind::Monitor(id), fut, PanicSlot::default());
        Ok(id)
    }
}

impl<A: Actor> ActorContext<A> {
    /// Links this actor to another: when either ends, the other is told.
    ///
    /// The target is an [`ActorHandle`], an [`ActorId`] or a name, as for
    /// [`monitor`](Self::monitor). When the actor it names ends, this actor
    /// ends with the same reason, with no hook run, as an Erlang/OTP process
    /// that does not trap exits ends with its partner's exit reason, unless
    /// the reason is [`Graceful`](StopReason::Graceful): an orderly end of
    /// the other actor does not end this one. An actor that called
    /// [`trap_exit`](Self::trap_exit) is told with a [`LinkExit`] instead,
    /// through the map it gave, for every reason including `Graceful`. The
    /// flag is read when the other actor's end is seen.
    ///
    /// The link is bound to one run of each actor. It is symmetric, and both
    /// ends hold one state, so two actors that link each other at once share
    /// one link, and one [`unlink`](Self::unlink) cancels it for both. A
    /// repeated call is the same link. A link to oneself does nothing.
    ///
    /// Linking an actor that is not running, or that no key resolves to, is
    /// not an error: the exit arrives at once with
    /// [`DownReason::NoProc`](crate::types::DownReason::NoProc), and an actor
    /// that does not trap exits ends with
    /// [`ActorError::NoProc`], as `link/1`
    /// on a missing process does in Erlang/OTP.
    ///
    /// A trapped exit enters through an ordinary mailbox slot, after every
    /// message the other actor sent before it ended, so a queued `LinkExit`
    /// is a queued message; [`link_priority`](Self::link_priority) takes the
    /// priority lane instead. An exit still pending when this actor stops for
    /// any reason but `Kill` is delivered once before `on_stopped`, and none
    /// is delivered under `Kill`.
    ///
    /// Links are made and cancelled with no await, and may cross runtimes
    /// and systems (each entry runs on its owner's runtime).
    ///
    /// # Errors
    /// [`LinkError::Stopping`] once this actor is stopping.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult, DownReason, LinkExit, StopReason,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Watch,
    ///     Exit(LinkExit),
    /// }
    ///
    /// struct Boss {
    ///     worker: ActorHandle<Worker>,
    ///     seen: tokio::sync::mpsc::UnboundedSender<LinkExit>,
    /// }
    ///
    /// impl Actor for Boss {
    ///     type Message = Msg;
    ///     type Response = bool;
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<bool> {
    ///         match msg {
    ///             Msg::Watch => {
    ///                 ctx.trap_exit(Msg::Exit);
    ///                 Ok(ctx.link(&self.worker).is_ok())
    ///             }
    ///             Msg::Exit(exit) => {
    ///                 let _ = self.seen.send(exit);
    ///                 Ok(true)
    ///             }
    ///         }
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let worker = Worker.spawn().await?;
    /// let (seen, mut exits) = tokio::sync::mpsc::unbounded_channel();
    /// let boss = Boss { worker: worker.clone(), seen }.spawn().await?;
    /// assert!(boss.send(Msg::Watch).await?);
    /// worker.stop(StopReason::Graceful).await?;
    /// let exit = exits.recv().await.expect("the boss is told");
    /// assert_eq!(exit.actor_id, *worker.id());
    /// assert_eq!(exit.reason, DownReason::Stopped(StopReason::Graceful));
    /// # Ok(())
    /// # }
    /// ```
    pub fn link<T: IntoMonitorTarget>(&mut self, target: T) -> Result<(), LinkError> {
        link::link(self, target, false, false)
    }

    /// As [`link`](Self::link), and this actor's exit takes the priority lane
    /// when the actor has one, otherwise the ordinary mailbox. The option is
    /// this actor's own: the other end's lane is its own choice, and a
    /// repeated call to `link` or `link_priority` sets this end's lane.
    ///
    /// # Errors
    /// [`LinkError::Stopping`] once this actor is stopping.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorConfig, ActorHandle, ActorResult, LinkExit, MailboxConfig,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Boss(ActorHandle<Worker>);
    ///
    /// impl Actor for Boss {
    ///     type Message = Option<LinkExit>;
    ///     type Response = bool;
    ///     async fn handle(
    ///         &mut self,
    ///         msg: Option<LinkExit>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<bool> {
    ///         if msg.is_none() {
    ///             ctx.trap_exit(Some);
    ///             return Ok(ctx.link_priority(&self.0).is_ok());
    ///         }
    ///         Ok(true)
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let worker = Worker.spawn().await?;
    /// let config = ActorConfig {
    ///     mailbox: MailboxConfig {
    ///         priority_capacity: Some(8),
    ///         ..MailboxConfig::default()
    ///     },
    ///     ..ActorConfig::default()
    /// };
    /// let boss = Boss(worker).spawn().with_config(config).await?;
    /// assert!(boss.send(None).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn link_priority<T: IntoMonitorTarget>(&mut self, target: T) -> Result<(), LinkError> {
        link::link(self, target, true, false)
    }

    /// Cancels the link to an actor. True means the cancel was in time: no
    /// exit from that link will be acted on from here on, however the other
    /// actor ends, as Erlang/OTP guarantees for `unlink/1`. False means no
    /// such link is held, or the exit was already acted on (it may be queued
    /// in this actor's mailbox, and it is still delivered).
    ///
    /// The other actor is told, and drops its end of the link.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult, StopReason,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Boss(ActorHandle<Worker>);
    ///
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = (bool, bool);
    ///     async fn handle(
    ///         &mut self,
    ///         _: (),
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<(bool, bool)> {
    ///         ctx.link(&self.0).expect("running");
    ///         let first = ctx.unlink(&self.0);
    ///         let second = ctx.unlink(&self.0);
    ///         Ok((first, second))
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let worker = Worker.spawn().await?;
    /// let boss = Boss(worker.clone()).spawn().await?;
    /// assert_eq!(boss.send(()).await?, (true, false));
    /// // With the link cancelled, the boss goes on when its worker fails.
    /// worker.stop(StopReason::Failure(tokio_actors::ActorError::user("boom"))).await?;
    /// assert!(boss.is_alive());
    /// # Ok(())
    /// # }
    /// ```
    pub fn unlink<T: IntoMonitorTarget>(&mut self, target: T) -> bool {
        link::unlink(self, target)
    }

    /// Makes this actor trap exits: a linked actor's end, and an exit signal
    /// sent to this actor, reach `handle` as the message `map` makes from a
    /// [`LinkExit`], instead of ending this actor. A later call replaces the
    /// map.
    ///
    /// The map runs on a task of the crate, concurrently with this actor, except
    /// for an exit signal that rides the priority lane (an
    /// [`exit_priority`](Self::exit_priority) through an active priority alias
    /// to an actor with a lane): that map runs on the actor's own task between
    /// turns, as a turn the stuck-turn watchdog sees. It must not block either
    /// way, and a panic of it is contained: the actor ends with
    /// [`StopReason::Failure`] carrying that panic, unless a `Kill` or a
    /// `Failure` already stands, as it does for a monitor's map, whatever
    /// the linked actor's reason was (an orderly `Graceful` end included), and
    /// the panic is logged with the `tracing` feature. The flag is read when a linked
    /// actor's end is seen, so a call made just before that end is in time
    /// and a call made after it is not.
    ///
    /// A supervisor traps exits from its creation, as an Erlang/OTP
    /// supervisor does. With no map it drops an exit that is not its
    /// child's, silently.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, LinkExit,
    /// };
    ///
    /// struct Trapper;
    ///
    /// impl Actor for Trapper {
    ///     type Message = Option<LinkExit>;
    ///     type Response = ();
    ///     async fn handle(
    ///         &mut self,
    ///         msg: Option<LinkExit>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<()> {
    ///         if msg.is_none() {
    ///             ctx.trap_exit(Some);
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let trapper = Trapper.spawn().await?;
    /// assert!(!trapper.get_status().await?.trap_exit);
    /// trapper.send(None).await?;
    /// assert!(trapper.get_status().await?.trap_exit);
    /// # Ok(())
    /// # }
    /// ```
    pub fn trap_exit<F>(&mut self, map: F)
    where
        F: Fn(LinkExit) -> A::Message + Send + Sync + 'static,
    {
        link::trap_exit(self, Arc::new(map));
    }

    /// Stops trapping exits: a linked actor's end ends this actor again.
    /// The flag is cleared first and the map after. A supervisor goes on
    /// trapping, and only its map is cleared.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, LinkExit,
    /// };
    ///
    /// struct Toggle;
    ///
    /// impl Actor for Toggle {
    ///     type Message = bool;
    ///     type Response = ();
    ///     async fn handle(&mut self, on: bool, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         if on {
    ///             ctx.trap_exit(|_: LinkExit| false);
    ///         } else {
    ///             ctx.untrap_exit();
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let toggle = Toggle.spawn().await?;
    /// toggle.send(true).await?;
    /// assert!(toggle.get_status().await?.trap_exit);
    /// toggle.send(false).await?;
    /// assert!(!toggle.get_status().await?.trap_exit);
    /// # Ok(())
    /// # }
    /// ```
    pub fn untrap_exit(&mut self) {
        link::untrap_exit(self);
    }

    /// The links this actor holds and the exits it is delivering; created
    /// with the first one.
    pub(crate) fn link_table(&mut self) -> &mut LinkTable {
        let owner = self.owner();
        &mut self
            .monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)))
            .links
    }

    /// The links this actor holds that have not been delivered or cancelled.
    pub(crate) fn link_count(&self) -> usize {
        self.monitors
            .as_deref()
            .map_or(0, |state| state.links.len())
    }

    /// Whether the stage has link work: an entry waits in the table, or items
    /// wait in the signal inbox.
    pub(crate) fn link_work(&self) -> bool {
        self.monitors
            .as_deref()
            .is_some_and(|state| !state.links.is_empty())
            || self.stop_lane().abort_record().signals_pending()
    }

    /// Whether this actor can deliver a trapped exit now: the flag is set and
    /// a map stands.
    pub(crate) fn can_deliver_exits(&self) -> bool {
        self.stop_lane().abort_record().trap()
            && self
                .monitors
                .as_deref()
                .is_some_and(|state| state.trap.get().is_some())
    }

    /// The stage closes every link nobody has taken; see
    /// [`LinkTable::stage_pass`].
    pub(crate) fn close_links(&mut self) -> Vec<(MonitorId, LinkExit)> {
        let deliver = self.can_deliver_exits();
        self.monitors
            .as_deref_mut()
            .map(|state| state.links.stage_pass(deliver))
            .unwrap_or_default()
    }

    /// The cell the tasks of the links read the trap map from.
    pub(crate) fn trap_share(&mut self) -> Arc<TrapMap<A::Message>> {
        let owner = self.owner();
        self.monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner.clone())))
            .trap
            .share(&owner)
    }

    /// The trap cell of this actor.
    pub(crate) fn trap_cell(&mut self) -> &mut TrapCell<A::Message> {
        let owner = self.owner();
        &mut self
            .monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)))
            .trap
    }

    /// Starts the entry task of a link or an exit signal on the forwarder
    /// plane.
    pub(crate) fn spawn_link_forwarder<F>(&mut self, id: MonitorId, fut: F)
    where
        F: std::future::Future<Output = Released> + Send + 'static,
    {
        self.spawn_forwarder(ForwarderKind::Link(id), fut, PanicSlot::default());
    }
}

impl<A: Actor> ActorContext<A> {
    /// Sends `target` an exit signal carrying `reason`, from this actor.
    ///
    /// An actor that does not trap exit signals ends at once with `reason`
    /// as its reason, runs no `on_stopped`, and a request it was handling
    /// reads `ResponseDropped` carrying `reason`. A `reason` of
    /// [`Kill`](StopReason::Kill) is not trappable. An actor that traps
    /// exit signals gets the signal as a
    /// [`LinkExit`] through its map, like a link's
    /// end. A [`Graceful`](StopReason::Graceful) signal to another actor that
    /// does not trap is ignored; to this actor itself it ends a non-trapping
    /// actor. The target's end is backstopped like a kill: a target that does
    /// not reach a turn boundary within its kill grace is aborted. A signal to
    /// an actor that has ended, or to a name that matches none, does nothing.
    ///
    /// A supervised child that ends this way is restarted or not by its
    /// restart type, like any other end: a `Transient` child that receives
    /// [`Shutdown`](StopReason::Shutdown) is not restarted.
    ///
    /// The signals waiting for a trapping actor are bounded: as many signals
    /// sent with `exit` or [`exit_priority`](Self::exit_priority) may wait
    /// for it as its mailbox holds (64 for an actor that traps without having
    /// called [`trap_exit`](Self::trap_exit), as a supervisor does), counted
    /// until each one's delivery takes a slot of that mailbox. The exits of
    /// linked peers are not counted.
    ///
    /// Erlang/OTP: `exit(Pid, Reason)`; Erlang/OTP queues every signal
    /// without a bound, where this call refuses one over the bound.
    ///
    /// # Errors
    /// - [`ExitError::Full`](crate::error::ExitError::Full) when the target
    ///   traps exit signals and as many signals already wait for it as that
    ///   bound allows; the signal is not sent. A signal to a target that
    ///   does not trap, a `Kill`, and a signal to an ended or missing target
    ///   are never refused.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorError, ActorHandle, ActorResult, StopReason,
    /// };
    ///
    /// struct Idle;
    ///
    /// impl Actor for Idle {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Sender;
    ///
    /// impl Actor for Sender {
    ///     type Message = ActorHandle<Idle>;
    ///     type Response = ();
    ///     async fn handle(
    ///         &mut self,
    ///         target: ActorHandle<Idle>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<()> {
    ///         ctx.exit(&target, StopReason::Failure(ActorError::user("signalled")))
    ///             .expect("a target that does not trap never refuses");
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let target = Idle.spawn().await?;
    /// let sender = Sender.spawn().await?;
    /// sender.send(target.clone()).await?;
    /// assert!(matches!(target.wait_stopped().await, StopReason::Failure(_)));
    /// # Ok(())
    /// # }
    /// ```
    pub fn exit<T: IntoMonitorTarget>(
        &mut self,
        target: T,
        reason: StopReason,
    ) -> Result<(), crate::error::ExitError> {
        self.exit_with(&target, reason, None)
    }

    /// `exit`, with the alias the signal is sent through, if any. The alias's
    /// read guard is held from the check that it is active to the end of the
    /// call, the push into the target's inbox and the raise on a receiver
    /// that does not trap, so the alias's `unalias` is ordered against all of
    /// it: a signal through an alias that is not active has no effect, and
    /// once `unalias` returned `true` nothing of that alias takes effect. The
    /// signal rides the priority lane only for an alias made with the
    /// priority option.
    fn exit_with<T: IntoMonitorTarget>(
        &mut self,
        target: &T,
        reason: StopReason,
        alias: Option<&AliasShared>,
    ) -> Result<(), crate::error::ExitError> {
        // The guard is released explicitly, before the locals below drop, so
        // the drop of a clone of the system or of a reason never runs under
        // it.
        let gate = alias.map(AliasShared::read);
        if alias.is_some_and(|alias| !alias.is_active()) {
            return Ok(());
        }
        let via = monitor::route_of(target);
        // An id or a name is looked up in this actor's system; a handle names
        // its own, reached by identity through its record. A system that is
        // gone has no actor left to signal.
        let system = match &via {
            monitor::ExitVia::Handle(system) => match system {
                Some(system) => system.clone(),
                None => {
                    drop(gate);
                    return Ok(());
                }
            },
            monitor::ExitVia::Id | monitor::ExitVia::Name => self.resolved_system(),
        };
        let resolved = monitor::resolve_target(target, &self.resolved_system());
        let (actor_id, Some(status_rx), Some(lane)) =
            (resolved.actor_id, resolved.status_rx, resolved.lane)
        else {
            drop(gate);
            return Ok(());
        };
        let own = self.stop_lane().abort_record();
        let to_self = status_rx.record().same_block(own);
        let from = PeerRef {
            actor_id: self.actor_id.clone(),
            incarnation: own.incarnation(),
        };
        // `Kill` is not trappable: it never enters an inbox. Any other signal
        // is handed to a trapping actor as a message, the flag read where the
        // signal is pushed, and only a non-trapping one is raised on below.
        if !matches!(reason, StopReason::Kill) {
            let pushed = match alias {
                None => status_rx.record().push_exit_or_trap(from, reason.clone()),
                Some(alias) => status_rx.record().push_exit_or_trap_with(
                    from,
                    reason.clone(),
                    alias.is_priority(),
                ),
            };
            match pushed {
                ExitPush::Pushed | ExitPush::Sealed => {
                    drop(gate);
                    return Ok(());
                }
                ExitPush::Full => {
                    drop(gate);
                    return Err(crate::error::ExitError::Full);
                }
                ExitPush::NotTrapping => {}
            }
        }
        let cause = match reason {
            StopReason::Kill => None,
            StopReason::Graceful if !to_self => {
                drop(gate);
                return Ok(());
            }
            other => Some(other),
        };
        let key = match via {
            monitor::ExitVia::Name => KillKey::Name(actor_id.as_str()),
            _ => KillKey::Id(&actor_id),
        };
        raise_kill_declared(
            Some(&system),
            key,
            &status_rx,
            &lane,
            None,
            None,
            || {},
            cause,
        );
        drop(gate);
        Ok(())
    }
}

impl<A: Actor> ActorContext<A> {
    /// Sets a receive timeout: when the actor has been idle for `after`,
    /// `map` makes a message and the actor handles it, once per idle spell.
    ///
    /// A spell starts when the run loop is about to wait with the normal
    /// mailbox and the priority lane empty, and ends when a message starts a
    /// turn. A request on the system channel, such as a status read, neither
    /// ends it nor restarts it. The message is handled inline, on the actor's
    /// task, through `handle`: it takes no mailbox slot, leaves the status
    /// `Running`, and has no caller to answer. It fires again in the next
    /// spell, so the timeout stands until
    /// [`cancel_receive_timeout`](Self::cancel_receive_timeout) clears it. A
    /// duration of zero fires once per spell and never spins. A later call
    /// replaces the length and the function without restarting the spell, so a
    /// handler that sets the timeout again does not make it fire a second time
    /// in the same spell: the next fire waits for the next message. A
    /// `Kill` stored on the stop lane when a threshold falls due ends the
    /// actor in place of the fire.
    ///
    /// `map` runs on the actor's task; a panic in it ends the actor with
    /// `Failure`, as a panic in `handle` does.
    ///
    /// Erlang/OTP: the `{timeout, Time, Message}` action, which system
    /// messages do not restart (the legacy `Time` is restarted by them); here
    /// the timeout stands until it is cancelled.
    ///
    /// # Panics
    /// Panics if the actor's runtime was built without the Tokio time driver.
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// enum Msg {
    ///     Tick,
    ///     Count,
    /// }
    ///
    /// struct Poller {
    ///     ticks: u32,
    /// }
    ///
    /// impl Actor for Poller {
    ///     type Message = Msg;
    ///     type Response = u32;
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.receive_timeout(Duration::from_secs(5), || Msg::Tick);
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
    ///         if let Msg::Tick = msg {
    ///             self.ticks += 1;
    ///         }
    ///         Ok(self.ticks)
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let poller = Poller { ticks: 0 }.spawn().await?;
    /// assert_eq!(poller.send(Msg::Count).await?, 0);
    /// # Ok(())
    /// # }
    /// ```
    pub fn receive_timeout<F>(&mut self, after: Duration, map: F)
    where
        F: Fn() -> A::Message + Send + Sync + 'static,
    {
        require_runtime_timers("ActorContext::receive_timeout", &self.runtime);
        self.turn_mut().set_receive(after, Arc::new(map));
    }

    /// Clears the receive timeout set with
    /// [`receive_timeout`](Self::receive_timeout).
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// struct Poller;
    ///
    /// impl Actor for Poller {
    ///     type Message = ();
    ///     type Response = ();
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.receive_timeout(Duration::from_secs(5), || ());
    ///         ctx.cancel_receive_timeout();
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// Poller.spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn cancel_receive_timeout(&mut self) {
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.clear_receive();
        }
    }

    /// Sets how long the actor may be idle before
    /// [`on_idle`](Actor::on_idle) runs, once per idle spell. The spell is the
    /// one [`receive_timeout`](Self::receive_timeout) describes: a status
    /// read does not restart it, and a message ends it. A duration of zero
    /// runs the hook once per spell and never spins. A later call replaces
    /// the length without restarting the spell.
    ///
    /// Erlang/OTP: the trigger of `hibernate_after`, without the hibernation.
    ///
    /// # Panics
    /// Panics if the actor's runtime was built without the Tokio time driver.
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// struct Cache;
    ///
    /// impl Actor for Cache {
    ///     type Message = ();
    ///     type Response = ();
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.idle_after(Duration::from_secs(30));
    ///         Ok(())
    ///     }
    ///
    ///     async fn on_idle(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// Cache.spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn idle_after(&mut self, after: Duration) {
        require_runtime_timers("ActorContext::idle_after", &self.runtime);
        self.turn_mut().set_idle(after);
    }

    /// Clears the idle length set with [`idle_after`](Self::idle_after).
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// struct Cache;
    ///
    /// impl Actor for Cache {
    ///     type Message = ();
    ///     type Response = ();
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.idle_after(Duration::from_secs(30));
    ///         ctx.cancel_idle_after();
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// Cache.spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn cancel_idle_after(&mut self) {
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.clear_idle();
        }
    }

    /// Asks the actor to handle `msg` next, right after this callback, before
    /// it reads any other message, request or stop.
    ///
    /// The step runs after the reply of the current request is sent, takes no
    /// mailbox slot and has no caller to answer. A step may call
    /// `continue_with` again, so a chain of steps runs without anything else
    /// between them; a request on the system channel, such as a status read,
    /// waits until the chain ends, and so does a graceful stop asked from
    /// outside, as a system message waits behind `{continue, _}` in OTP. A
    /// stored `Kill` is read before each step and ends the actor without
    /// running the rest. Each step spends the task's cooperative budget, so a
    /// long chain gives the runtime's other tasks their turn. A second call in one
    /// callback replaces the first. A message set in `pre_start` or
    /// `on_started` runs once after the start is acknowledged, before the
    /// first message, and one set in [`on_idle`](Actor::on_idle) runs after it.
    ///
    /// The message is dropped, not handled, when the callback returns an
    /// `Err` or panics, when it calls [`stop`](Self::stop), when the start
    /// fails, when it is set in a callback that runs after the stop began, in
    /// `on_child_stopped` or in a `pre_stop` that vetoes the stop, and when the
    /// actor ends first.
    ///
    /// Erlang/OTP: the `{continue, Term}` return of a `gen_server` callback
    /// and `handle_continue/2`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// enum Msg {
    ///     Start,
    ///     Step,
    ///     Count,
    /// }
    ///
    /// struct Job {
    ///     log: Vec<&'static str>,
    /// }
    ///
    /// impl Actor for Job {
    ///     type Message = Msg;
    ///     type Response = usize;
    ///
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<usize> {
    ///         match msg {
    ///             Msg::Start => {
    ///                 self.log.push("start");
    ///                 ctx.continue_with(Msg::Step);
    ///             }
    ///             Msg::Step => self.log.push("step"),
    ///             Msg::Count => {}
    ///         }
    ///         Ok(self.log.len())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let job = Job { log: Vec::new() }.spawn().await?;
    /// assert_eq!(job.send(Msg::Start).await?, 1);
    /// assert_eq!(job.send(Msg::Count).await?, 2);
    /// # Ok(())
    /// # }
    /// ```
    pub fn continue_with(&mut self, msg: A::Message) {
        self.turn_mut().set_continuation(msg);
    }

    /// Makes an alias of this actor: a name others can send to, which this
    /// actor can end with [`unalias`](Self::unalias).
    ///
    /// The alias accepts messages until `unalias` returns `true` for it, it
    /// is a reply alias ([`AliasOptions::reply`]) that has taken its one
    /// message, or this actor ends. It never keeps the actor alive. With
    /// [`AliasOptions::priority`] its priority sends ride the actor's
    /// priority lane, which stays the alias's own when the actor's
    /// [`priority_by_alias`](crate::actor::runtime::MailboxConfig::with_priority_by_alias)
    /// option is set.
    ///
    /// Erlang/OTP: `alias/1`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::oneshot;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Alias, AliasOptions,
    /// };
    ///
    /// struct Owner;
    ///
    /// impl Actor for Owner {
    ///     type Message = oneshot::Sender<Alias<Owner>>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, reply: Self::Message, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let options = AliasOptions { reply: true, ..AliasOptions::default() };
    ///         let _ = reply.send(ctx.alias(options));
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let owner = Owner.spawn().await?;
    /// let (tx, rx) = oneshot::channel();
    /// owner.notify(tx).await?;
    /// let alias = rx.await?;
    /// let (first, _first_rx) = oneshot::channel();
    /// alias.notify(first).await?;
    /// let (second, _second_rx) = oneshot::channel();
    /// assert!(alias.notify(second).await.is_err());
    /// # Ok(())
    /// # }
    /// ```
    pub fn alias(&mut self, options: AliasOptions) -> Alias<A> {
        let own = self.stop_lane().abort_record().incarnation();
        let shared = Arc::new(AliasShared::new(options, (self.actor_id.clone(), own)));
        self.turn_mut().add_alias(shared.clone());
        Alias::new(self.self_handle.clone(), shared)
    }

    /// Ends `alias`. Returns `true` when it was active and this actor made it.
    ///
    /// When this returns `true` no message of the alias enters the mailbox
    /// afterwards; a message that entered before it stays queued and is
    /// handled. A send that was between its check and its put is waited for.
    /// An alias that is not active, or that another actor made, returns
    /// `false` and changes nothing.
    ///
    /// Erlang/OTP: `unalias/1`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::oneshot;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Alias, AliasOptions,
    /// };
    ///
    /// struct Owner;
    ///
    /// impl Actor for Owner {
    ///     type Message = oneshot::Sender<bool>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, reply: Self::Message, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let alias: Alias<Owner> = ctx.alias(AliasOptions::default());
    ///         let first = ctx.unalias(&alias);
    ///         let second = ctx.unalias(&alias);
    ///         let _ = reply.send(first && !second);
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let owner = Owner.spawn().await?;
    /// let (tx, rx) = oneshot::channel();
    /// owner.notify(tx).await?;
    /// assert!(rx.await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn unalias(&mut self, alias: &Alias<A>) -> bool {
        let shared = alias.shared();
        let own = self.stop_lane().abort_record().incarnation();
        let (id, incarnation) = shared.owner();
        if *id != self.actor_id || *incarnation != own {
            return false;
        }
        let was_active = shared.deactivate();
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.remove_alias(shared);
        }
        was_active
    }

    /// Sends `target`'s actor an exit signal carrying `reason`, as
    /// [`exit`](Self::exit) does, and asks that a trapping receiver gets it
    /// ahead of its ordinary messages.
    ///
    /// The signal rides the receiver's priority lane only when all of this
    /// holds: `target` is active and was made with
    /// [`AliasOptions::priority`], the receiver traps exits, and the
    /// receiver has a priority lane. Otherwise it behaves as
    /// [`exit`](Self::exit) does for the alias's actor: through the ordinary
    /// mailbox for a trapping receiver, and as an end for one that does not
    /// trap. A signal sent through an alias that is not active, because
    /// `unalias` ended it or its owner ended, has no effect, and `unalias` is
    /// ordered against this call: once it returned `true` no signal of that
    /// alias is queued or takes effect.
    ///
    /// Erlang/OTP: `exit/3` with the `priority` option and an alias as the
    /// destination; a signal sent to an inactive alias is dropped.
    ///
    /// # Errors
    /// - [`ExitError::Full`](crate::error::ExitError::Full) as for
    ///   [`exit`](Self::exit): the receiver traps exit signals and as many
    ///   signals already wait for it as its bound allows.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::oneshot;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorError, ActorResult, Alias, AliasOptions, StopReason,
    /// };
    ///
    /// struct Owner;
    ///
    /// impl Actor for Owner {
    ///     type Message = oneshot::Sender<Alias<Owner>>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, reply: Self::Message, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let options = AliasOptions { priority: true, ..AliasOptions::default() };
    ///         let _ = reply.send(ctx.alias(options));
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Sender;
    ///
    /// impl Actor for Sender {
    ///     type Message = Alias<Owner>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, alias: Alias<Owner>, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.exit_priority(&alias, StopReason::Failure(ActorError::user("signalled")))
    ///             .expect("a target that does not trap never refuses");
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let owner = Owner.spawn().await?;
    /// let (tx, rx) = oneshot::channel();
    /// owner.notify(tx).await?;
    /// let alias = rx.await?;
    /// let sender = Sender.spawn().await?;
    /// sender.send(alias).await?;
    /// assert!(matches!(owner.wait_stopped().await, StopReason::Failure(_)));
    /// # Ok(())
    /// # }
    /// ```
    pub fn exit_priority<B: Actor>(
        &mut self,
        target: &Alias<B>,
        reason: StopReason,
    ) -> Result<(), crate::error::ExitError> {
        let handle = target.handle();
        self.exit_with(&handle, reason, Some(&**target.shared()))
    }
}

impl<A: Actor> ActorContext<A> {
    /// Asks another actor and gets the answer as one of this actor's own
    /// messages, without waiting: the request goes out from a task of the
    /// crate, and the callee's response, or the reason there is none, arrives
    /// through `map` as a [`RequestReply`] that names the returned
    /// [`RequestId`].
    ///
    /// The reply enters this actor's mailbox through an ordinary slot, after
    /// every message the callee sent this actor before it answered, so a full
    /// mailbox delays it like any other message. `map` runs on a task of the
    /// crate, inside a catch: a panic in it fails this actor. The callee's
    /// end gives a reply too: `result` is exactly what
    /// [`send`](crate::actor::handle::ActorHandle::send) would have returned,
    /// including [`AskError::ResponseDropped`](crate::error::AskError) with
    /// the callee's final reason. `map` owns the reply: if it panics while
    /// the response in the reply has a `Drop` that panics too, the second
    /// panic happens during an unwind and ends the process, as it would
    /// anywhere else.
    ///
    /// A request to this actor itself is allowed, and cannot deadlock: it is
    /// handled after the current turn, as any message sent to oneself is.
    ///
    /// At most as many requests as this actor's mailbox holds may be in
    /// flight; [`set_request_cap`](Self::set_request_cap) raises or lowers
    /// the limit. A request leaves the count when its reply reaches `handle`
    /// or it is cancelled with [`cancel_request`](Self::cancel_request). A
    /// request has no deadline of its own: use
    /// [`request_within`](Self::request_within) for one.
    ///
    /// A reply that is still pending when this actor stops, queued or not, is
    /// dropped and never delivered, under any stop reason: it answers this
    /// actor's own question, and nothing is owed. The callee still handles
    /// the request. Only a typed handle names a callee, because `map` needs
    /// the reply type.
    ///
    /// Erlang/OTP: `gen_server:send_request/2`, with `wait_response` and
    /// `check_response` replaced by the mapped message (there is no selective
    /// receive here, so the message is the only way a reply arrives).
    ///
    /// # Errors
    /// - [`RequestError::Stopping`] once this actor is stopping.
    /// - [`RequestError::InFlightLimit`] while the cap is reached.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult, RequestReply,
    /// };
    ///
    /// struct Doubler;
    ///
    /// impl Actor for Doubler {
    ///     type Message = u32;
    ///     type Response = u32;
    ///     async fn handle(&mut self, msg: u32, _: &mut ActorContext<Self>) -> ActorResult<u32> {
    ///         Ok(msg * 2)
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Ask(ActorHandle<Doubler>, u32),
    ///     Answer(RequestReply<u32>),
    /// }
    ///
    /// struct Asker(mpsc::UnboundedSender<u32>);
    ///
    /// impl Actor for Asker {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         match msg {
    ///             Msg::Ask(doubler, value) => {
    ///                 ctx.request(&doubler, value, Msg::Answer).expect("the asker is running");
    ///             }
    ///             Msg::Answer(reply) => {
    ///                 let _ = self.0.send(reply.result.expect("the doubler answers"));
    ///             }
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let (answers, mut received) = mpsc::unbounded_channel();
    /// let doubler = Doubler.spawn().await?;
    /// let asker = Asker(answers).spawn().await?;
    /// asker.notify(Msg::Ask(doubler, 21)).await?;
    /// assert_eq!(received.recv().await, Some(42));
    /// # Ok(())
    /// # }
    /// ```
    pub fn request<B, F>(
        &mut self,
        target: &ActorHandle<B>,
        msg: B::Message,
        map: F,
    ) -> Result<RequestId, RequestError>
    where
        B: Actor,
        F: Fn(RequestReply<B::Response>) -> A::Message + Send + Sync + 'static,
    {
        self.register_request(target, msg, None, map)
    }

    /// As [`request`](Self::request), bounded by `within`: one deadline spans
    /// sending the request and waiting for its answer, and when it passes first
    /// the reply carries [`AskError::Timeout`](crate::error::AskError) (its
    /// `enqueued` flag says whether the callee had the request). The callee is
    /// not cancelled, and an answer that comes late is discarded, so the
    /// request gets exactly one reply.
    ///
    /// Erlang/OTP: `gen_server:send_request/2` with the timeout of
    /// `receive_response/2`.
    ///
    /// # Panics
    /// Panics, before it does anything else, when this actor's runtime was
    /// built without the Tokio time driver.
    ///
    /// # Errors
    /// As [`request`](Self::request).
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio::sync::mpsc;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult, AskError, RequestReply,
    /// };
    ///
    /// struct Sleeper;
    ///
    /// impl Actor for Sleeper {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         tokio::time::sleep(Duration::from_secs(3600)).await;
    ///         Ok(())
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Ask(ActorHandle<Sleeper>),
    ///     Answer(RequestReply<()>),
    /// }
    ///
    /// struct Asker(mpsc::UnboundedSender<bool>);
    ///
    /// impl Actor for Asker {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         match msg {
    ///             Msg::Ask(sleeper) => {
    ///                 let within = Duration::from_millis(50);
    ///                 ctx.request_within(&sleeper, (), within, Msg::Answer)
    ///                     .expect("the asker is running");
    ///             }
    ///             Msg::Answer(reply) => {
    ///                 let timed_out = matches!(reply.result, Err(AskError::Timeout { .. }));
    ///                 let _ = self.0.send(timed_out);
    ///             }
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let (answers, mut received) = mpsc::unbounded_channel();
    /// let sleeper = Sleeper.spawn().await?;
    /// let asker = Asker(answers).spawn().await?;
    /// asker.notify(Msg::Ask(sleeper)).await?;
    /// assert_eq!(received.recv().await, Some(true));
    /// # Ok(())
    /// # }
    /// ```
    pub fn request_within<B, F>(
        &mut self,
        target: &ActorHandle<B>,
        msg: B::Message,
        within: Duration,
        map: F,
    ) -> Result<RequestId, RequestError>
    where
        B: Actor,
        F: Fn(RequestReply<B::Response>) -> A::Message + Send + Sync + 'static,
    {
        require_runtime_timers("ActorContext::request_within", &self.runtime);
        self.register_request(target, msg, Some(within), map)
    }

    /// Drops a request's reply: `true` means no reply had been queued and
    /// none will be, `false` means one had been queued, and it is dropped
    /// when its turn comes instead of reaching `handle`. An id this actor did
    /// not make, one that is unknown, and one whose reply was already handed
    /// to `handle` all return `false`.
    ///
    /// The callee is not told: a request that is already in its mailbox is
    /// still handled, and its answer is discarded. A request still waiting
    /// for room in the callee's mailbox is never sent.
    ///
    /// Erlang/OTP has no call that cancels one request: a request id is
    /// dropped from a collection through the `Delete` argument of
    /// `gen_server:wait_response/3`, `receive_response/3` and
    /// `check_response/3`, and a late reply is discarded by the monitor alias.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult,
    /// };
    ///
    /// struct Echo;
    ///
    /// impl Actor for Echo {
    ///     type Message = u32;
    ///     type Response = u32;
    ///     async fn handle(&mut self, msg: u32, _: &mut ActorContext<Self>) -> ActorResult<u32> {
    ///         Ok(msg)
    ///     }
    /// }
    ///
    /// struct Asker(ActorHandle<Echo>);
    ///
    /// impl Actor for Asker {
    ///     type Message = ();
    ///     type Response = bool;
    ///     async fn handle(&mut self, _: (), ctx: &mut ActorContext<Self>) -> ActorResult<bool> {
    ///         let id = ctx.request(&self.0, 1, |_| ()).expect("running");
    ///         // The handler has not yielded, so no reply can have been queued.
    ///         Ok(ctx.cancel_request(id))
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let echo = Echo.spawn().await?;
    /// let asker = Asker(echo).spawn().await?;
    /// assert!(asker.send(()).await?);
    /// assert_eq!(asker.get_status().await?.requests_in_flight, 0);
    /// # Ok(())
    /// # }
    /// ```
    pub fn cancel_request(&mut self, id: RequestId) -> bool {
        self.monitors
            .as_deref_mut()
            .and_then(|state| state.requests.cancel(id.monitor()))
            .unwrap_or(false)
    }

    /// Sets how many requests this actor may have in flight at once. Unset,
    /// the limit is the capacity of its mailbox. A request over the limit is
    /// refused with [`RequestError::InFlightLimit`]; requests already in
    /// flight are unaffected by a lower limit. A cap of `0` is read as `1`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult,
    /// };
    ///
    /// struct Echo;
    ///
    /// impl Actor for Echo {
    ///     type Message = u32;
    ///     type Response = u32;
    ///     async fn handle(&mut self, msg: u32, _: &mut ActorContext<Self>) -> ActorResult<u32> {
    ///         Ok(msg)
    ///     }
    /// }
    ///
    /// struct Asker(ActorHandle<Echo>);
    ///
    /// impl Actor for Asker {
    ///     // `Some(cap)` starts a round of requests; `None` is a reply.
    ///     type Message = Option<usize>;
    ///     type Response = usize;
    ///     async fn handle(
    ///         &mut self,
    ///         msg: Option<usize>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<usize> {
    ///         let Some(cap) = msg else { return Ok(0) };
    ///         ctx.set_request_cap(cap);
    ///         let mut made = 0;
    ///         while ctx.request(&self.0, 1, |_| None).is_ok() {
    ///             made += 1;
    ///         }
    ///         Ok(made)
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let echo = Echo.spawn().await?;
    /// let asker = Asker(echo).spawn().await?;
    /// assert_eq!(asker.send(Some(3)).await?, 3);
    /// # Ok(())
    /// # }
    /// ```
    pub fn set_request_cap(&mut self, cap: usize) {
        let owner = self.owner();
        self.monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)))
            .requests
            .set_cap(cap);
    }

    /// Makes a request and starts its task. `within` is the deadline of
    /// `request_within`.
    fn register_request<B, F>(
        &mut self,
        target: &ActorHandle<B>,
        msg: B::Message,
        within: Option<Duration>,
        map: F,
    ) -> Result<RequestId, RequestError>
    where
        B: Actor,
        F: Fn(RequestReply<B::Response>) -> A::Message + Send + Sync + 'static,
    {
        if matches!(self.status(), ActorStatus::Stopping | ActorStatus::Stopped) {
            return Err(RequestError::Stopping);
        }
        let default_cap = self.self_handle.mailbox_capacity();
        if self
            .monitors
            .as_deref()
            .is_some_and(|state| !state.requests.has_room(default_cap))
        {
            return Err(RequestError::InFlightLimit);
        }
        let id = MonitorId::next();
        let shared = MonitorShared::for_request(id);
        let token = CancellationToken::new();
        let owner = self.owner();
        self.monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner.clone())))
            .requests
            .insert(id, shared.clone(), token.clone());
        let task = request::RequestTask {
            asker: self.self_handle.clone(),
            callee: target.clone(),
            msg,
            within,
            shared,
            token,
            map: request::HeldMap::new(map, owner),
        };
        self.spawn_forwarder(
            ForwarderKind::Request(id),
            request::request_forward(task),
            PanicSlot::default(),
        );
        Ok(RequestId::from_monitor(id))
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
    runtime: Option<Handle>,
    significant: bool,
    await_init: bool,
    sys: SysOptions,
    _child: std::marker::PhantomData<C>,
}

/// The `Debug` view of a child's start bound: `unbounded` for the default
/// and for `unbounded_start`, the duration for `start_timeout`.
struct StartBound(Option<Duration>);

impl std::fmt::Debug for StartBound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            None => f.write_str("unbounded"),
            Some(dur) => std::fmt::Debug::fmt(&dur, f),
        }
    }
}

impl<'ctx, A: Actor, F, C: Actor> std::fmt::Debug for ChildSpawnBuilder<'ctx, A, F, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSpawnBuilder")
            .field("name", &self.name)
            .field("restart_type", &self.restart_type)
            .field("shutdown", &self.shutdown)
            .field("start_timeout", &StartBound(self.start_timeout))
            .field("kill_grace", &self.kill_grace)
            .field("significant", &self.significant)
            .field("await_init", &self.await_init)
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
    /// worker that does not trap exit signals receives when its own supervisor
    /// is killed. An actor that is itself a supervisor, or a worker that traps
    /// exit signals, stops in order when its supervisor is killed,
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

    /// Starts the child with the given debug options on: statistics, the log
    /// and the trace, as the [`sys`](crate::actor::sys) calls would turn them
    /// on. Every restart of the child starts from the same options, not from
    /// the toggles of the incarnation before.
    pub fn debug(mut self, options: DebugOptions) -> Self {
        self.sys.debug = options;
        self
    }

    /// Lets [`inspect`](crate::actor::sys::Sys::inspect) and
    /// [`replace_state`](crate::actor::sys::Sys::replace_state) reach the
    /// child's value, in every incarnation. A sensitive child stays closed.
    pub fn allow_state_access(mut self) -> Self {
        self.sys.allow_state_access = true;
        self
    }

    /// Makes the child sensitive, as [`Actor::SENSITIVE`] does for every
    /// spawn of its type.
    pub fn sensitive(mut self) -> Self {
        self.sys.sensitive = true;
        self
    }

    /// Runs the child on the Tokio runtime behind `handle` instead of the
    /// supervisor's. Every restart of the child runs there too, and so does
    /// everything the child spawns in turn: its own children, its timers and
    /// its streams. The supervisor keeps the stop ladder of a child it stops
    /// or restarts: the child's [`Shutdown`] window, the kill grace and the
    /// settle that follow it, and the restart decisions run on the
    /// supervisor's runtime. A `Kill` raised from outside that ladder
    /// (`ActorSystem::kill`, `ActorSystem::kill_by_id`, `stop` with
    /// `StopReason::Kill` on the child's handle, or the `Kill` step of
    /// `stop_within`), and the end a gone supervisor leaves a child (if the
    /// child is a supervisor, its `Shutdown` window, then its kill grace), wait
    /// on the child's runtime: the kill grace, or the `within` of a
    /// `stop_within` called with `StopReason::Kill`.
    ///
    /// The runtime behind `handle` needs the time driver
    /// (`Builder::enable_time` or `enable_all`), because the abort timers that
    /// run on it (the kill grace of a `Kill` from outside the supervisor's
    /// ladder, and the end a gone supervisor leaves the child) sleep on it.
    /// Awaiting the builder panics, before the factory
    /// runs and
    /// before anything is registered, when the runtime was built without it:
    /// a placed child would lose its kill grace without a sign. The refusals
    /// of [`spawn_child`](ActorContext::spawn_child)
    /// ([`SpawnError::NotASupervisor`], [`SpawnError::SupervisorStopping`]
    /// and [`SpawnError::DuplicateChild`]) are answered first, so a spawn
    /// refused for one of those three reasons never panics; any other
    /// refusal, such as a zero child mailbox capacity or a name already taken
    /// in the system, is answered after the check and panics first on a
    /// runtime without the driver. The check asks the runtime whether it
    /// has been shut down before it asks about the driver, so a runtime that
    /// has ended passes it (see below) and no panic hook runs for it. With
    /// `panic = "abort"` there is no probe and so no check at the entry: the
    /// first timer the crate starts on a runtime without the driver ends the
    /// process.
    ///
    /// When the placed runtime is not running the child:
    ///
    /// - **Gone** (the `Runtime` was dropped or shut down): the child is
    ///   killed. [`spawn_child`](ActorContext::spawn_child) still returns
    ///   `Ok`, each restart fails at once, and each failure charges the
    ///   restart budget until it is spent.
    /// - **Alive but undriven** (a current-thread runtime that nothing is
    ///   running): no incarnation starts. A child spawned onto it is never
    ///   confirmed stopped: under a bounded `Shutdown`,
    ///   [`stop_child`](ActorContext::stop_child) and
    ///   [`terminate_child`](ActorContext::terminate_child) answer
    ///   [`SupervisionError::ChildUnresponsive`] for it once its `Shutdown`
    ///   window, kill grace and settle have passed. A restart onto such a
    ///   runtime stays pending: the crash that began it is charged once and the
    ///   wait charges nothing more; [`stop_child`](ActorContext::stop_child)
    ///   and [`terminate_child`](ActorContext::terminate_child) answer
    ///   [`SupervisionError::ChildRestarting`] for it until the runtime is
    ///   driven again or the supervisor stops. Under `OneForAll` and
    ///   `RestForOne`, with a [`start_timeout`](Self::start_timeout) on the
    ///   child, a group restart that waits for it cannot finish: once the
    ///   start timeout, the kill grace and the settle have passed, the
    ///   supervisor stops with
    ///   [`SupervisionError::ChildUnresponsive`] for that child.
    pub fn on_runtime(mut self, handle: &Handle) -> Self {
        self.runtime = Some(handle.clone());
        self
    }

    /// Overrides the default [`ActorConfig`](runtime::ActorConfig) for the child.
    pub fn with_config(mut self, config: runtime::ActorConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Bounds the wait for the initialization of each restart of this child.
    /// By default a restart's initialization waits without a bound, under any
    /// [`Shutdown`], as an Erlang/OTP supervisor's does; a bound is a
    /// deliberate departure that trades strict parity for a supervisor that
    /// one child's hung initialization cannot stall. A restart that does not
    /// finish `pre_start` and `on_started` within `dur` is cut: `Kill` is
    /// raised on it and its task is aborted once its kill grace has passed.
    /// A plain [`spawn_child`](ActorContext::spawn_child) never awaits the
    /// first start, so for it this bound applies to restarts only; with
    /// [`await_init`](Self::await_init) the bound also covers the first start
    /// and the awaited call returns [`SpawnError::StartTimeout`]. The strict
    /// start of a supervisor
    /// ([`with_strict_start`](crate::SupervisionConfig::with_strict_start))
    /// waits for the first start of each child outside it. A bound beyond 30
    /// years is treated as 30 years. The last
    /// call between this method and [`unbounded_start`](Self::unbounded_start)
    /// wins.
    pub fn start_timeout(mut self, dur: Duration) -> Self {
        self.start_timeout = Some(dur);
        self
    }

    /// Writes the default out: the initialization of each restart of this
    /// child waits without a bound, as an Erlang/OTP supervisor's does, under
    /// any [`Shutdown`]. It undoes an earlier
    /// [`start_timeout`](Self::start_timeout), and the last call between the
    /// two wins.
    ///
    /// This differs from `start_timeout(Duration::MAX)`: that call is a bound
    /// of 30 years, which arms a timer that a paused Tokio clock can fire
    /// once it advances past it, and its [`Debug`](std::fmt::Debug) output
    /// shows the duration. Here no timer is armed and the
    /// [`Debug`](std::fmt::Debug) output reads `unbounded`.
    pub fn unbounded_start(mut self) -> Self {
        self.start_timeout = None;
        self
    }

    /// Marks the child as significant, as Erlang/OTP's `significant` child
    /// flag does: when the supervisor does not restart the child, its end can
    /// shut the supervisor down, under the supervisor's
    /// [`AutoShutdown`] mode.
    /// Default: `false`.
    ///
    /// Only an end the supervisor did not order counts: a `Transient` child's
    /// normal exit or a `Temporary` child's exit of any kind. A child stopped
    /// by [`stop_child`](ActorContext::stop_child),
    /// [`terminate_child`](ActorContext::terminate_child) or a group restart
    /// never shuts the supervisor down.
    ///
    /// Awaiting the builder answers
    /// [`SpawnError::InvalidSupervision`] before any factory runs when
    /// `significant` is `true` and the supervisor's mode is
    /// [`AutoShutdown::Never`],
    /// or the child is [`RestartType::Permanent`], which Erlang/OTP refuses
    /// too.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, AutoShutdown, RestartType, SupervisionConfig,
    /// };
    ///
    /// #[derive(Default)]
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    /// }
    ///
    /// struct Boss;
    ///
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.spawn_child(Worker::default)
    ///             .restart_type(RestartType::Transient)
    ///             .significant(true)
    ///             .await?;
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let _boss = Boss
    ///     .spawn()
    ///     .with_supervision(
    ///         SupervisionConfig::one_for_one().with_auto_shutdown(AutoShutdown::AnySignificant),
    ///     )
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn significant(mut self, significant: bool) -> Self {
        self.significant = significant;
        self
    }

    /// Makes the awaited builder return after the child's initialization
    /// (`pre_start` and `on_started`), as Erlang/OTP's `supervisor:start_child`
    /// does, instead of once the child is registered. Default: not awaited.
    ///
    /// On success the call returns the child's handle with the child running.
    /// If the initialization returns an error or panics, the call returns
    /// [`SpawnError::Init`] with it and the child is discarded: no spec is
    /// kept, [`on_child_stopped`](crate::actor::Actor::on_child_stopped) is not
    /// called for it and the restart budget is not charged. A child that was
    /// killed first gives [`SpawnError::Killed`]. A spawn under the same id
    /// afterwards waits for the failed incarnation's subtree to end, as after
    /// [`delete_child`](ActorContext::delete_child).
    ///
    /// The wait has no bound by default, as Erlang/OTP's start has none;
    /// [`start_timeout`](Self::start_timeout) bounds it, and a child whose
    /// initialization is still running at the bound is cut (`Kill` raised,
    /// task aborted) and the call returns [`SpawnError::StartTimeout`]. With
    /// that bound the runtime needs the time driver, and awaiting the builder
    /// panics without it. A child that
    /// calls its supervisor during its own initialization waits for the
    /// supervisor, which waits for it, until that bound, as in Erlang/OTP.
    ///
    /// Dropping the future before it completes leaves the child registered
    /// as a plain [`spawn_child`](ActorContext::spawn_child) leaves it; a later
    /// failure of its initialization is then supervised like any crash, and
    /// the [`start_timeout`](Self::start_timeout) the dropped wait carried no
    /// longer bounds the first start.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorError, ActorResult, SpawnError, SupervisionConfig,
    /// };
    ///
    /// #[derive(Default)]
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn pre_start(&mut self, _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Err(ActorError::user("no config"))
    ///     }
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    /// }
    ///
    /// struct Boss;
    ///
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = bool;
    ///     async fn handle(&mut self, _: (), ctx: &mut ActorContext<Self>) -> ActorResult<bool> {
    ///         let started = ctx.spawn_child(Worker::default).named("w").await_init().await;
    ///         Ok(matches!(started, Err(SpawnError::Init(_))))
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss
    ///     .spawn()
    ///     .with_supervision(SupervisionConfig::one_for_one())
    ///     .await?;
    /// assert!(boss.send(()).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn await_init(mut self) -> Self {
        self.await_init = true;
        self
    }
}

/// The supervisor's report of a `spawn_child` it refused before any child
/// existed: `named` is the child's name, when it has one.
fn report_refused(
    supervisor: &ActorId,
    named: Option<&ActorId>,
    err: &SpawnError,
    sensitive: bool,
) {
    let unnamed = ActorId::from("?");
    reports::start_error(
        supervisor,
        named.unwrap_or(&unnamed),
        None,
        || StopReason::Failure(ActorError::Spawn(err.clone())),
        sensitive,
    );
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
                runtime,
                significant,
                await_init,
                sys,
                _child,
            } = self;
            let sensitive = sys.effective_sensitive::<C>();
            let supervisor = ctx.actor_id().clone();
            // The refusals of the setup itself come before any factory runs
            // and before anything is registered; the refusals of the spawn
            // (not a supervisor, stopping, a taken name) keep their own order
            // and are answered first.
            ctx.refuse_setup(name.as_deref(), significant, restart_type, config.as_ref())
                .inspect_err(|err| {
                    let named = name.as_deref().map(ActorId::from);
                    report_refused(&supervisor, named.as_ref(), err, sensitive);
                })?;
            if let Some(placed) = &runtime {
                // The refusals come first, as in `stop_within`: a spawn that
                // one of them refuses needs no timer and never panics.
                let named = name.as_deref().map(ActorId::from);
                ctx.refuse_spawn(named.as_ref()).inspect_err(|err| {
                    report_refused(&supervisor, named.as_ref(), err, sensitive);
                })?;
                require_runtime_timers("ChildSpawnBuilder::on_runtime", placed);
            }
            if await_init && start_timeout.is_some() {
                let named = name.as_deref().map(ActorId::from);
                ctx.refuse_spawn(named.as_ref()).inspect_err(|err| {
                    report_refused(&supervisor, named.as_ref(), err, sensitive);
                })?;
                require_caller_timers("ChildSpawnBuilder::await_init");
            }
            let name: Option<Arc<str>> = name.map(Arc::from);
            ctx.await_retired_subtree(name.as_ref())
                .await
                .inspect_err(|err| {
                    let named = name.as_ref().map(|name| ActorId::from_shared(name.clone()));
                    report_refused(&supervisor, named.as_ref(), err, sensitive);
                })?;
            let (handle, wait) = ctx.spawn_child_internal(
                factory,
                name,
                restart_type,
                shutdown,
                config,
                start_timeout,
                kill_grace,
                runtime,
                ChildFlags {
                    significant,
                    await_init,
                },
                sys,
            )?;
            match wait {
                None => Ok(handle),
                Some(wait) => ctx.finish_await_init(handle, wait, start_timeout).await,
            }
        })
    }
}

/// The options of a child spawn that the spec records or the call awaits.
#[derive(Clone, Copy)]
struct ChildFlags {
    significant: bool,
    await_init: bool,
}

/// What a spawn with `await_init` waits on: the child's start
/// acknowledgment, and what cuts the start at a bound.
struct InitWait {
    ack: oneshot::Receiver<Result<(), ActorError>>,
    child_id: ActorId,
    incarnation: u64,
    lane: StopLane,
    abort: tokio::task::AbortHandle,
}

/// Waits for a start acknowledgment until `expired` completes. When the bound
/// passes first, the channel is read once: a value there means the child
/// acknowledged, and the start succeeded or failed as it answered; a sender
/// dropped without a reply is the init's own end, as it is when the end is read
/// before the bound. Only with nothing to read is the channel closed and read
/// again: a value that landed before the close wins, and otherwise the child's
/// acknowledgment cannot be delivered after that (its send fails from the close
/// on), and `cut` runs. A child whose answer or end the first read finds is
/// never cut with a start timeout.
async fn bounded_ack(
    ack: &mut oneshot::Receiver<Result<(), ActorError>>,
    expired: impl Future<Output = ()>,
    cut: impl FnOnce(),
) -> InitEnd {
    tokio::select! {
        biased;
        answer = &mut *ack => init_end(answer),
        () = expired => match ack.try_recv() {
            Ok(answer) => init_end(Ok(answer)),
            Err(oneshot::error::TryRecvError::Closed) => InitEnd::Closed,
            Err(oneshot::error::TryRecvError::Empty) => {
                #[cfg(test)]
                before_close_hook();
                // The close wakes a sender that waits for it, inside a catch.
                contain_waker_panic("start ack close", None, || ack.close());
                match ack.try_recv() {
                    Ok(answer) => init_end(Ok(answer)),
                    Err(_) => {
                        cut();
                        InitEnd::TimedOut
                    }
                }
            }
        },
    }
}

#[cfg(test)]
thread_local! {
    /// Runs once, on the test's thread, between the first read of
    /// `bounded_ack`'s expiry branch and the close, so a test can land an
    /// acknowledgment in that window.
    static BEFORE_CLOSE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn before_close_hook() {
    if let Some(hook) = BEFORE_CLOSE.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

/// Reads the answer of a start acknowledgment.
fn init_end(answer: Result<Result<(), ActorError>, oneshot::error::RecvError>) -> InitEnd {
    match answer {
        Ok(Ok(())) => InitEnd::Started,
        Ok(Err(err)) => InitEnd::Failed(err),
        Err(_closed) => InitEnd::Closed,
    }
}

/// How the wait of a spawn with `await_init` ended.
enum InitEnd {
    Started,
    Failed(ActorError),
    /// The acknowledgment's sender dropped without a reply.
    Closed,
    TimedOut,
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
/// To send after a delay to another actor, or to stop or kill one later, as
/// Erlang/OTP's `timer:send_after` and `timer:kill_after` do, `tokio::spawn` a
/// `tokio::time::sleep`, then `notify`, `stop` or `kill_by_id`; a send to an
/// actor that has stopped returns `Closed`.
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
    /// years away, which in practice never fires. To read the time left of a
    /// pending timer, as Erlang/OTP's `erlang:read_timer/1` does, keep the
    /// `Instant` passed here and subtract `Instant::now()`; the result reads
    /// zero once the instant has passed and does not tell a cancelled timer
    /// from a pending one, where `erlang:read_timer/1` returns `false` for both
    /// an expired and a cancelled timer.
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
    /// years, which in practice never fires. The first tick is due one
    /// interval after the call that registers the timer (the final
    /// `.await`); the miss policy governs the ticks after it.
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
        let id = self.ctx.ex_mut().id_gen.next();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.ctx.self_handle.clone();
        let slot = PanicSlot::default();
        let first = runtime::saturating_deadline(Instant::now(), self.interval);
        let fut = recurring_loop(
            handle,
            id,
            self.factory,
            first,
            self.interval,
            self.miss_policy,
            cancel_clone,
            slot.clone(),
        );
        self.ctx
            .spawn_forwarder(ForwarderKind::Timer(id), fut, slot);
        self.ctx.ex_mut().timers.insert(
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
        // The group and scope monitors end with the actor, before its status
        // can read `Stopped`; the changes they had queued are dropped.
        if let Some(state) = self.monitors.as_deref_mut() {
            state.groups.clear();
        }
        // Nothing drains the signal inbox from here on: a later push is
        // refused, and what waits in it is dropped, outside the lock.
        self.self_handle
            .stop_lane_ref()
            .abort_record()
            .seal_signals();
        if let Some(extras) = self.extras.as_deref() {
            for entry in extras.timers.values() {
                entry.token.cancel();
            }
            for entry in extras.streams.values() {
                entry.token.cancel();
            }
        }
        // A context dropped before the terminal write ends an incarnation
        // that was dropped from outside: the reason is `Kill`, or a panic
        // when this task's own poll is unwinding (see
        // `runtime::TaskMarker`). It writes first only for a task dropped
        // before its first poll; on every other path the task's abort guard
        // or its seal wrote the reason already, and a written reason stays.
        // The common end wrote its reason already, and the plane says so
        // without its lock or a read of the stop lane.
        if self.status_tx.reason_written() {
            return;
        }
        let reason = runtime::aborted_reason(&self.status_tx, &self.task_marker);
        if crate::telemetry::engaged_for(self.stop_lane().abort_record(), self.system_in_hand()) {
            self.write_reason_noted(reason);
            return;
        }
        StatusValue::write_reason(&self.status_tx, reason, false);
    }
}

impl<A: Actor> ActorContext<A> {
    /// The tail of the context's drop for a system that takes telemetry
    /// events or holds an identity: the reason is copied for the end event.
    #[inline(never)]
    fn write_reason_noted(&self, reason: StopReason) {
        let kept = reason.clone();
        if StatusValue::write_reason(&self.status_tx, reason, false) {
            // This writer won the reason: the end event goes to the pump, as
            // every writer's does.
            runtime::emit_actor_ended(
                self.system_in_hand(),
                self.stop_lane().abort_record(),
                &kept,
            );
        }
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
    /// The owner's count of forwarder ends (see `Extras::forwarders_done`).
    done: Arc<AtomicU64>,
    /// Set once this forwarder's end is counted.
    counted: bool,
    future: Option<Pin<Box<F>>>,
    slot: PanicSlot,
    owner: runtime::Owner,
    _status: StatusTx,
    _subtree: Arc<SubtreeHold>,
}

impl<F> Forwarder<F> {
    /// Counts this forwarder's end once: at its ready poll, or at its drop
    /// when it ends without one (an abort, a runtime shutdown, an unwind out
    /// of its poll), which tokio runs before it completes the task.
    fn count_end(&mut self) {
        if !self.counted {
            self.counted = true;
            self.done.fetch_add(1, Ordering::Release);
        }
    }
}

impl<F> Drop for Forwarder<F> {
    fn drop(&mut self) {
        self.count_end();
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
pub(crate) struct Released {
    /// A drop before the forwarder saw the mailbox closed.
    pub(crate) run: Option<String>,
    /// A drop once the forwarder had seen the mailbox closed.
    pub(crate) end: Option<String>,
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
    fn both_at_the_close<T, U>(first: T, second: U, owner: &runtime::Owner) -> Self {
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
#[cfg(test)]
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
    build_for_attempt_of(factory, io, supervisor, child_id, incarnation, false)
}

/// [`build_for_attempt`] for a child that is `sensitive` or not: its log
/// lines print no text of a panic when it is.
#[cfg_attr(not(feature = "tracing"), allow(unused_variables))]
fn build_for_attempt_of<F, A>(
    factory: &std::sync::Weak<F>,
    io: &SlotIo,
    supervisor: &ActorId,
    child_id: &ActorId,
    incarnation: u64,
    sensitive: bool,
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
            panic = %crate::telemetry::LogFields::of_sensitive(sensitive).panic_text(payload),
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
            panic = %crate::telemetry::LogFields::of_sensitive(sensitive).panic_text(payload),
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
    owner: runtime::Owner,
    slot: PanicSlot,
}

impl<T> ReleaseGuard<T> {
    fn new(value: T, owner: runtime::Owner, slot: PanicSlot) -> Self {
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

/// Holds a value the caller of a public call handed in while the call checks
/// that it can run. If the check panics, the value is dropped here inside a
/// catch, so a drop that panics adds no second panic to the unwind (which
/// would end the process); that drop's panic is logged and the check's stands.
struct EntryHold<T> {
    value: Option<T>,
    owner: runtime::Owner,
}

impl<T> Drop for EntryHold<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            if let Some(panic) = runtime::end_value(value) {
                runtime::log_discarded(&panic, &self.owner);
            }
        }
    }
}

/// Panics, naming `op`, when `rt` was built without the time driver, and
/// otherwise gives `value` back. The value is held in the meantime so that
/// its drop, if the check panics, is contained.
fn require_timers_holding<T>(op: &'static str, rt: &Handle, value: T, owner: runtime::Owner) -> T {
    let mut held = EntryHold {
        value: Some(value),
        owner,
    };
    require_runtime_timers(op, rt);
    held.value.take().expect("held until the check passed")
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
    /// The incarnation's record, for the identity fields of the log line;
    /// held only in a build that writes the line.
    #[cfg(feature = "tracing")]
    record: crate::actor::subtree::AbortRecord,
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
            let fields = crate::telemetry::log_fields(&self.record, None);
            caught_log!(warn,
                actor_id = %self.owner,
                actor_name = fields.name(),
                incarnation = fields.incarnation(),
                system = fields.system(),
                panic = %fields.panic_text(&payload),
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
        let out = match (polled, parked) {
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
        };
        if out.is_ready() {
            this.count_end();
        }
        out
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
    Monitor(MonitorId),
    Link(MonitorId),
    Group(MonitorId),
    Request(MonitorId),
    Subscription(SubscriptionId),
}

pub(crate) type MessageFactory<A> = dyn FnMut() -> <A as Actor>::Message + Send + 'static;

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

#[allow(clippy::too_many_arguments)]
async fn recurring_loop<A: Actor>(
    handle: ActorHandle<A>,
    timer: RecurringId,
    factory: Box<MessageFactory<A>>,
    mut next: Instant,
    interval: Duration,
    miss_policy: MissPolicy,
    token: CancellationToken,
    slot: PanicSlot,
) -> Released {
    // The first local of the body: when the future is dropped (the end's
    // abort), the tick and the send in flight, which live in inner scopes,
    // drop before it, so the factory's drop comes last. Keep it first.
    let mut factory = ReleaseGuard::new(factory, handle.owner(), slot);
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
                        runtime::keep_first_of(&mut run, release(factory.take()), &handle.owner());
                        return Released { run, end: None };
                    }
                    permit = handle.reserve_forward() => permit,
                };
                let Some(permit) = permit else {
                    return Released::both_at_the_close(tick, factory.take(), &handle.owner());
                };
                permit.send(tick);
                let ticks = TickEvents { timer };
                match adjust_next(&mut next, interval, miss_policy, &token, &handle, factory.get_mut(), ticks) {
                    Adjusted::Next => {}
                    Adjusted::Refused(tick) => {
                        return Released::both_at_the_close(tick, factory.take(), &handle.owner());
                    }
                    Adjusted::Panicked(payload) => {
                        let mut run = Some(payload);
                        runtime::keep_first_of(&mut run, release(factory.take()), &handle.owner());
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
    let mut stream = ReleaseGuard::new(stream, handle.owner(), slot);
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
                        // The item waits for a slot in the mailbox and is never
                        // moved into the wait: a cancel that comes first ends
                        // the stream, and the item and the stream drop each in
                        // their own catch, in this poll, as the run's.
                        let permit = tokio::select! {
                            biased;
                            _ = token.cancelled() => {
                                let mut run = release(msg);
                                runtime::keep_first_of(&mut run, release(stream.take()), &handle.owner());
                                return Released { run, end: None };
                            }
                            permit = handle.reserve_forward() => permit,
                        };
                        let Some(permit) = permit else {
                            return Released::both_at_the_close(msg, stream.take(), &handle.owner());
                        };
                        permit.send(msg);
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
        #[cfg(feature = "tracing")]
        record: handle.stop_lane_ref().abort_record().clone(),
    };
    let msg: A::Message = StreamEvent::Finished.into();
    // `Finished` waits for a slot as an item does, and a cancel that comes
    // first drops it.
    let permit = tokio::select! {
        biased;
        _ = token.cancelled() => {
            let mut dropped = run.take();
            runtime::keep_first_of(&mut dropped, release(msg), &handle.owner());
            return Released { run: dropped, end: None };
        }
        permit = handle.reserve_forward() => permit,
    };
    match permit {
        Some(permit) => {
            permit.send(msg);
            Released {
                run: run.take(),
                end: None,
            }
        }
        None => Released {
            run: run.take(),
            end: release(msg),
        },
    }
}

/// The forwarder of one event subscription: takes the events the crate queued
/// for it, maps each into a message of the actor and puts it in the
/// mailbox, and reports the events the queue had to drop.
///
/// `entry` is a parameter, so it drops with the future on every path (the
/// future finishing, a cancel, the end's abort, and a future that was never
/// polled) and removes the subscription from the registry then.
async fn subscription_forward<A, F>(
    entry: crate::telemetry::subscription::SubscriptionEntryGuard,
    mut rx: mpsc::Receiver<crate::telemetry::subscription::QueueItem>,
    queue: Arc<crate::telemetry::subscription::SubQueue>,
    map: F,
    handle: ActorHandle<A>,
    token: CancellationToken,
    slot: PanicSlot,
) -> Released
where
    A: Actor,
    F: FnMut(Delivery) -> A::Message + Send + 'static,
{
    use crate::telemetry::subscription::QueueItem;

    let _entry = entry;
    // The first local of the body: when the future is dropped (the end's
    // abort), the message and the send in flight, which live in inner
    // scopes, drop before it, so the map's drop comes last. Keep it first.
    let mut map = ReleaseGuard::new(map, handle.owner(), slot);
    let closed = loop {
        let item = tokio::select! {
            biased;
            _ = token.cancelled() => return Released::default(),
            // The actor stopped; once the mailbox is closed the map is never
            // called again.
            () = handle.mailbox_close() => break true,
            item = rx.recv() => item,
        };
        let Some(item) = item else { break false };
        match item {
            QueueItem::Event {
                event,
                missed_before,
            } => {
                let delivery = Delivery {
                    event,
                    missed_before,
                };
                if let Err(ended) = map_and_send(&mut map, delivery, &handle, &token).await {
                    return ended;
                }
            }
            QueueItem::Wake => queue.wake_taken(),
        }
        // The queue ran empty: report what it had to drop.
        if rx.is_empty() {
            let missed = queue.take_dropped();
            if missed > 0 {
                let notice = Delivery {
                    event: Arc::new(TelemetryEvent::Notice(SubscriptionNotice::Overflow {
                        missed,
                    })),
                    missed_before: 0,
                };
                if let Err(ended) = map_and_send(&mut map, notice, &handle, &token).await {
                    return ended;
                }
            }
        }
    };
    if closed || handle.mailbox_closed() {
        return Released::at_the_close(map.take());
    }
    Released {
        run: release(map.take()),
        end: None,
    }
}

/// Maps `delivery` and puts the message in the mailbox. A map that panics
/// gets one more call, with the notice that it panicked, and ends the
/// subscription. `Err` is the end of the forwarder, with what it released.
async fn map_and_send<A, F>(
    map: &mut ReleaseGuard<F>,
    delivery: Delivery,
    handle: &ActorHandle<A>,
    token: &CancellationToken,
) -> Result<(), Released>
where
    A: Actor,
    F: FnMut(Delivery) -> A::Message + Send + 'static,
{
    let msg = match catch_sync(|| (map.get_mut())(delivery)) {
        Ok(msg) => msg,
        Err(payload) => {
            let panic = payload_into_string(payload);
            #[cfg(feature = "tracing")]
            caught_log!(warn,
                actor_id = %handle.id(),
                panic = %handle.log_fields().panic_text(&panic),
                "the map of an event subscription panicked; the subscription ends"
            );
            #[cfg(not(feature = "tracing"))]
            let _ = panic;
            let notice = Delivery {
                event: Arc::new(TelemetryEvent::Notice(SubscriptionNotice::MapPanicked)),
                missed_before: 0,
            };
            // A second panic ends the subscription without a notice.
            if let Ok(msg) = catch_sync(|| (map.get_mut())(notice)) {
                forward_message(msg, map, handle, token).await?;
            }
            return Err(Released {
                run: release(map.take()),
                end: None,
            });
        }
    };
    forward_message(msg, map, handle, token).await
}

/// Waits for room in the mailbox and sends `msg`; a cancel that comes first,
/// or a closed mailbox, ends the forwarder after releasing `msg` and the map.
async fn forward_message<A, F>(
    msg: A::Message,
    map: &mut ReleaseGuard<F>,
    handle: &ActorHandle<A>,
    token: &CancellationToken,
) -> Result<(), Released>
where
    A: Actor,
{
    // The message waits for a slot in the mailbox and is never moved into the
    // wait: a cancel that comes first ends the subscription, and the message
    // and the map drop each in their own catch, in this poll, as the run's.
    let permit = tokio::select! {
        biased;
        _ = token.cancelled() => {
            let mut run = release(msg);
            runtime::keep_first_of(&mut run, release(map.take()), &handle.owner());
            return Err(Released { run, end: None });
        }
        permit = handle.reserve_forward() => permit,
    };
    let Some(permit) = permit else {
        return Err(Released::both_at_the_close(
            msg,
            map.take(),
            &handle.owner(),
        ));
    };
    permit.send(msg);
    Ok(())
}

/// What [`adjust_next`] ended with.
pub(crate) enum Adjusted<M> {
    /// The timer goes on to its next tick.
    Next,
    /// The mailbox is closed: the tick that could not be sent is handed back.
    Refused(M),
    /// A `CatchUp` tick the full mailbox had no room for panicked when
    /// dropped, in its own catch: the timer ends with that panic.
    Panicked(String),
}

/// What a timer needs to report its ticks: its id.
pub(crate) struct TickEvents {
    pub(crate) timer: RecurringId,
}

/// Moves `next` past the tick just delivered and reports the tick to the
/// `TimerTick` sinks through the pump. Under `CatchUp` the missed ticks are
/// sent at once, each reported with its own lateness; a tick the full mailbox
/// has no room for is dropped, in its own catch, and the burst ends with one
/// report that says it paused.
pub(crate) fn adjust_next<A: Actor>(
    next: &mut Instant,
    interval: Duration,
    miss_policy: MissPolicy,
    token: &CancellationToken,
    handle: &ActorHandle<A>,
    factory: &mut Box<MessageFactory<A>>,
    ticks: TickEvents,
) -> Adjusted<A::Message> {
    let now = Instant::now();
    let TickEvents { timer } = ticks;
    // Whether a sink takes the kind, read once for the call: with none, the
    // off path of every report below is one branch on this value. A sink
    // attached during a catch-up burst reads from the next call.
    let taken = handle
        .stop_lane_ref()
        .abort_record()
        .mode()
        .is_some_and(|mode| mode.armed(EventKind::TimerTick));
    // Reports one tick, due at `due`, through the pump: this task is a timer
    // forwarder, which the actor's end joins, so it runs no handler. Nothing
    // is computed or built unless a sink takes the kind.
    let report = |due: Instant, missed: u128, paused: bool| {
        if !taken {
            return;
        }
        crate::telemetry::push_about(
            handle.stop_lane_ref().abort_record(),
            None,
            EventKind::TimerTick,
            |actor| TelemetryEvent::TimerTick {
                actor: Cow::Owned(actor.clone()),
                timer,
                policy: miss_policy,
                lateness: now.saturating_duration_since(due),
                missed: u64::try_from(missed).unwrap_or(u64::MAX),
                paused,
            },
        );
    };
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
            let missed = behind.as_nanos() / interval.as_nanos();
            let periods = missed + 1;
            report(*next, missed, false);
            *next = match u32::try_from(periods)
                .ok()
                .and_then(|periods| interval.checked_mul(periods))
            {
                Some(advance) => runtime::saturating_deadline(*next, advance),
                None => runtime::saturating_deadline(now, interval),
            };
        }
        MissPolicy::Delay => {
            // The schedule drifts by the lateness: the next deadline is one
            // interval from now, and no tick is counted as missed.
            report(*next, 0, false);
            *next = runtime::saturating_deadline(now, interval);
        }
        MissPolicy::CatchUp => {
            // The tick just delivered was due at `*next`.
            report(*next, 0, false);
            *next = runtime::saturating_deadline(*next, interval);
            while *next <= now {
                if token.is_cancelled() {
                    return Adjusted::Next;
                }
                let msg = (factory.as_mut())();
                match handle.try_forward(msg) {
                    Ok(()) => report(*next, 0, false),
                    // The mailbox has no room: the message made for this tick is
                    // dropped and the burst pauses here. The timer loop serves
                    // the same deadline again, with a fresh message, once there
                    // is room.
                    Err(mpsc::error::TrySendError::Full(tick)) => {
                        report(*next, 0, true);
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
#[path = "forwarder_count_tests.rs"]
mod forwarder_count_tests;

#[cfg(test)]
#[path = "forwarder_tests.rs"]
mod forwarder_tests;

#[cfg(test)]
#[path = "placement_tests.rs"]
mod placement_tests;

#[cfg(test)]
#[path = "skip_policy_tests.rs"]
mod skip_policy_tests;

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
        dummy_ctx_with_lane().0
    }

    /// A context and the receiving end of its stop lane.
    fn dummy_ctx_with_lane() -> (ActorContext<Dummy>, crate::types::LaneRx) {
        let (tx, _rx) = crate::actor::mailbox::test_channel(1);
        let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, stop_rx) = StopLane::new();
        let id = ActorId::from("dummy");
        let handle = ActorHandle::new(
            id.clone(),
            tx,
            stop_lane,
            1,
            status_rx,
            None,
            Some(Arc::new(std::sync::atomic::AtomicBool::new(false))),
        );
        let ctx = ActorContext::new(
            id,
            handle,
            Handle::current(),
            status_tx,
            None,
            None,
            Some(SupervisionState::new(SupervisionConfig::default())),
            TurnClock::default(),
            SubtreeHold::new(None),
        );
        (ctx, stop_rx)
    }

    /// `stop` refuses a `Kill` and writes nothing, reports a lane that takes
    /// no request as `Closed`, and marks the turn as a self-stop only when the
    /// lane took the request.
    #[tokio::test]
    async fn stop_refuses_kill_and_marks_the_turn_only_when_the_lane_took_it() {
        let (mut ctx, rx) = dummy_ctx_with_lane();
        assert_eq!(ctx.stop(StopReason::Kill), Err(StopError::KillRefused));
        assert!(
            rx.borrow().reason.is_none(),
            "a refused Kill writes nothing"
        );
        assert!(!ctx.take_self_stop());
        assert_eq!(ctx.stop(StopReason::Graceful), Ok(()));
        assert_eq!(rx.borrow().reason, Some(StopReason::Graceful));
        assert!(ctx.take_self_stop());
        assert!(!ctx.take_self_stop(), "the flag is read once");
        ctx.clear_self_stop();
        drop(rx);
        assert_eq!(ctx.stop(StopReason::Graceful), Err(StopError::Closed));
        assert!(!ctx.take_self_stop(), "a refused stop marks nothing");
        // A self-stop, an idle hook and a continuation leave nothing for the
        // end's signal checks; a signal cell does.
        assert!(ctx.monitors.is_none(), "a self-stop makes no signal state");
        ctx.continue_with(());
        assert!(!ctx.has_end_state(), "the turn state is no signal state");
        ctx.stop_lane().abort_record().set_exit_cap(1);
        assert!(ctx.has_end_state(), "a signal cell is checked at the end");
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
                        .ex_mut()
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

    /// A one-shot timer whose send waits on a full mailbox is cancelled: the
    /// cancel reads `Ok`, and the message never arrives.
    async fn a_cancel_while_the_send_waits_reads_ok() {
        let (answers, fired) = cancel_in_a_turn(TimedMsg::CancelWhileTheSendWaits).await;
        assert!(matches!(answers.as_slice(), [Ok(())]), "{answers:?}");
        assert_eq!(fired, 0, "the message never arrives");
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

    /// The bound of a spawn with `await_init` passes at the moment the child
    /// answers: an acknowledgment read at the expiry wins, and so does one that
    /// lands after that read and before the close, and the child is not cut; an
    /// init error read at the expiry is the start's failure and not a timeout;
    /// an init that ended without a reply is reported as its own end and not
    /// as a timeout, as it is when the end is read before the bound; with
    /// nothing to read, the close fails the child's send and the cut runs.
    #[tokio::test]
    async fn an_ack_that_lands_as_the_bound_expires_is_not_cut() {
        #[derive(Clone, Copy)]
        enum Landing {
            Acknowledged,
            InitError,
            SenderDropped,
            BetweenReadAndClose,
            Nothing,
        }
        for landing in [
            Landing::Acknowledged,
            Landing::InitError,
            Landing::SenderDropped,
            Landing::BetweenReadAndClose,
            Landing::Nothing,
        ] {
            let (tx, mut rx) = oneshot::channel::<Result<(), ActorError>>();
            let tx = std::cell::RefCell::new(Some(tx));
            let cut = std::cell::Cell::new(false);
            if matches!(landing, Landing::BetweenReadAndClose) {
                let sender = tx.borrow_mut().take();
                BEFORE_CLOSE.with(|slot| {
                    *slot.borrow_mut() = Some(Box::new(move || {
                        if let Some(sender) = sender {
                            let _ = sender.send(Ok(()));
                        }
                    }));
                });
            }
            let end = bounded_ack(
                &mut rx,
                async {
                    match landing {
                        Landing::Acknowledged => {
                            if let Some(tx) = tx.borrow_mut().take() {
                                let _ = tx.send(Ok(()));
                            }
                        }
                        Landing::InitError => {
                            if let Some(tx) = tx.borrow_mut().take() {
                                let _ = tx.send(Err(ActorError::user("init")));
                            }
                        }
                        Landing::SenderDropped => drop(tx.borrow_mut().take()),
                        Landing::BetweenReadAndClose | Landing::Nothing => {}
                    }
                },
                || cut.set(true),
            )
            .await;
            match landing {
                Landing::Acknowledged => {
                    assert!(matches!(end, InitEnd::Started), "acknowledged: started");
                    assert!(!cut.get(), "an acknowledged child is not cut");
                }
                Landing::InitError => {
                    assert!(
                        matches!(end, InitEnd::Failed(_)),
                        "an init error read at the expiry is the start's failure"
                    );
                    assert!(!cut.get(), "a child that answered is not cut");
                }
                Landing::SenderDropped => {
                    assert!(
                        matches!(end, InitEnd::Closed),
                        "a sender dropped without a reply: the init's own end"
                    );
                    assert!(!cut.get(), "an init that ended is not cut");
                }
                Landing::BetweenReadAndClose => {
                    assert!(
                        matches!(end, InitEnd::Started),
                        "acknowledged after the first read and before the close: started"
                    );
                    assert!(!cut.get(), "that child is not cut");
                }
                Landing::Nothing => {
                    assert!(matches!(end, InitEnd::TimedOut), "no ack: timed out");
                    assert!(cut.get(), "the cut ran");
                    let tx = tx.borrow_mut().take().expect("the sender is still here");
                    assert!(
                        tx.send(Ok(())).is_err(),
                        "the child's acknowledgment cannot be delivered after the close"
                    );
                }
            }
        }
    }
}
