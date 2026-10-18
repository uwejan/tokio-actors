//! Error types surfaced by the Tokio Actors runtime.

use thiserror::Error;

use crate::types::{ActorId, StopReason};

/// Result type for actor operations.
pub type ActorResult<T> = Result<T, ActorError>;

/// Errors returned from user-defined actor logic.
///
/// `User` is reserved for actor-authored errors (see [`ActorError::user`]).
/// `Timer`, `Stream`, and `Supervision` carry their source error
/// structurally - matchable as `ActorError::Timer(TimerError::NotFound)` and
/// so on via `?` - instead of collapsing it into a display string.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
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
///   processed. Retry only if the operation is idempotent. It carries the
///   reason the actor stopped with, the same one
///   [`wait_stopped`](crate::actor::handle::ActorHandle::wait_stopped)
///   returns; under a `Kill` it is [`StopReason::Kill`]. It is returned
///   once that reason is final: the actor makes it final when it seals its
///   stop lane, after its children's teardown and before its queued
///   messages, its value and its context are dropped, so the wait has no
///   bound of its own: bound the call with `tokio::time::timeout` when the
///   target's teardown may not return. A status query
///   ([`get_status`](crate::actor::handle::ActorHandle::get_status)) fails
///   this way too.
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
    /// The actor stopped without sending a reply.
    #[error("actor stopped before replying: {reason}")]
    ResponseDropped {
        /// Why the actor stopped.
        reason: StopReason,
    },
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
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// No Tokio runtime was found in the current context.
    #[error("tokio runtime handle not in scope")]
    MissingRuntime,
    /// The configured mailbox capacity, or the configured priority lane
    /// capacity, was zero. Every bounded channel a spawned actor owns - its
    /// mailbox, and its priority lane when one is enabled - must hold at
    /// least one message; `tokio::sync::mpsc::channel` panics on a
    /// zero-sized buffer, so this is caught before either channel is ever
    /// created.
    #[error("mailbox and priority lane capacities must be at least 1, got 0")]
    ZeroMailboxCapacity,
    /// An actor with this id is already registered: a supervised child's id
    /// in use under its supervisor, or a second live registration of an id
    /// in its system.
    #[error("an actor with id `{0}` is already registered")]
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
    /// The actor system `{0}` is not currently accepting registrations: it is
    /// either shutting down (`shutdown`/`shutdown_with` in flight, or dropped
    /// before it completed, which leaves the system shutting down until a later
    /// `shutdown` call finishes the job) or has finished shutting down,
    /// matching OTP's application-controller behavior during stop. A supervised
    /// restart is refused the same way, and charged to the restart budget like
    /// any failed restart, so a supervisor whose child dies in that phase and
    /// is due a restart exhausts a budget whose window holds its `max_restarts`
    /// restarts, while a zero window keeps retrying until a shutdown reaches
    /// the supervisor, unless the restarts read the same instant (see
    /// [`SupervisionConfig::restart_window`](crate::actor::supervision::SupervisionConfig::restart_window)).
    /// This is not permanent -
    /// [`ActorSystem::reactivate`](crate::system::ActorSystem::reactivate)
    /// returns a fully-shut-down system to active once its shutdown has
    /// completed, so a system's name is never poisoned forever. A spawn that
    /// names no system joins the default system, so it is refused the same way
    /// while the default system is not active, until
    /// [`ActorSystem::reactivate`](crate::system::ActorSystem::reactivate) is
    /// called; so is a child's restart in the default system.
    #[error("actor system `{0}` is shutting down")]
    SystemShuttingDown(String),
    /// The actor's `pre_start` or `on_started` returned `Err`, or panicked,
    /// before the actor ever reached the message loop, and no `Kill` reached
    /// its end before its reason was final; or the actor's task panicked
    /// outside every catch during its initialization, before it answered,
    /// and its status plane reads the same failure. Carries the
    /// recorded failure, boxed to avoid an unbounded size cycle with
    /// `ActorError` (`ActorError::Spawn` already holds a `SpawnError` by
    /// value).
    #[error("actor failed to initialize: {0}")]
    Init(Box<ActorError>),
    /// The actor did not finish `pre_start`/`on_started` within the deadline
    /// passed to `.start_timeout()`. The task is aborted immediately (no
    /// lifecycle hook runs afterward), matching OTP's untrappable
    /// `exit(_, kill)` on a start timeout.
    #[error("actor did not finish initializing within the start timeout")]
    StartTimeout,
    /// The actor was killed before its spawn was answered: a `Kill` it
    /// observed during its initialization, a `Kill` that reached the end of
    /// a failed initialization before its reason was final (the
    /// initialization's error is then logged with the `tracing` feature, and
    /// the actor's status plane reads `Kill`), the abort that backs every kill
    /// ([`ActorSystem::kill`](crate::system::ActorSystem::kill),
    /// [`ActorSystem::kill_by_id`](crate::system::ActorSystem::kill_by_id),
    /// [`ActorHandle::stop`](crate::actor::handle::ActorHandle::stop) with
    /// `StopReason::Kill`,
    /// [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within),
    /// a system shutdown), or the abort of its runtime's shutdown. Erlang/OTP
    /// reports the same case to the starter as `{error, killed}`.
    #[error("actor was killed before its spawn was answered")]
    Killed,
    /// [`ActorContext::spawn_child`](crate::actor::context::ActorContext::spawn_child)
    /// was called on an actor with no supervision config. This is the
    /// spawn-time surface of the fact; [`SupervisionError::NotASupervisor`]
    /// reports the same condition to the child-management APIs
    /// (`stop_child`, `terminate_child`, `restart_child`, `delete_child`).
    #[error("actor is not configured as a supervisor")]
    NotASupervisor,
    /// Returned by a supervision call made inside `on_child_stopped` while
    /// the supervisor itself is stopping.
    #[error("the supervisor is stopping")]
    SupervisorStopping,
    /// A previous actor under this id, or an actor it spawned, did not end in
    /// time, so no new actor was started beside it. The wait is bounded by the
    /// longest chain of stop budgets in the subtree (a worker's kill grace, a
    /// supervisor child's `Shutdown` window plus its kill grace), each followed
    /// by a short settle time, plus one more settle time, and unbounded only
    /// while a supervisor child under `Shutdown::Infinity` runs in it or a
    /// budget is too large to represent. Past that bound, the wait gives no
    /// verdict on an actor before the abort the system owes it has had the
    /// settle time to end it: an abort that is late holds the wait, for at most
    /// the bound once more plus one more settle time.
    #[error("a previous actor under `{0}`, or one it spawned, did not end in time")]
    ChildUnresponsive(ActorId),
}

/// Errors emitted by the timer subsystem.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TimerError {
    /// The specified timer ID was not found (already fired or cancelled).
    #[error("timer id not found")]
    NotFound,
    /// A recurring interval of zero would fire without bound and can never
    /// be caught up, so it is rejected when the timer is registered.
    #[error("recurring interval must be greater than zero")]
    ZeroInterval,
}

/// Errors emitted by the stream subsystem.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// The specified stream ID was not found (already finished or cancelled).
    #[error("stream id not found")]
    NotFound,
}

/// Errors emitted by the supervision subsystem.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
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
    /// The child has a restart in flight, is being stopped, owes the decision
    /// about its death, or is a member of a pending group restart; the manual
    /// operation must wait for it to settle.
    #[error("child `{0}` is restarting")]
    ChildRestarting(ActorId),
    /// The child did not end even after its task was aborted - it is stuck in a
    /// non-yielding loop (see the crate docs on abort limits) - or an actor it
    /// spawned did not end in time. The supervisor stops waiting instead of
    /// hanging. A wait for the child's subtree is bounded by the longest chain
    /// of stop budgets in the subtree (a worker's kill grace, a supervisor
    /// child's `Shutdown` window plus its kill grace), each followed by a short
    /// settle time, plus one more settle time, and unbounded only while a
    /// supervisor child under `Shutdown::Infinity` runs in it or a budget is
    /// too large to represent. Past that bound, the wait gives no verdict on an
    /// actor before the abort the system owes it has had the settle time to end
    /// it: an abort that is late holds the wait, for at most the bound once
    /// more plus one more settle time. A child that has ended itself is named
    /// when an actor it spawned is the one that did not end.
    #[error("child `{0}` or an actor it spawned did not end in time")]
    ChildUnresponsive(ActorId),
    /// Returned by a supervision call made inside `on_child_stopped` while
    /// the supervisor itself is stopping.
    #[error("the supervisor is stopping")]
    SupervisorStopping,
}
