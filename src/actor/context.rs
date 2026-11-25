//! Actor execution context providing timers, streams, and supervision hooks.

use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
#[cfg(test)]
use tokio::sync::oneshot;
use tokio::task::JoinSet;
#[cfg(test)]
use tokio::time;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::actor::handle::ActorHandle;
use crate::actor::incarnation::{StatusTx, StatusValue, SubtreeHold, TurnClock};
use crate::actor::monitor::MonitorState;
#[cfg(test)]
use crate::actor::supervision::{await_cell, CellOutcome, RestartOutcome, SlotIo};
use crate::actor::supervision::{CellWatch, ManualReply, ManualStop, SupervisionState};
use crate::actor::sys::SysOptions;
use crate::actor::sys_debug::DebugState;
use crate::actor::{runtime, Actor};
#[cfg(test)]
use crate::error::StopError;
use crate::error::{ActorError, SupervisionError};
use crate::system::ActorSystem;
use crate::telemetry::SubscriptionId;
#[cfg(test)]
use crate::types::ActorStatus;
use crate::types::{
    ActorId, MissPolicy, RecurringId, RecurringIdGenerator, RestartType, Shutdown,
    StopChildOutcome, StopReason, StreamId,
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
    /// Set by `ignore`, read and cleared once by the run loop when
    /// `pre_start` returns `Ok(())`.
    ignore_start: bool,
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
            ignore_start: false,
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

#[cfg(test)]
thread_local! {
    /// Runs once, on the test's thread, between the first read of
    /// `bounded_ack`'s expiry branch and the close, so a test can land an
    /// acknowledgment in that window.
    static BEFORE_CLOSE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
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
// The parts of this module, in the order the generated docs list their methods.

mod access;

mod children;
#[cfg(test)]
use self::children::*;

mod child_ops;

mod timers;
#[cfg(test)]
pub(crate) use self::timers::*;

mod streams;
#[cfg(test)]
use self::streams::*;

#[cfg(feature = "telemetry")]
mod subscriptions;

mod monitors;

mod links;

mod idle;

mod aliases;

mod requests;

mod forwarders;
pub(crate) use self::forwarders::*;

mod signal_state;

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
mod tests;
