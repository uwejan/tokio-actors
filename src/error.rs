//! Error types surfaced by the MSPC actor runtime.

use thiserror::Error;

use crate::types::ActorId;

/// Result type for actor operations.
pub type ActorResult<T> = Result<T, ActorError>;

/// Errors returned from user-defined actor logic.
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

impl From<SpawnError> for ActorError {
    fn from(value: SpawnError) -> Self {
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
#[derive(Debug, Error, Clone)]
pub enum AskError {
    /// Failed to send the request.
    #[error(transparent)]
    Send(#[from] SendError),
    /// The actor dropped the response channel without sending a reply.
    #[error("response channel dropped")]
    ResponseDropped,
    /// The actor returned an error.
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
}

/// Errors emitted by the timer subsystem.
#[derive(Debug, Error, Clone)]
pub enum TimerError {
    /// The specified timer ID was not found (already fired or cancelled).
    #[error("timer id not found")]
    NotFound,
}
