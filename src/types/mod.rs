//! Shared type definitions used across the Tokio Actors runtime.

pub(crate) mod status_cell;
pub(crate) mod stop_lane;
pub(crate) mod turn_clock;

use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::error::{ActorError, AskError};

pub(crate) use status_cell::{StatusRx, StatusTx};
pub(crate) use stop_lane::{
    LaneCell, LaneRx, LaneState, RaiseResult, StopLane, StopSeverity, WakeSlot,
};
pub(crate) use turn_clock::TurnClock;

/// Identifier of an actor within its system, unique among the system's live
/// actors. An actor keeps its id across supervised restarts. An anonymous
/// actor gets a random id; a named actor's id is its name, so a later actor
/// that takes the name after the first has ended has an equal id, and an
/// id-keyed call such as `ActorSystem::kill_by_id` reaches whichever actor
/// holds the id when the call is made.
///
/// An actor's address is its handle's channels; its `ActorId` and name are
/// aliases a restarted child keeps, so a handle to an ended incarnation and one
/// to its successor compare equal: check `is_alive()` or resolve the id again
/// with `ActorSystem::get_by_id`.
///
/// An `ActorId` names the actor across its runs; the [`Incarnation`] of a
/// handle names one run, and a restart gives a new one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ActorId(Arc<str>);

impl ActorId {
    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id of a named actor, sharing the allocation that holds its name.
    pub(crate) fn from_shared(name: Arc<str>) -> Self {
        Self(name)
    }

    /// A fresh random (version 4) UUID id, hyphenated and lowercase, formatted
    /// on the stack into a single allocation.
    pub(crate) fn anonymous() -> Self {
        let id = uuid::Uuid::new_v4();
        let mut buf = [0u8; uuid::fmt::Hyphenated::LENGTH];
        Self(Arc::from(&*id.hyphenated().encode_lower(&mut buf)))
    }
}

/// What a drop path of an actor's task names in a log line: the actor's id in
/// a build that logs (the `tracing` feature) or tests, and nothing otherwise,
/// so the paths that only log hold no reference count on the id.
#[cfg(any(test, feature = "tracing"))]
pub(crate) type Who = ActorId;

/// What a drop path of an actor's task names in a log line: nothing, in a
/// build that neither logs nor tests.
#[cfg(not(any(test, feature = "tracing")))]
#[derive(Clone, Debug)]
pub(crate) struct Who;

// A plain build's drop paths hold no name at all.
#[cfg(not(any(test, feature = "tracing")))]
const _: () = assert!(std::mem::size_of::<Who>() == 0);

#[cfg(any(test, feature = "tracing"))]
impl ActorId {
    /// The name a drop path keeps for its log lines: a clone of the id.
    #[inline]
    pub(crate) fn who(&self) -> Who {
        self.clone()
    }

    /// The same, borrowed, for a call that only logs.
    #[inline]
    pub(crate) fn as_who(&self) -> &Who {
        self
    }
}

#[cfg(not(any(test, feature = "tracing")))]
impl ActorId {
    /// The name a drop path keeps for its log lines: nothing, where none is
    /// written.
    #[inline]
    pub(crate) fn who(&self) -> Who {
        Who
    }

    /// The same, borrowed, for a call that only logs.
    #[inline]
    pub(crate) fn as_who(&self) -> &Who {
        &Who
    }
}

impl<T> From<T> for ActorId
where
    T: Into<String>,
{
    fn from(value: T) -> Self {
        let owned: String = value.into();
        Self(Arc::from(owned.into_boxed_str()))
    }
}

impl Display for ActorId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Strategy for handling timer drift when recurring timers miss their scheduled time.
#[derive(Debug, Clone, Copy)]
pub enum MissPolicy {
    /// Skip missed ticks and schedule the next tick at the next aligned interval
    /// (after `u32::MAX` or more missed intervals, one interval from now).
    /// If multiple intervals have passed, they are all skipped.
    /// Example: If a 1s timer is delayed by 3.5s, the next tick happens at 4s.
    Skip,

    /// Send the missed ticks at once (catch up), then resume the normal schedule.
    /// No tick is lost: when the mailbox is full the burst pauses, and the timer
    /// task waits for room before it sends the next missed tick. Each time the
    /// burst finds the mailbox full, the message made for that tick is dropped
    /// and made again, so an `every_with` factory runs once more for each time
    /// the burst finds the mailbox full; keep it free of side effects.
    /// Example: If a 1s timer is delayed by 3.5s, send 3 messages immediately,
    /// then schedule the next at 4s.
    CatchUp,

    /// Reset the timer to fire after one interval from now, ignoring the original schedule.
    /// This prevents bursts but drifts from the original alignment.
    /// Example: If a 1s timer is delayed by 3.5s, the next tick happens at 4.5s.
    Delay,
}

/// Identifier used for recurring timers scheduled by an actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecurringId(u64);

impl RecurringId {
    pub(crate) fn new(id: u64) -> Self {
        Self(id)
    }
}

/// Incrementing ID source for timers and streams.
#[derive(Default)]
pub(crate) struct RecurringIdGenerator {
    next: AtomicU64,
}

impl RecurringIdGenerator {
    pub(crate) fn next(&self) -> RecurringId {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        RecurringId::new(id)
    }

    pub(crate) fn next_stream_id(&self) -> StreamId {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        StreamId::new(id)
    }
}

/// Reason describing why an actor stopped processing messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The actor stopped gracefully (e.g., finished its work).
    Graceful,
    /// The parent actor requested this actor to stop.
    ///
    /// Also the exit reason of a supervisor whose restart budget was
    /// exhausted (OTP parity: intensity-exceeded supervisors exit with
    /// `shutdown`), so a grandparent's `Transient` policy does not restart it.
    ParentRequest,
    /// The actor stopped itself, or was stopped, for an intended reason of its
    /// own, named by the term (an `Arc<str>` in place of an arbitrary term).
    /// It is a clean end: a `Transient` or `Temporary` child that ends with it
    /// is not restarted, the exit is not an abnormal one, and `pre_stop` runs
    /// as it does for [`Graceful`](StopReason::Graceful), so the actor can
    /// veto it. In the stop lane it is a plain request that ranks above
    /// `Graceful` and below [`ParentRequest`](StopReason::ParentRequest): an
    /// outside `Graceful` does not replace a pending `Shutdown`, and a
    /// `Shutdown` never replaces a pending `ParentRequest` or a stronger
    /// request.
    ///
    /// Erlang/OTP: `{shutdown, Term}`, which `proc_lib` and a supervisor
    /// treat like `normal` and `shutdown` (no crash report, no restart of a
    /// `transient` child). Raise it from a callback with
    /// [`ActorContext::stop`](crate::actor::context::ActorContext::stop),
    /// the counterpart of `{stop, {shutdown, Term}, State}`.
    Shutdown(Arc<str>),
    /// The actor stopped due to a failure: the handler returned `Err` during a
    /// `send` or a `notify` (cast-exception parity), or any callback panicked
    /// ([`ActorError::Panic`]).
    Failure(ActorError),
    /// A cancellation requested through the actor's stop lane. Not
    /// vetoable: `pre_stop` does not run, and `on_stopped` does.
    Cancelled,
    /// Raised by the crate on an actor that is itself a supervisor when its
    /// own supervisor ends without stopping it in order: that supervisor was
    /// killed or aborted, or its supervision of this actor failed
    /// internally. Not vetoable: `pre_stop` does not run; `on_stopped` runs
    /// and the actor stops its own children in order, bounded by its own
    /// [`Shutdown`]. Raising it by hand is likewise not vetoable and runs the
    /// same orderly teardown, but only the stop raised when its supervisor
    /// ends is bounded by the actor's own [`Shutdown`].
    ///
    /// Erlang/OTP: a supervisor traps its parent's exit and terminates in
    /// order.
    ///
    /// A worker that traps exit signals (it has called
    /// [`trap_exit`](crate::actor::context::ActorContext::trap_exit)) when its
    /// supervisor ends this way is raised `ParentKilled` too: it runs
    /// `on_stopped`, bounded by its own `Shutdown` window, after which it is
    /// killed; under `Shutdown::Infinity` that stop has no bound, and a wait
    /// on the subtree above the worker, such as
    /// [`SubtreeObserver::wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended),
    /// has none either while the worker lives and traps. A worker that does
    /// not trap has `Kill` raised on it at once, as Erlang/OTP kills a process
    /// that does not trap exits. The flag is read when the
    /// supervisor's end reaches the worker.
    ParentKilled,
    /// The actor was forcibly killed. Not vetoable, and no lifecycle hook
    /// begins once the actor reads it: a hook already running finishes, or is
    /// aborted when it next yields once the actor's kill grace has passed (or
    /// the window of the `stop_within` call that raised the `Kill`; at once
    /// when the actor's runtime has no time driver). The actor's value,
    /// context and queued messages are still dropped. Closest Erlang/OTP
    /// equivalent: `exit(Pid, kill)`, an untrappable exit signal that ends the
    /// process with reason `killed`.
    ///
    /// Also the reason when the actor's task was dropped because its runtime
    /// shut down, where Erlang/OTP's nearest end is `noconnection`.
    ///
    /// A `Kill` raised for an exit signal
    /// ([`exit`](crate::actor::context::ActorContext::exit)) ends the actor
    /// with the signal's reason in place of `Kill`: the plane, `wait_stopped`
    /// and a pending request all read that reason. The first signal's reason
    /// stands, and an explicit `Kill` carries none and takes its place. The
    /// actor runs no `on_stopped`, as an Erlang/OTP process that does not trap
    /// exits runs no `terminate/2`.
    Kill,
}

impl StopReason {
    /// Whether the reason is a clean end: `Graceful`, `ParentRequest`,
    /// `ParentKilled` and `Shutdown(_)`. A `Transient` child that ends with a
    /// clean reason is not restarted. For the crash report the reasons differ:
    /// `ParentKilled` counts as clean for the restart decision, yet the end it
    /// names, ordered by the supervisor's death, writes one crash report at
    /// error level, while `Graceful`, `ParentRequest` and `Shutdown(_)` write
    /// none.
    ///
    /// Erlang/OTP: the exit reasons `normal`, `shutdown` and
    /// `{shutdown, Term}`, which a `transient` child's supervisor treats as
    /// a normal termination.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_actors::StopReason;
    ///
    /// assert!(StopReason::Graceful.is_normal());
    /// assert!(StopReason::Shutdown("done".into()).is_normal());
    /// assert!(!StopReason::Cancelled.is_normal());
    /// assert!(!StopReason::Kill.is_normal());
    /// ```
    pub fn is_normal(&self) -> bool {
        matches!(
            self,
            StopReason::Graceful
                | StopReason::ParentRequest
                | StopReason::ParentKilled
                | StopReason::Shutdown(_)
        )
    }
}

impl Display for StopReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::Graceful => write!(f, "graceful"),
            StopReason::ParentRequest => write!(f, "parent requested"),
            StopReason::Shutdown(term) => write!(f, "shutdown: {term}"),
            StopReason::Failure(err) => write!(f, "failure: {err}"),
            StopReason::Cancelled => write!(f, "cancelled"),
            StopReason::ParentKilled => write!(f, "parent killed"),
            StopReason::Kill => write!(f, "killed"),
        }
    }
}

/// The answer of a successful
/// [`stop_child`](crate::actor::context::ActorContext::stop_child).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopChildOutcome {
    /// The call completed a stop: a running child was stopped under its
    /// `Shutdown` policy, by this call or by a manual stop already in
    /// progress that it joined, or a child whose task had already ended was
    /// handled with its real end reason, and the child was then restarted or
    /// kept down by its restart type, as `on_child_stopped` reports; a child
    /// that was already down is left as it is. A child that is not
    /// `Permanent` and is a member of, or was started during, a group
    /// restart that is still pending is stopped as `terminate_child` stops
    /// it: it stays down through that group restart, a `Transient` child
    /// keeping its specification and a `Temporary` child removed.
    Stopped,
    /// The child is a `Permanent` member of a group restart that is still
    /// pending and has still to restart it, so the call changed nothing: the
    /// group stops the child, if it has not already, and restarts it, also
    /// when it restarted the child once already and is now restarting the
    /// whole group again after another member failed to start; the group's
    /// own report reaches `on_child_stopped`.
    Absorbed,
}

/// Lifecycle status for a running actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActorStatus {
    /// The actor is initializing: from its creation through `pre_start` and
    /// `on_started`, until it enters its message loop.
    Initializing,
    /// The actor is running and processing messages. It stays `Running`
    /// while `pre_stop` decides on a stop request, so a veto leaves it here.
    Running,
    /// The actor is stopping: its message loop ended for a reason other than
    /// `Kill`, for example a stop its `pre_stop` accepted or a failure; it runs
    /// `on_stopped`, a supervisor first delivering the child deaths still
    /// pending and afterwards stopping its children. An actor whose message
    /// loop ends with `Kill`, or whose initialization fails, never reads
    /// `Stopping`: its status goes from `Running` (or `Initializing`) to
    /// `Stopped`.
    Stopping,
    /// The actor has stopped.
    Stopped,
}

/// The value of an actor's runtime status plane: its lifecycle status and,
/// once the actor's end is final, the reason it ended with and the instant
/// that reason became final.
///
/// The reason is written exactly once and never changed, and the instant is
/// taken by that same write. An actor that ends normally writes it at the
/// seal of its stop lane, before its queued messages, its value and its
/// context drop, and publishes the terminal [`ActorStatus::Stopped`] after
/// them. A task that never reaches its seal has it written by its abort
/// guard once its name is free (by its context when it is dropped before its
/// first poll): `Kill` for an abort, a shutdown, or a runtime dropped during
/// another unwind, `Failure(Panic)` for the task's own unwind outside every
/// catch; no `Stopped` follows. A reader that finds the plane closed with no
/// reason reads [`StopReason::Kill`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusValue {
    pub(crate) status: ActorStatus,
    pub(crate) reason: Option<StopReason>,
    /// When the reason was written: the instant the actor's end became
    /// final. `None` until the reason is written.
    pub(crate) final_at: Option<tokio::time::Instant>,
}

impl StatusValue {
    /// A non-terminal value: `status` with no reason.
    pub(crate) fn new(status: ActorStatus) -> Self {
        Self {
            status,
            reason: None,
            final_at: None,
        }
    }

    /// Writes the plane's reason, once, and stamps the instant of that
    /// write: a plane that already carries a reason is left exactly as it
    /// is, its stamp included. With `stopped` the same write publishes the
    /// terminal status, so no reader sees `Stopped` without its reason.
    /// Succeeds with no receivers left. Returns whether this call wrote.
    pub(crate) fn write_reason(tx: &StatusTx, reason: StopReason, stopped: bool) -> bool {
        tx.send_if_modified(|value| {
            if value.reason.is_some() {
                return false;
            }
            value.reason = Some(reason);
            value.final_at = Some(tokio::time::Instant::now());
            if stopped {
                value.status = ActorStatus::Stopped;
            }
            true
        })
    }
}

/// Event wrapper for items received from a stream attached via
/// [`ActorContext::add_stream`](crate::actor::context::ActorContext::add_stream).
///
/// Stream items are wrapped in `Data(T)` and delivered to the actor's `handle` method.
/// When the stream is exhausted (yields `None`), a `Finished` event is sent.
///
/// For fallible streams (`Stream<Item = Result<V, E>>`), `T` is `Result<V, E>`.
/// The actor handles errors in its `match` with full type information preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent<T> {
    /// The stream yielded an item.
    Data(T),
    /// The stream is exhausted (yielded `None`).
    Finished,
}

/// Identifier for a stream attached to an actor via
/// [`ActorContext::add_stream`](crate::actor::context::ActorContext::add_stream).
///
/// Used with [`cancel_stream`](crate::actor::context::ActorContext::cancel_stream)
/// to cancel a specific stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId(u64);

impl StreamId {
    pub(crate) fn new(id: u64) -> Self {
        Self(id)
    }
}

/// The process-wide source of [`Incarnation`] numbers. It starts at 1, so no
/// incarnation is 0.
static NEXT_INCARNATION: AtomicU64 = AtomicU64::new(1);

/// The process-wide source of [`MonitorId`] numbers. It starts at 1.
static NEXT_MONITOR: AtomicU64 = AtomicU64::new(1);

/// Names one run of an actor.
///
/// An [`ActorId`] names an actor across its supervised restarts; an
/// `Incarnation` names a single run of it. A restart gives the actor a new
/// incarnation, and no two runs in the process, in any
/// [`ActorSystem`](crate::system::ActorSystem), share one. Read it from a
/// handle with [`ActorHandle::incarnation`](crate::actor::handle::ActorHandle::incarnation)
/// or from a [`Down`].
///
/// Erlang/OTP: the nearest counterpart is a pid, which names one run of a
/// process; a pid can be reused once its process has ended and everything it
/// held is released, an incarnation never is.
///
/// # Examples
/// ```rust
/// use tokio_actors::{actor::{context::ActorContext, Actor, ActorExt}, ActorResult};
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
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let first = Idle.spawn().await?;
/// let second = Idle.spawn().await?;
/// assert_ne!(first.incarnation(), second.incarnation());
/// assert_eq!(first.incarnation(), first.clone().incarnation());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Incarnation(u64);

impl Incarnation {
    /// A fresh incarnation, unique in the process.
    pub(crate) fn mint() -> Self {
        Self(NEXT_INCARNATION.fetch_add(1, Ordering::Relaxed))
    }

    /// The incarnation a number drawn by [`mint`](Self::mint) names, for the
    /// paths that carry the number (a supervisor's ledger token, a
    /// registration's sequence) and build the record from it.
    pub(crate) fn from_token(number: u64) -> Self {
        debug_assert_ne!(number, 0, "an incarnation number is never 0");
        Self(number)
    }

    /// The number, which is never 0.
    #[inline]
    pub(crate) fn number(self) -> u64 {
        self.0
    }
}

impl Display for Incarnation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Names one monitor made with
/// [`ActorContext::monitor`](crate::actor::context::ActorContext::monitor).
///
/// The number is unique in the process, so an id made by another actor, or
/// one that has already fired or been revoked, is never taken for a live
/// monitor of the actor that is asked about it.
///
/// Erlang/OTP: the nearest counterpart is the reference `monitor/2` returns.
///
/// # Examples
/// ```rust
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     ActorHandle, ActorResult, Down,
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
/// struct Watcher(ActorHandle<Worker>);
///
/// impl Actor for Watcher {
///     type Message = Option<Down>;
///     type Response = bool;
///     async fn handle(
///         &mut self,
///         _: Option<Down>,
///         ctx: &mut ActorContext<Self>,
///     ) -> ActorResult<bool> {
///         // Two monitors of one target are independent and have distinct ids.
///         let first = ctx.monitor(&self.0, Some).expect("running");
///         let second = ctx.monitor(&self.0, Some).expect("running");
///         Ok(first != second)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MonitorId(u64);

impl MonitorId {
    /// A fresh id, unique in the process.
    pub(crate) fn next() -> Self {
        Self(NEXT_MONITOR.fetch_add(1, Ordering::Relaxed))
    }
}

/// The name of one request an actor made with
/// [`request`](crate::actor::context::ActorContext::request).
///
/// The id comes back from the call that made the request and is carried by the
/// [`RequestReply`] that answers it, so an actor with many requests in flight
/// matches each reply to its request. It is opaque, unique in the process, and
/// equal only to itself.
///
/// Erlang/OTP: the request id `gen_server:send_request/2` returns.
///
/// # Examples
/// ```rust
/// use std::collections::HashSet;
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     ActorHandle, ActorResult, RequestId, RequestReply,
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
///     type Message = Option<RequestReply<u32>>;
///     type Response = bool;
///     async fn handle(
///         &mut self,
///         msg: Option<RequestReply<u32>>,
///         ctx: &mut ActorContext<Self>,
///     ) -> ActorResult<bool> {
///         if msg.is_some() {
///             return Ok(false);
///         }
///         // Two requests have two different ids, usable as a set key.
///         let first = ctx.request(&self.0, 1, Some).expect("running");
///         let second = ctx.request(&self.0, 2, Some).expect("running");
///         let ids: HashSet<RequestId> = [first, second].into_iter().collect();
///         Ok(ids.len() == 2)
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let echo = Echo.spawn().await?;
/// let asker = Asker(echo).spawn().await?;
/// assert!(asker.send(None).await?);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

impl RequestId {
    /// The request id that shares `id`'s number: requests draw from the
    /// monitor counter, so the machinery of monitors serves them.
    pub(crate) fn from_monitor(id: MonitorId) -> Self {
        Self(id.0)
    }

    /// The monitor id of the same number.
    pub(crate) fn monitor(self) -> MonitorId {
        MonitorId(self.0)
    }
}

/// The answer to a request, as the actor that asked receives it.
///
/// The asking actor's map function turns it into one of its own messages,
/// which reaches `handle` like any other message, after every message the
/// callee sent the asker before it replied. `result` is what
/// [`ActorHandle::send`](crate::actor::handle::ActorHandle::send) would have
/// returned: the callee's response, or the reason there is none
/// ([`AskError::Actor`] when the handler failed, [`AskError::ResponseDropped`]
/// when the callee ended first, [`AskError::Closed`] when its mailbox was
/// closed, [`AskError::Timeout`] when a deadline passed first).
///
/// Erlang/OTP: the result of `gen_server:wait_response/2`, `{reply, Reply}` or
/// `{error, {Reason, ServerRef}}`.
///
/// # Examples
/// ```rust
/// use tokio_actors::{AskError, RequestReply};
///
/// fn describe(reply: &RequestReply<u32>) -> String {
///     match &reply.result {
///         Ok(value) => format!("{:?} answered {value}", reply.id),
///         Err(AskError::ResponseDropped { reason }) => format!("callee ended: {reason}"),
///         Err(other) => format!("no answer: {other}"),
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct RequestReply<R> {
    /// The request this answers.
    pub id: RequestId,
    /// The callee's response, or why there is none.
    pub result: Result<R, AskError>,
}

/// The notice a monitor delivers when the actor it watches has ended.
///
/// The watcher's map function turns it into one of its own messages, which
/// reaches `handle` like any other message.
///
/// Erlang/OTP: the `{'DOWN', Ref, process, Pid, Reason}` message.
///
/// # Examples
/// ```rust
/// use tokio_actors::{Down, DownReason, StopReason};
///
/// fn describe(down: &Down) -> String {
///     let how = match &down.reason {
///         DownReason::Stopped(StopReason::Graceful) => "finished".to_string(),
///         DownReason::Stopped(other) => format!("ended: {other}"),
///         DownReason::NoProc => "was not running".to_string(),
///         DownReason::Unknown => "was lost".to_string(),
///     };
///     format!("{} {how}", down.actor_id.as_str())
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Down {
    /// The monitor that fired.
    pub monitor: MonitorId,
    /// The watched actor.
    pub actor_id: ActorId,
    /// The run of the watched actor the monitor was bound to when it was
    /// made. `None` only when the key given to `monitor` (an id or a name)
    /// resolved to no actor, so there was no run to name.
    pub incarnation: Option<Incarnation>,
    /// Why the notice was sent.
    pub reason: DownReason,
}

/// Why a [`Down`] was sent.
///
/// # Examples
/// ```rust
/// use tokio_actors::{DownReason, StopReason};
///
/// let reason = DownReason::Stopped(StopReason::Graceful);
/// assert_ne!(reason, DownReason::NoProc);
/// assert_ne!(DownReason::NoProc, DownReason::Unknown);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownReason {
    /// The watched run ended with this reason. It is the reason the run's
    /// status plane carries, so the actor's registered name is free by the
    /// time the notice is delivered.
    Stopped(StopReason),
    /// There was no running actor to watch when the monitor was made: the
    /// id or name matched none, or the watched run had already ended.
    /// Erlang/OTP: `noproc`.
    NoProc,
    /// Never produced by this crate: a local end always carries its reason
    /// (an abort reads [`Kill`](StopReason::Kill)) and there is no
    /// distribution. The variant keeps a match complete over the reasons an
    /// Erlang/OTP monitor can give. Erlang/OTP: `noconnection`.
    Unknown,
}

/// The notice a link delivers to an actor that traps exits, when the actor it
/// is linked to has ended, and the notice an exit signal sent with `exit`
/// delivers.
///
/// The map the actor gave to
/// [`trap_exit`](crate::actor::context::ActorContext::trap_exit) turns it
/// into one of the actor's own messages, which reaches `handle` like any
/// other message. It enters through an ordinary mailbox slot, after every
/// message the ended actor sent before it ended. One type serves a link and
/// an exit signal, as one message shape does in Erlang/OTP.
///
/// Erlang/OTP: the `{'EXIT', From, Reason}` message.
///
/// # Examples
/// ```rust
/// use tokio_actors::{DownReason, LinkExit, StopReason};
///
/// fn describe(exit: &LinkExit) -> String {
///     let how = match &exit.reason {
///         DownReason::Stopped(StopReason::Graceful) => "finished".to_string(),
///         DownReason::Stopped(other) => format!("ended: {other}"),
///         DownReason::NoProc => "was not running".to_string(),
///         DownReason::Unknown => "was lost".to_string(),
///     };
///     format!("{} {how}", exit.actor_id.as_str())
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkExit {
    /// The actor whose end or exit signal this is.
    pub actor_id: ActorId,
    /// The run of that actor the link was bound to. `None` only when the
    /// key given to `link` (an id or a name) resolved to no actor, so there
    /// was no run to name.
    pub incarnation: Option<Incarnation>,
    /// Why the notice was sent. A link to an actor that was not running
    /// when the link was made reads [`DownReason::NoProc`].
    pub reason: DownReason,
}

/// One run of one actor: its id and its [`Incarnation`].
///
/// # Examples
/// ```rust
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     ActorResult, PeerRef,
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
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let idle = Idle.spawn().await?;
/// let peer = PeerRef {
///     actor_id: idle.id().clone(),
///     incarnation: idle.incarnation(),
/// };
/// assert_eq!(peer.clone(), peer);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerRef {
    /// The actor's id.
    pub actor_id: ActorId,
    /// The run of the actor.
    pub incarnation: Incarnation,
}

/// Tags a message that carries a [`Down`] or a [`GroupChange`], so the actor
/// that made the monitor can revoke a notice that is already queued. It is opaque; it only
/// appears in [`Envelope::Signal`].
#[doc(hidden)]
#[derive(Clone)]
pub struct SignalTag {
    pub(crate) shared: Arc<crate::actor::monitor::MonitorShared>,
}

impl std::fmt::Debug for SignalTag {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalTag").finish_non_exhaustive()
    }
}

/// A change to a process group, as a group or scope monitor reports it.
///
/// The watcher's map function turns it into one of its own messages, which
/// reaches `handle` like any other message. A change names the monitor it
/// belongs to, so an actor with several monitors tells them apart.
///
/// Erlang/OTP: the `{Ref, join | leave, Group, Pids}` notifications of
/// `pg:monitor/2` and `pg:monitor_scope/1`.
///
/// # Examples
/// ```rust
/// use tokio_actors::GroupChange;
///
/// fn describe(change: &GroupChange) -> String {
///     match change {
///         GroupChange::Join { group, members, .. } => {
///             format!("{} joined {group}", members.len())
///         }
///         GroupChange::Leave { group, members, .. } => {
///             format!("{} left {group}", members.len())
///         }
///         GroupChange::Resync { groups, .. } => format!("{} groups, as they are now", groups.len()),
///     }
/// }
/// # let _ = describe;
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupChange {
    /// A member joined the group. Each join of one member is its own change.
    Join {
        /// The monitor this change belongs to.
        monitor: MonitorId,
        /// The group that gained the member.
        group: Arc<str>,
        /// The member that joined.
        members: Vec<PeerRef>,
    },
    /// A member left the group, or ended. Each leave of one member is its own
    /// change.
    Leave {
        /// The monitor this change belongs to.
        monitor: MonitorId,
        /// The group that lost the member.
        group: Arc<str>,
        /// The member that left.
        members: Vec<PeerRef>,
    },
    /// The monitor's queue of changes was full, so some changes were not
    /// kept. This is the state of what the monitor watches now: the changes
    /// that followed the overflow come after it. A group monitor's `Resync`
    /// holds its one group (empty when the group has no member); a scope
    /// monitor's holds every group that has a member. One `Resync` follows
    /// each spell of overflow.
    Resync {
        /// The monitor this change belongs to.
        monitor: MonitorId,
        /// Each group with its members as they are now. A member that
        /// joined a group twice is listed twice.
        groups: Vec<(Arc<str>, Vec<PeerRef>)>,
    },
}

// ---------------------------------------------------------------------------
// Supervision types
// ---------------------------------------------------------------------------

/// Strategy used by a supervisor to handle child failures.
///
/// Maps 1:1 to OTP supervisor restart strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
    /// Restart only the failed child. Other children are unaffected.
    OneForOne,
    /// Restart all children when any one fails.
    OneForAll,
    /// Restart the failed child and all children started after it.
    RestForOne,
    /// Dynamic-only variant: same factory type, per-child restart, no ordering guarantees.
    SimpleOneForOne,
}

/// Determines whether a child is restarted after stopping.
///
/// Maps to OTP `permanent`/`transient`/`temporary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartType {
    /// Always restart, even after a graceful stop.
    Permanent,
    /// Restart on crash/cancel/kill, but stay down on a clean end
    /// ([`StopReason::is_normal`]): [`StopReason::Graceful`],
    /// [`StopReason::ParentRequest`], [`StopReason::ParentKilled`] and
    /// [`StopReason::Shutdown`].
    Transient,
    /// Never restart. Removed from the registry on any stop.
    Temporary,
}

/// Policy for how long to wait when stopping a child.
///
/// Maps to OTP `brutal_kill`/`{timeout, T}`/`infinity`.
///
/// # Pairing with the parent's window
/// A parent's own `Shutdown` covers its children only when it is at least
/// the sum of their stop budgets: each child's `Shutdown`, plus its kill
/// grace, plus the short wait that confirms an abort (or the largest of them
/// for a [`SimpleOneForOne`](RestartStrategy::SimpleOneForOne) pool, whose
/// children are stopped at once). A root supervisor stopped by
/// [`ActorSystem::shutdown`](crate::system::ActorSystem::shutdown) gets
/// [`per_actor_timeout`](crate::system::ShutdownPolicy::per_actor_timeout)
/// (5 seconds by default) for its whole teardown. A shorter window escalates
/// the parent to `Kill`, and its children still running then take the path
/// of a killed supervisor's children: workers that do not trap exit signals
/// receive `Kill`, and a child that is itself a supervisor, or a worker that
/// traps, stops in order within its own `Shutdown`.
///
/// Erlang/OTP states a stronger rule for supervisor children: "If the child
/// process is another supervisor, the shutdown time must be set to infinity
/// to give the subtree ample time to shut down." (`supervisor`
/// documentation, child specification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutdown {
    /// Raises `Kill` at once (OTP `brutal_kill`, which needs no grace): the
    /// child ends at its next turn boundary, or its task is aborted once its
    /// kill grace has passed (zero aborts it as soon as the `Kill` is raised).
    Kill,
    /// Wait up to `Duration` for graceful stop, then escalate to Kill. A
    /// duration beyond 30 years is treated as 30 years.
    Timeout(Duration),
    /// Wait indefinitely for the child to stop.
    Infinity,
}

impl Default for Shutdown {
    fn default() -> Self {
        Shutdown::Timeout(Duration::from_secs(5))
    }
}

/// Event delivered to the parent actor when a supervised child stops.
///
/// Enriched with the `SupervisionAction` taken by the runtime.
#[derive(Debug, Clone)]
pub struct ChildEvent {
    /// The ID of the child that stopped.
    pub child_id: ActorId,
    /// The registered name of the child, if any.
    pub child_name: Option<String>,
    /// The reason the child stopped.
    pub reason: StopReason,
    /// The action taken by the supervision runtime.
    pub action: SupervisionAction,
}

/// Action taken by the supervision runtime in response to a child stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisionAction {
    /// A restart has been INITIATED for the child (it completes
    /// asynchronously; the new incarnation may still be starting when the
    /// parent observes this action).
    RestartInitiated,
    /// The supervisor decided not to restart the child in response to this
    /// stop. The child's state when the event is read, and whether its spec
    /// is kept, is visible through
    /// [`ActorContext::children`](crate::actor::context::ActorContext::children):
    /// a Temporary or SimpleOneForOne child's spec is deleted, a child
    /// stopped with `terminate_child` or whose `restart_child` failed keeps
    /// its spec, and a later `restart_child` may already have started it
    /// again. The event's `reason` carries the cause; for a failed
    /// `restart_child` it is the initialization error.
    Removed,
    /// The child was not supervised (no supervision config).
    NotSupervised,
    /// The restart budget was exhausted. Supervisor is stopping.
    BudgetExhausted,
    /// The supervisor is itself stopping: this child's death was still
    /// pending when the supervisor's message loop ended, so it is reported
    /// once before `on_stopped`, and no restart follows.
    SupervisorStopping,
    /// The child is significant and ended without a restart in a way that
    /// shuts the supervisor down under its
    /// [`AutoShutdown`](crate::actor::supervision::AutoShutdown) mode. The
    /// child is not restarted, its spec is removed as for any end without a
    /// restart that removes it, and the supervisor ends with
    /// [`StopReason::ParentRequest`] right after this delivery: the child
    /// deaths still pending are delivered as
    /// [`SupervisorStopping`](Self::SupervisorStopping) before `on_stopped`.
    AutoShutdown,
}

/// Introspection snapshot of an actor's current state.
#[derive(Debug, Clone)]
pub struct ActorStatusInfo {
    /// The actor's unique ID.
    pub id: ActorId,
    /// The actor's registered name, if any.
    pub name: Option<String>,
    /// Current lifecycle status.
    pub status: ActorStatus,
    /// Number of mailbox slots in use: the messages queued plus the slots
    /// senders have reserved and not yet filled.
    pub mailbox_len: usize,
    /// Total mailbox capacity.
    pub mailbox_capacity: usize,
    /// Number of priority-lane slots in use: the messages queued in the lane
    /// plus the slots senders have reserved and not yet filled. `0` when the
    /// actor has no priority lane enabled.
    pub priority_len: usize,
    /// Total capacity of the priority lane. `0` when the actor has no
    /// priority lane enabled.
    pub priority_capacity: usize,
    /// Number of supervised children.
    pub child_count: usize,
    /// Number of active timers.
    pub timer_count: usize,
    /// Number of active streams.
    pub stream_count: usize,
    /// The run of the actor this snapshot describes; equal to the
    /// [`incarnation`](crate::actor::handle::ActorHandle::incarnation) of the
    /// actor's handle.
    pub incarnation: Incarnation,
    /// Number of monitors this actor holds that have not yet been delivered
    /// or revoked.
    pub monitors: usize,
    /// Number of monitors other actors hold on this actor's run that have
    /// not fired or been revoked.
    pub monitored_by: usize,
    /// Whether the actor traps exits: it reads `true` once
    /// [`trap_exit`](crate::actor::context::ActorContext::trap_exit) was
    /// called and `false` again after
    /// [`untrap_exit`](crate::actor::context::ActorContext::untrap_exit),
    /// and always `true` for a supervisor.
    pub trap_exit: bool,
    /// Number of links this actor holds that have not been delivered or
    /// cancelled.
    pub links: usize,
    /// Number of process-group memberships this actor holds, a membership
    /// joined twice counting twice.
    pub pg_groups: usize,
    /// Number of group and scope monitors this actor holds that have not
    /// been revoked.
    pub group_monitors: usize,
    /// Number of aliases this actor made that still accept messages: not
    /// ended with `unalias`, and not reply aliases that took their message.
    pub aliases: usize,
    /// Number of requests this actor has sent with `request` whose answer has
    /// not been handled or cancelled.
    pub requests_in_flight: usize,
    /// Whether the actor is suspended: it takes no message until it is
    /// resumed.
    pub suspended: bool,
    /// The actor's own account of its state, from
    /// [`Actor::format_status`](crate::actor::Actor::format_status); `None`
    /// when the actor gives none, and always `None` for a sensitive actor.
    pub state: Option<String>,
}

/// Introspection snapshot of a supervised child.
#[derive(Debug, Clone)]
pub struct ChildInfo {
    /// The child's unique ID.
    pub id: ActorId,
    /// The child's registered name, if any.
    pub name: Option<String>,
    /// How the child is restarted.
    pub restart_type: RestartType,
    /// How the child is shut down.
    pub shutdown: Shutdown,
    /// True while the child has a started incarnation whose stop, if any,
    /// has not completed.
    pub is_alive: bool,
    /// True while a restart attempt is in flight or a restart decision is
    /// owed.
    pub restart_pending: bool,
    /// True when the child is itself a supervisor, that is when its
    /// configuration enables supervision.
    pub is_supervisor: bool,
}

/// The counts a supervisor's children add up to, as Erlang/OTP's
/// `supervisor:count_children/1` reports them.
///
/// Returned by
/// [`SupervisorRef::count_children`](crate::actor::supervisor_ref::SupervisorRef::count_children).
/// A child counts as active while it has an incarnation alive; one being
/// restarted is not active.
///
/// # Examples
/// ```rust
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     ActorResult, ChildCounts, SupervisorRef,
/// };
///
/// struct Boss;
/// impl Actor for Boss {
///     type Message = ();
///     type Response = ();
///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let boss = Boss.spawn().named("counts-type-doc").supervisor().await?;
/// let counts = SupervisorRef::of(&boss).count_children().await?;
/// assert_eq!(
///     counts,
///     ChildCounts { specs: 0, active: 0, supervisors: 0, workers: 0 }
/// );
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildCounts {
    /// Child specs the supervisor holds, running or not.
    pub specs: usize,
    /// Children with an incarnation alive.
    pub active: usize,
    /// Specs whose child is itself a supervisor.
    pub supervisors: usize,
    /// Specs whose child is not a supervisor.
    pub workers: usize,
}

// ---------------------------------------------------------------------------
// System shutdown types
// ---------------------------------------------------------------------------

/// Outcome of stopping a single actor: the step of the call's own escalation
/// ladder at which the actor reached a terminal status, whoever raised the
/// `Kill` that ended it.
///
/// Reported per actor, in the order the roots were stopped (reverse
/// registration order), in the [`ShutdownReport`] returned by
/// [`ActorSystem::shutdown`](crate::system::ActorSystem::shutdown) (or
/// [`shutdown_with`](crate::system::ActorSystem::shutdown_with)); also returned
/// directly, for a single actor, by
/// [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within).
/// A final-sweep leftover in [`ShutdownReport::swept`] is classified instead by
/// the instant its end became final against the system's sweep deadline (see
/// `swept`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Reached a terminal status before this call raised `StopReason::Kill` on
    /// it, or without its `Kill` being taken: the actor's lane refused it, or
    /// the call found the actor already out of the registry at its `Kill` step
    /// and raised nothing. A final-sweep leftover in [`ShutdownReport::swept`]
    /// is classified instead by the instant its end became final against the
    /// system's sweep deadline (see `swept`).
    Graceful,
    /// Reached a terminal status after this call's `StopReason::Kill` was taken
    /// by the actor's lane. A `Kill` taken before the actor seals its lane is
    /// its final reason (a stop request only strengthens), so this holds even
    /// when the actor had already left the registry and then published
    /// `Stopped` by itself; for that same end, a call that found the actor out
    /// of the registry at its `Kill` step raised nothing and reports
    /// `Graceful`. A final-sweep leftover in [`ShutdownReport::swept`] is
    /// classified instead by the instant its end became final against the
    /// system's sweep deadline (see `swept`).
    Killed,
    /// The actor's task ended without the actor publishing `Stopped`: it was
    /// aborted (it did not react to `Kill` in time, or a `Kill` raised by
    /// another caller was backed by the abort), or it ended outside every
    /// catch. A supervisor reports an aborted end as `Kill`.
    Aborted,
    /// Reached no terminal status within the call's last bound. While the
    /// actor is registered, that bound is the short wait after its abort (a
    /// task that never reaches a cancellation point, see the crate docs on
    /// abort limits). Once the actor has left the registry and is still
    /// finishing its end, nothing is left to abort, and the bound is the
    /// call's own kill wait (the actor's kill grace, or `within` for
    /// `stop_within(StopReason::Kill, within)`) followed by that same short
    /// wait.
    Unresponsive,
}

/// How an actor's stop ended: the step of the escalation ladder at which it
/// reached a terminal status, and the reason it ended with.
///
/// Reported per actor by
/// [`ActorSystem::kill_by_id`](crate::system::ActorSystem::kill_by_id) and,
/// in the [`ShutdownReport`], for every actor a system shutdown stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReport {
    /// The step of the ladder at which the actor ended.
    pub outcome: StopOutcome,
    /// The reason the actor ended with, read when the ladder saw its
    /// terminal status or its closed status plane: an actor whose task was
    /// aborted reads `Some(StopReason::Kill)`. `None` only for
    /// [`StopOutcome::Unresponsive`], an actor that did not end within the
    /// ladder.
    pub reason: Option<StopReason>,
}

/// How a wait for an actor's subtree ended: the answer of
/// [`SubtreeObserver::wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtreeEnd {
    /// The actor and every actor below it have ended.
    Ended,
    /// An actor below it was still running when the stop budgets the subtree
    /// carries ran out: for each child below, its kill grace and the short
    /// wait that confirms an abort, and for a supervisor child its
    /// `Shutdown` window as well. The wait stopped nothing: a later call can
    /// answer `Ended`.
    Unresponsive,
}

/// Report returned by [`ActorSystem::shutdown`](crate::system::ActorSystem::shutdown)
/// and [`ActorSystem::shutdown_with`](crate::system::ActorSystem::shutdown_with):
/// how stopping every root actor registered in the system ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// One entry per root incarnation the shutdown took on: how it ended (its
    /// [`StopOutcome`]) and its final reason. Roots stopped one at a time come
    /// in stop order (reverse registration order); roots handed to the
    /// concurrent force sweep follow, in the order their waits end. Only root
    /// actors are reported here; a non-root leftover that the final sweep
    /// handles is reported in `swept`.
    pub outcomes: Vec<(ActorId, StopReport)>,
    /// Non-root leftovers handled by the final sweep. Normally empty:
    /// supervised children vanish via their own guard drops during each
    /// root's cascade, and roots are always reported in `outcomes`. The
    /// sweep waits on every leftover at once, each wait bounded by the
    /// system's sweep deadline (the earliest deadline of a sweep that started
    /// in a shutdown still in flight), so a supervisor still stopping its
    /// children in order because its own supervisor ended before it did can
    /// finish; the leftovers still running at that deadline are then
    /// force-stopped (`Kill` + `abort`). Each leftover's verdict compares the
    /// instant its end became final with that deadline: one that published
    /// `Stopped` reads `Graceful` with the reason it ended with when that
    /// instant is before the deadline (a `Kill` from its own supervisor
    /// included), and `Killed` when it is at or after it or when this
    /// shutdown's force took a `Kill` on it; one whose task ended without
    /// publishing `Stopped` reads `Aborted`, and one that even the abort did
    /// not end reads `Unresponsive`. A leftover whose wait or force task
    /// failed is reported from its plane by the same rule once it has ended,
    /// `Unresponsive` while it still runs, though the abort the system
    /// scheduled may still end it. The deadline `Kill` and its abort are
    /// scheduled with the system when the sweep begins, on the runtime of each
    /// leftover whose task is already spawned, so they happen even if the walk
    /// is cut short first, and they stand for a later shutdown with a later
    /// deadline.
    pub swept: Vec<(ActorId, StopReport)>,
}

/// Internal system-level messages sent via the system channel.
///
/// These have priority over user messages via `biased; select!`, and sit
/// below the stop lane, which the run loop observes first of all (see the
/// `runtime` module docs). Stop/kill requests do not travel through this
/// channel: they are delivered through the stop lane instead, so they are
/// never delayed by anything queued here. A child's death and a restart
/// attempt's completion likewise never travel here: they are delivered
/// through the supervisor's death plane and restart plane (per-incarnation
/// fate cells and a parent-owned `JoinSet`, respectively) instead.
///
/// The full branch order, highest priority first: stop lane, system channel,
/// restart plane, death plane, verdict signal, forwarder plane, priority lane,
/// mailbox.
#[derive(Debug)]
pub(crate) enum SystemMessage {
    /// Request a status snapshot.
    GetStatus(StatusRequest),
    /// Link, unlink or exit items are waiting in the actor's signal inbox.
    Signals,
    /// A request made through a
    /// [`SupervisorRef`](crate::actor::supervisor_ref::SupervisorRef),
    /// answered at a boundary of the run loop. Boxed so the message stays
    /// the size of a status request.
    Supervise(Box<SuperviseRequest>),
    /// A call of the `sys` bundle, boxed so the channel's slots stay the
    /// size of a status request.
    #[allow(dead_code)]
    Sys(Box<SysCommand>),
}

/// One request of a `SupervisorRef`. Each carries the sender its answer goes
/// to, except [`Settle`](Self::Settle) and [`Start`](Self::Start), whose
/// answers travel inside what they carry.
#[derive(Debug)]
pub(crate) enum SuperviseRequest {
    /// `restart_child`.
    Restart {
        /// The child to restart.
        child: ActorId,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<(), crate::error::SupervisionError>>,
    },
    /// `delete_child`.
    Delete {
        /// The child whose spec is removed.
        child: ActorId,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<(), crate::error::SupervisionError>>,
    },
    /// `which_children`.
    Which {
        /// Where the answer goes.
        reply: oneshot::Sender<Result<Vec<ChildInfo>, crate::error::SupervisionError>>,
    },
    /// `count_children`.
    Count {
        /// Where the answer goes.
        reply: oneshot::Sender<Result<ChildCounts, crate::error::SupervisionError>>,
    },
    /// `terminate_child`: the stop is committed at the boundary, and the
    /// answer goes out when the child and its subtree ended.
    Terminate {
        /// The child to stop.
        child: ActorId,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<(), crate::error::SupervisorRefError>>,
    },
    /// The end of a child stopped by a `Terminate`, seen by the reply helper:
    /// the supervisor finishes the classification of that stop, if its death
    /// arm has not, before the answer goes out.
    Settle {
        /// The child whose stop ended.
        child: ActorId,
        /// The incarnation the stop concerns.
        incarnation: u64,
        /// The kind the stop is classified with.
        kind: crate::actor::supervision::ManualStop,
        /// How the incarnation ended.
        reason: StopReason,
    },
    /// `start_child`: the typed job the ref built, opened at one site by
    /// `supervisor_ref::serve` and never seen by user code.
    Start(Box<dyn std::any::Any + Send>),
}

/// Where the answer to a `sys` command goes.
pub(crate) type Reply<T> = oneshot::Sender<T>;

/// A call of the `sys` bundle, as the system channel carries it. The run
/// loop serves it between two turns.
#[allow(dead_code)]
pub(crate) enum SysCommand {
    /// Stop taking messages.
    Suspend(Reply<()>),
    /// Take messages again.
    Resume(Reply<()>),
    /// Turn the statistics on or off.
    SetStatistics(bool, Reply<()>),
    /// Read the statistics.
    Statistics(Reply<Option<crate::actor::sys::Statistics>>),
    /// Turn the log on with this capacity, or off.
    SetLog(Option<usize>, Reply<()>),
    /// Read the log.
    Log(Reply<Option<Vec<crate::actor::sys::DebugEntry>>>),
    /// Turn the trace on or off.
    SetTrace(bool, Reply<()>),
    /// Install a function under an id.
    Install(
        String,
        Box<dyn FnMut(&crate::actor::sys::DebugEntry) -> crate::actor::sys::Install + Send>,
        Reply<()>,
    ),
    /// Remove the function installed under an id.
    Remove(String, Reply<bool>),
    /// Run a read or a change of the actor's value on its task.
    State(Box<dyn crate::actor::sys_state::StateOp>),
    /// Count a run, for the tests of the request path.
    #[cfg(test)]
    Probe(std::sync::Arc<std::sync::atomic::AtomicUsize>, Reply<()>),
}

impl std::fmt::Debug for SysCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Suspend(_) => "Suspend",
            Self::Resume(_) => "Resume",
            Self::SetStatistics(..) => "SetStatistics",
            Self::Statistics(_) => "Statistics",
            Self::SetLog(..) => "SetLog",
            Self::Log(_) => "Log",
            Self::SetTrace(..) => "SetTrace",
            Self::Install(..) => "Install",
            Self::Remove(..) => "Remove",
            Self::State(_) => "State",
            #[cfg(test)]
            Self::Probe(..) => "Probe",
        };
        f.debug_tuple("SysCommand").field(&name).finish()
    }
}

/// One status request: where its reply goes, and whether the requester is a
/// runtime task. The kind travels with its own request, so the reply to it
/// reads that request's kind whatever else is queued.
#[derive(Debug)]
pub(crate) struct StatusRequest {
    /// Where the snapshot is sent. The snapshot travels boxed: the channel
    /// moves its value several times on the way, and a box moves one pointer.
    pub(crate) reply: oneshot::Sender<Box<ActorStatusInfo>>,
    /// The requester ran inside a runtime task when it queued the request.
    pub(crate) task: bool,
}

/// Internal event describing a child's death, delivered to the parent's
/// system channel.
#[derive(Debug, Clone)]
pub(crate) struct ChildStoppedInternal {
    pub child_id: ActorId,
    pub reason: StopReason,
    /// Incarnation token: the number the supervisor drew for the incarnation
    /// that ended, from the process-wide counter (never 0). Events whose
    /// incarnation matches neither the child's current incarnation nor its
    /// pending restart seq are stale (a superseded instance) and are ignored.
    pub incarnation: u64,
}

/// Terminal outcome of one child incarnation, written by its runner once the
/// child's task has fully exited. Tokio guarantees a task's `JoinHandle`
/// resolves only after every one of that task's own drops - its
/// `RegistryGuard` included - have already run, so a populated fate cell
/// always means the child's registered name is already free.
// A completion-only wait only needs `is_some()`; the fields themselves are
// for a caller that also needs to know why and which incarnation.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct ChildFate {
    /// The child's real stop reason (panic, clean exit, or an abort observed
    /// as `StopReason::Kill`).
    pub reason: StopReason,
    /// The incarnation this fate belongs to.
    pub incarnation: u64,
}

// ---------------------------------------------------------------------------
// Envelope (user messages only - Stop moved to SystemMessage)
// ---------------------------------------------------------------------------

/// Internal representation of actual payloads sent to actors.
#[doc(hidden)]
#[derive(Debug)]
pub enum Envelope<M, R> {
    /// A standard message with an optional responder.
    Message {
        /// The message payload.
        payload: M,
        /// The channel to send the response to (if any).
        responder: Option<oneshot::Sender<Result<R, ActorError>>>,
    },
    /// A message built from a [`Down`] or a [`GroupChange`], carrying the tag
    /// that lets the actor that made the monitor drop it while it is still
    /// queued. It reaches
    /// `handle` exactly as a [`Message`](Envelope::Message) with no responder
    /// does.
    Signal {
        /// Identifies the monitor the notice belongs to.
        tag: SignalTag,
        /// The message the monitor's map function made from the [`Down`].
        payload: M,
    },
}
