//! Shared type definitions used across the Tokio Actors runtime.

use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::error::ActorError;

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
    pub fn next(&self) -> RecurringId {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        RecurringId::new(id)
    }

    pub fn next_stream_id(&self) -> StreamId {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        StreamId::new(id)
    }
}

/// Reason describing why an actor stopped processing messages.
#[derive(Debug, Clone)]
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
    /// The actor's task was aborted (observed via `JoinError::is_cancelled`).
    Cancelled,
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
            StopReason::Kill => write!(f, "killed"),
        }
    }
}

/// Lifecycle status for a running actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActorStatus {
    /// The actor is initializing (running `on_started`).
    Initializing,
    /// The actor is running and processing messages.
    Running,
    /// The actor is stopping (running `on_stopped`).
    Stopping,
    /// The actor has stopped.
    Stopped,
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
    /// Restart on crash/cancel/kill, but stay down on [`StopReason::Graceful`].
    Transient,
    /// Never restart. Removed from the registry on any stop.
    Temporary,
}

/// Policy for how long to wait when stopping a child.
///
/// Maps to OTP `brutal_kill`/`{timeout, T}`/`infinity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutdown {
    /// Force-kill immediately (OTP `brutal_kill`).
    Kill,
    /// Wait up to `Duration` for graceful stop, then escalate to Kill.
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
    /// The child was removed (temporary or graceful transient).
    Removed,
    /// The child was not supervised (no supervision config).
    NotSupervised,
    /// The restart budget was exhausted. Supervisor is stopping.
    BudgetExhausted,
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
    /// Whether the child is currently alive.
    pub is_alive: bool,
    /// Whether a restart is pending for this child.
    pub restart_pending: bool,
}

// ---------------------------------------------------------------------------
// System shutdown types
// ---------------------------------------------------------------------------

/// Outcome of stopping a single root actor during
/// [`ActorSystem::shutdown`](crate::system::ActorSystem::shutdown) (or
/// [`shutdown_with`](crate::system::ActorSystem::shutdown_with)).
///
/// Reported per actor in [`ShutdownReport`], in the order the roots were
/// stopped (reverse registration order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The actor reached a terminal status within the per-actor window after
    /// `StopReason::ParentRequest`.
    Graceful,
    /// The actor did not stop gracefully in time and needed
    /// `StopReason::Kill` to reach a terminal status.
    Killed,
    /// The actor did not react to `Kill` in time and its task had to be
    /// aborted directly.
    Aborted,
    /// The actor survived even the post-abort grace window: a non-yielding
    /// task that never reaches a cancellation point (see the crate docs on
    /// abort limits).
    Unresponsive,
}

/// Report returned by [`ActorSystem::shutdown`](crate::system::ActorSystem::shutdown)
/// and [`ActorSystem::shutdown_with`](crate::system::ActorSystem::shutdown_with):
/// the outcome of stopping every root actor registered in the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Per-actor outcomes, in the order the roots were stopped (reverse
    /// registration order). Only root actors are reported: a supervised
    /// child's fate is decided by its own supervisor's shutdown cascade, not
    /// signalled by the system directly.
    pub outcomes: Vec<(ActorId, StopOutcome)>,
}

/// Internal system-level messages sent via the system channel.
///
/// These have priority over user messages via `biased; select!`.
#[derive(Debug)]
pub(crate) enum SystemMessage {
    /// Stop the actor.
    Stop(StopReason),
    /// Request a status snapshot.
    GetStatus(oneshot::Sender<ActorStatusInfo>),
    /// A child actor has stopped (delivered by the child's watcher task, or
    /// synthesized by the restart task on a restart failure).
    ChildStopped(ChildStoppedInternal),
    /// A restarted child has been spawned (sent by the restart background
    /// task). The parent validates `seq`, spawns the new watcher itself (so
    /// watcher events can never outrun this message on the channel), and
    /// adopts the new incarnation; a stale completion is rejected and its
    /// instance killed.
    RestartComplete {
        /// Monotonic sequence token to discard stale completions.
        seq: u64,
        /// The child that was restarted.
        child_id: ActorId,
        /// The new system channel sender for the restarted child.
        new_system_tx: mpsc::Sender<SystemMessage>,
        /// The join handle of the restarted child's task (the parent wraps it
        /// in a watcher on acceptance).
        new_join: tokio::task::JoinHandle<StopReason>,
    },
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
