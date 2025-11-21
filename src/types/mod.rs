//! Shared type definitions used across the Tokio Actors runtime.

use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

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

/// Incrementing ID source for timers.
#[derive(Default)]
pub(crate) struct RecurringIdGenerator {
    next: AtomicU64,
}

impl RecurringIdGenerator {
    pub fn next(&self) -> RecurringId {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        RecurringId::new(id)
    }
}

/// Reason describing why an actor stopped processing messages.
/// Reason describing why an actor stopped processing messages.
#[derive(Debug, Clone)]
pub enum StopReason {
    /// The actor stopped gracefully (e.g., finished its work).
    Graceful,
    /// The parent actor requested this actor to stop.
    ParentRequest,
    /// The actor stopped due to a failure (panic or error).
    Failure(ActorError),
    /// The actor was cancelled.
    Cancelled,
}

impl Display for StopReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::Graceful => write!(f, "graceful"),
            StopReason::ParentRequest => write!(f, "parent requested"),
            StopReason::Failure(err) => write!(f, "failure: {err}"),
            StopReason::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Lifecycle status for a running actor.
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

/// Internal representation of actual payloads sent to actors.
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
    /// A signal to stop the actor.
    Stop(StopReason),
}
