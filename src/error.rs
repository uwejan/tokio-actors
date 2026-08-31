//! Error types surfaced by the Tokio Actors runtime.

use thiserror::Error;

use crate::types::ActorId;

/// Result type for actor operations.
pub type ActorResult<T> = Result<T, ActorError>;

/// Errors returned from user-defined actor logic.
///
/// `User` is reserved for actor-authored errors (see [`ActorError::user`]).
/// `Timer`, `Stream`, and `Supervision` carry their source error
/// structurally - matchable as `ActorError::Timer(TimerError::NotFound)` and
/// so on via `?` - instead of collapsing it into a display string.
#[derive(Debug, Error, Clone)]
pub enum ActorError {
    /// A custom error message from the actor.
    #[error("actor logic error: {0}")]
    User(String),
    /// The actor panicked during execution.
    #[error("actor panicked: {0}")]
    Panic(String),
    /// A spawn error occurred.
    #[error(transparent)]
    Spawn(#[from] SpawnError),
    /// A timer operation failed.
    #[error(transparent)]
    Timer(#[from] TimerError),
    /// A stream operation failed.
    #[error(transparent)]
    Stream(#[from] StreamError),
    /// A supervision operation failed.
    #[error(transparent)]
    Supervision(#[from] SupervisionError),
}

impl ActorError {
    /// Creates a new user-defined error.
    pub fn user(message: impl Into<String>) -> Self {
        Self::User(message.into())
    }
}

/// Failures encountered while sending a message asynchronously.
#[derive(Debug, Error, Clone)]
pub enum SendError {
    /// The actor's mailbox is closed (actor stopped).
    #[error("mailbox closed")]
    Closed,
    /// No actor is registered under the given name or ID.
    #[error("no actor registered under this name")]
    NotFound,
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
/// The four variants encode four distinct facts:
/// - [`Closed`](AskError::Closed): the request was never enqueued - it was
///   definitely NOT processed. Safe to retry unconditionally.
/// - [`ResponseDropped`](AskError::ResponseDropped): the actor stopped after
///   the request was enqueued but before replying - it MAY have been partially
///   processed. Retry only if the operation is idempotent.
/// - [`Actor`](AskError::Actor): the handler produced this error - either it
///   returned `Err` or it panicked ([`ActorError::Panic`]).
/// - [`Timeout`](AskError::Timeout): the caller-supplied deadline
///   ([`send_timeout`](crate::actor::handle::ActorHandle::send_timeout))
///   elapsed before a response arrived. `enqueued: false` behaves like
///   `Closed` (the request never entered the mailbox - safe to retry
///   unconditionally); `enqueued: true` behaves like `ResponseDropped` (the
///   request may already be processed - retry only if idempotent). The
///   handler is never cancelled by the deadline: it keeps running, and a
///   reply that arrives afterward lands in a dropped `oneshot` and is
///   silently discarded.
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
    /// The caller-supplied deadline elapsed before a response arrived.
    ///
    /// `enqueued: false` means the request never entered the mailbox -
    /// unconditionally safe to retry, exactly like [`Closed`](AskError::Closed).
    /// `enqueued: true` means the request was accepted before the deadline
    /// fired and may already have been processed - retry only if the
    /// operation is idempotent, exactly like
    /// [`ResponseDropped`](AskError::ResponseDropped). Either way the handler
    /// itself is never cancelled: it runs to completion, and a late reply
    /// lands in a dropped `oneshot` and is discarded.
    #[error("send_timeout deadline elapsed (enqueued: {enqueued})")]
    Timeout {
        /// Whether the request was enqueued into the mailbox before the
        /// deadline elapsed.
        enqueued: bool,
    },
}

/// Failures encountered when spawning a child actor.
#[derive(Debug, Error, Clone)]
pub enum SpawnError {
    /// No Tokio runtime was found in the current context.
    #[error("tokio runtime handle not in scope")]
    MissingRuntime,
    /// The configured mailbox capacity was zero. Every actor's mailbox must
    /// hold at least one message; `tokio::sync::mpsc::channel` panics on a
    /// zero-sized buffer, so this is caught before the channel is ever
    /// created.
    #[error("mailbox capacity must be at least 1, got 0")]
    ZeroMailboxCapacity,
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
    /// The actor system `{0}` is not currently accepting registrations: it
    /// is either shutting down (`shutdown`/`shutdown_with` in flight) or has
    /// finished shutting down, matching OTP's application-controller
    /// behavior during stop. This is not permanent -
    /// [`ActorSystem::reactivate`](crate::system::ActorSystem::reactivate)
    /// returns a fully-shut-down system to active once its shutdown has
    /// completed, so a system's name is never poisoned forever. Un-named,
    /// un-systemed spawns are unaffected - they were never the registry's to
    /// manage.
    #[error("actor system `{0}` is shutting down")]
    SystemShuttingDown(String),
    /// The actor's `pre_start` or `on_started` returned `Err`, or panicked,
    /// before the actor ever reached the message loop. Carries the recorded
    /// failure, boxed to avoid an unbounded size cycle with `ActorError`
    /// (`ActorError::Spawn` already holds a `SpawnError` by value).
    #[error("actor failed to initialize: {0}")]
    Init(Box<ActorError>),
    /// The actor did not finish `pre_start`/`on_started` within the deadline
    /// passed to `.start_timeout()`. The task is aborted immediately (no
    /// lifecycle hook runs afterward), matching OTP's untrappable
    /// `exit(_, kill)` on a start timeout.
    #[error("actor did not finish initializing within the start timeout")]
    StartTimeout,
    /// [`ActorContext::spawn_child`](crate::actor::context::ActorContext::spawn_child)
    /// was called on an actor with no supervision config. This is the
    /// spawn-time surface of the fact; [`SupervisionError::NotASupervisor`]
    /// reports the same condition to the child-management APIs
    /// (`stop_child`, `terminate_child`, `restart_child`, `delete_child`).
    #[error("actor is not configured as a supervisor")]
    NotASupervisor,
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
    /// The actor is not configured as a supervisor. Returned by the child
    /// management APIs (`stop_child`, `terminate_child`, `restart_child`,
    /// `delete_child`); the spawn-time equivalent for
    /// [`ActorContext::spawn_child`](crate::actor::context::ActorContext::spawn_child)
    /// is [`SpawnError::NotASupervisor`].
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
