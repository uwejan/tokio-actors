//! Shared type definitions used across the Tokio Actors runtime.

mod stop_lane;
mod turn_clock;

use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::error::ActorError;

pub(crate) use stop_lane::{LaneState, RaiseResult, StopLane, StopSeverity};
pub(crate) use turn_clock::TurnClock;

/// Unique identifier assigned to each actor within the system.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ActorId(Arc<str>);

impl ActorId {
    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
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

/// Scheduling policies supported by actor timers.
#[derive(Debug, Clone, Copy)]
pub enum SchedulePolicy {
    /// Fire once at the specified time.
    Once,
    /// Fire repeatedly at the specified interval.
    Recurring {
        /// The interval between ticks.
        interval: Duration,
        /// The policy for handling missed ticks.
        miss_policy: MissPolicy,
    },
}

/// Strategy for handling timer drift when recurring timers miss their scheduled time.
#[derive(Debug, Clone, Copy)]
pub enum MissPolicy {
    /// Skip missed ticks and schedule the next tick at the next aligned interval.
    /// If multiple intervals have passed, they are all skipped.
    /// Example: If a 1s timer is delayed by 3.5s, the next tick happens at 4s.
    Skip,

    /// Send all missed messages immediately (catch up), then resume normal schedule.
    /// This ensures no messages are lost but may cause bursts of activity.
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
    ParentKilled,
    /// The actor was forcibly killed. Bypasses ALL lifecycle hooks.
    /// Only `RegistryGuard::drop` fires for cleanup.
    /// OTP equivalent: `exit(Pid, kill)` (untrappable).
    Kill,
}

impl Display for StopReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::Graceful => write!(f, "graceful"),
            StopReason::ParentRequest => write!(f, "parent requested"),
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

    /// The reason a reader takes from a plane it found terminal: the one
    /// written, or `Kill` when the plane closed without one.
    pub(crate) fn reason_or_kill(&self) -> StopReason {
        self.reason.clone().unwrap_or(StopReason::Kill)
    }

    /// Writes the plane's reason, once, and stamps the instant of that
    /// write: a plane that already carries a reason is left exactly as it
    /// is, its stamp included. With `stopped` the same write publishes the
    /// terminal status, so no reader sees `Stopped` without its reason.
    /// Succeeds with no receivers left. Returns whether this call wrote.
    pub(crate) fn write_reason(
        tx: &tokio::sync::watch::Sender<StatusValue>,
        reason: StopReason,
        stopped: bool,
    ) -> bool {
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
    /// Restart on crash/cancel/kill, but stay down on [`StopReason::Graceful`],
    /// [`StopReason::ParentRequest`] and [`StopReason::ParentKilled`].
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
/// of a killed supervisor's children: workers receive `Kill`, and a child
/// that is itself a supervisor stops in order within its own `Shutdown`.
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
    /// Current number of messages in the mailbox.
    pub mailbox_len: usize,
    /// Total mailbox capacity.
    pub mailbox_capacity: usize,
    /// Current number of messages queued in the priority lane. `0` when the
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

/// Report returned by [`ActorSystem::shutdown`](crate::system::ActorSystem::shutdown)
/// and [`ActorSystem::shutdown_with`](crate::system::ActorSystem::shutdown_with):
/// how stopping every root actor registered in the system ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// One entry per root incarnation the shutdown took on: how it ended (its
    /// [`StopOutcome`]) and its final reason. Roots stopped one at a time come
    /// in stop order (reverse registration order); roots handed to the
    /// concurrent force sweep follow, in the order their waits end. Only root
    /// actors are reported here: a supervised child's fate is decided by its
    /// own supervisor's shutdown cascade, not signalled by the system directly.
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
    /// scheduled with the system when the sweep begins, so they happen even
    /// if the shutdown future is dropped first, and they stand for a later
    /// shutdown with a later deadline.
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
    GetStatus(oneshot::Sender<ActorStatusInfo>),
}

/// Internal event describing a child's death, delivered to the parent's
/// system channel.
#[derive(Debug, Clone)]
pub(crate) struct ChildStoppedInternal {
    pub child_id: ActorId,
    pub reason: StopReason,
    /// Incarnation token: 0 for the initial spawn, the restart `seq` after an
    /// accepted restart. Events whose incarnation matches neither the child's
    /// current incarnation nor its pending restart seq are stale (a superseded
    /// instance) and are ignored.
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
#[derive(Debug)]
pub enum Envelope<M, R> {
    /// A standard message with an optional responder.
    Message {
        /// The message payload.
        payload: M,
        /// The channel to send the response to (if any).
        responder: Option<oneshot::Sender<Result<R, ActorError>>>,
    },
}
