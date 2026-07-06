//! Error types surfaced by the Tokio Actors runtime.

use thiserror::Error;

use crate::types::ActorId;

/// Result type for actor operations.
pub type ActorResult<T> = Result<T, ActorError>;

/// Errors returned from user-defined actor logic.
#[derive(Debug, Error, Clone)]
pub enum ActorError {
    /// A custom error message from the actor.
    #[error("actor logic error: {0}")]
    User(String),
    /// The actor was cancelled.
    #[error("actor cancelled")]
    Cancelled,
    /// The actor panicked during execution.
    #[error("actor panicked: {0}")]
    Panic(String),
    /// A spawn error occurred.
    #[error(transparent)]
    Spawn(#[from] SpawnError),
}

impl ActorError {
    /// Creates a new user-defined error.
    pub fn user(message: impl Into<String>) -> Self {
        Self::User(message.into())
    }
}

impl From<TimerError> for ActorError {
    fn from(value: TimerError) -> Self {
        ActorError::User(value.to_string())
    }
}

/// Failures encountered while sending a message asynchronously.
#[derive(Debug, Error, Clone)]
pub enum SendError {
    /// The actor's mailbox is closed (actor stopped).
    #[error("mailbox closed")]
    Closed,
}

/// Failures encountered while sending without awaiting capacity.
#[derive(Debug, Error, Clone)]
pub enum TrySendError {
    /// The mailbox is full.
    #[error("mailbox full")]
    Full,
    /// The mailbox is closed (actor stopped).
    #[error("mailbox closed")]
    Closed,
}

/// Errors reported when awaiting a response from an actor.
///
/// The three variants encode three distinct facts:
/// - [`Closed`](AskError::Closed): the request was never enqueued - it was
///   definitely NOT processed. Safe to retry unconditionally.
/// - [`ResponseDropped`](AskError::ResponseDropped): the actor stopped after
///   the request was enqueued but before replying - it MAY have been partially
///   processed. Retry only if the operation is idempotent.
/// - [`Actor`](AskError::Actor): the handler produced this error - either it
///   returned `Err` or it panicked ([`ActorError::Panic`]).
#[derive(Debug, Error, Clone)]
pub enum AskError {
    /// The actor's mailbox was closed before the request could be enqueued.
    #[error("mailbox closed before the request was sent")]
    Closed,
    /// The actor dropped the response channel without sending a reply.
    #[error("actor stopped before replying")]
    ResponseDropped,
    /// The actor's handler returned an error or panicked.
    #[error("actor returned error: {0}")]
    Actor(ActorError),
}

/// Failures encountered when spawning a child actor.
#[derive(Debug, Error, Clone)]
pub enum SpawnError {
    /// No Tokio runtime was found in the current context.
    #[error("tokio runtime handle not in scope")]
    MissingRuntime,
    /// A child with the same ID is already registered.
    #[error("child actor `{0}` already registered")]
    DuplicateChild(ActorId),
    /// An actor with the given name is already registered in the system.
    #[error("actor name `{name}` already taken in system `{system}`")]
    NameTaken {
        /// The actor name that was already taken.
        name: String,
        /// The system in which the collision occurred.
        system: String,
    },
    /// An actor system with the given name already exists.
    #[error("actor system `{0}` already exists")]
    SystemNameTaken(String),
}

/// Errors emitted by the timer subsystem.
#[derive(Debug, Error, Clone)]
pub enum TimerError {
    /// The specified timer ID was not found (already fired or cancelled).
    #[error("timer id not found")]
    NotFound,
}

/// Errors emitted by the stream subsystem.
#[derive(Debug, Error, Clone)]
pub enum StreamError {
    /// The specified stream ID was not found (already finished or cancelled).
    #[error("stream id not found")]
    NotFound,
}

impl From<StreamError> for ActorError {
    fn from(value: StreamError) -> Self {
        ActorError::User(value.to_string())
    }
}

/// Errors emitted by the supervision subsystem.
#[derive(Debug, Error, Clone)]
pub enum SupervisionError {
    /// The specified child was not found.
    #[error("child `{0}` not found")]
    ChildNotFound(ActorId),
    /// The restart budget has been exhausted.
    #[error("restart budget exhausted")]
    BudgetExhausted,
    /// The factory closure failed during a restart attempt.
    #[error("factory failed: {0}")]
    FactoryFailed(String),
    /// The actor is not configured as a supervisor.
    #[error("actor is not configured as a supervisor")]
    NotASupervisor,
    /// The operation requires the child to be stopped, but it is running.
    #[error("child `{0}` is running")]
    ChildRunning(ActorId),
    /// The child has a restart in flight or is a member of a pending group
    /// restart; manual operations must wait for it to settle.
    #[error("child `{0}` is restarting")]
    ChildRestarting(ActorId),
    /// The child did not terminate within the kill grace even after its task
    /// was aborted - it is stuck in a non-yielding loop (see the crate docs on
    /// abort limits). The supervisor stops waiting instead of hanging.
    #[error("child `{0}` is unresponsive to kill")]
    ChildUnresponsive(ActorId),
}

impl From<SupervisionError> for ActorError {
    fn from(value: SupervisionError) -> Self {
        ActorError::User(value.to_string())
    }
}
