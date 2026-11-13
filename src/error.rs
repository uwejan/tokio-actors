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
/// so on via `?` - instead of collapsing it into a display string. A failed
/// call to another actor converts with `?` too: an [`AskError`] or a
/// [`SendError`] becomes [`User`](ActorError::User) with the error's text, so
/// this actor's exit reads as its own failure and not as the other actor's.
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
    /// The actor was linked to an actor that was not running when the link
    /// was made, so it ends with this error unless it traps exits. The
    /// payload names the actor the link was made to. Erlang/OTP: the exit
    /// reason `noproc`.
    #[error("noproc: {0}")]
    NoProc(ActorId),
    /// A supervisor with
    /// [`strict_start`](crate::actor::supervision::SupervisionConfig::with_strict_start)
    /// failed to start because one of the children it spawned while starting
    /// ended its own initialization with an error. It names the child and
    /// carries the child's own error. Erlang/OTP: the supervisor's
    /// start returns `{error, {shutdown, {failed_to_start_child, Id, Reason}}}`.
    #[error("child `{child}` failed to start: {error}")]
    ChildStart {
        /// The id of the child that failed to start.
        child: ActorId,
        /// The error the child's initialization ended with.
        error: Box<ActorError>,
    },
}

impl ActorError {
    /// Creates a new user-defined error.
    pub fn user(message: impl Into<String>) -> Self {
        Self::User(message.into())
    }
}

/// `?` on a call to another actor inside a callback: the error becomes
/// [`ActorError::User`] with the error's text, whatever its variant, so the
/// callback fails this actor with that text. The other actor's own error
/// (`AskError::Actor`) is not passed through: this actor's exit names a
/// failure of its own, never the other actor's panic or failed start. The exit
/// is a [`Failure`](StopReason::Failure), so a `Transient` child that fails
/// this way is restarted, an end of the other actor with
/// [`Shutdown`](StopReason::Shutdown) included.
impl From<AskError> for ActorError {
    fn from(err: AskError) -> Self {
        Self::User(err.to_string())
    }
}

/// `?` on a `notify` to another actor inside a callback: the error becomes
/// [`ActorError::User`] with the error's text, so the callback fails this
/// actor with that text, and a `Transient` child that fails this way is
/// restarted.
impl From<SendError> for ActorError {
    fn from(err: SendError) -> Self {
        Self::User(err.to_string())
    }
}

/// Failures encountered while sending a message asynchronously.
#[derive(Debug, Error, Clone)]
pub enum SendError {
    /// The actor's mailbox is closed: the actor is stopping or has stopped, and
    /// the message was not processed. Its name can stay registered to the
    /// ending incarnation until its teardown ends.
    #[error("mailbox closed")]
    Closed,
    /// No actor is registered under the given name or ID.
    #[error("no actor registered under this name")]
    NotFound,
    /// A waiting send was made from the actor's own task to its own full
    /// mailbox or priority lane: only that task makes room, so the wait could
    /// never end. The message was not sent. Returned only to the actor's own
    /// task; a send with room goes through as from any other task.
    #[error("the actor's own mailbox is full, so its own send could never complete")]
    Full,
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

/// Why [`ActorContext::stop`](crate::actor::context::ActorContext::stop)
/// did not stop the actor.
///
/// Both variants say that nothing was written to the actor's stop lane.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum StopError {
    /// The stop lane takes no request any more because it is sealed or the
    /// actor's task has ended. A callback of a live actor does not meet it:
    /// until its teardown seals the lane, a request is accepted, and from
    /// `on_stopped` it changes nothing.
    #[error("the actor's stop lane is closed")]
    Closed,
    /// The reason was [`StopReason::Kill`]. An actor
    /// does not kill itself through its context: a `Kill` is raised through
    /// a handle or the system, which declare its deadline and arm its
    /// backstop.
    #[error("an actor cannot kill itself through its context")]
    KillRefused,
}

/// Errors reported when awaiting a response from an actor.
///
/// The variants encode distinct facts:
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
///   returned `Err` or it panicked ([`ActorError::Panic`]). It is returned at
///   the same point as `ResponseDropped`, when the actor seals its stop lane:
///   after its `on_stopped` and its children's teardown, with its name
///   already free, as an Erlang/OTP `gen_server:call` caller reads a crash
///   after the process is dead. A lookup by name right after it finds no
///   registration, or the next incarnation if a supervisor restarts the actor;
///   `None` is final for an actor that nothing restarts, and for a supervised
///   one it means either that the next incarnation is not registered yet or
///   that its supervisor will not restart it, so bound the retries. A `Kill`,
///   an abort or the end of the actor's runtime that ends the actor first
///   makes the call `ResponseDropped` with the final reason instead.
/// - [`Timeout`](AskError::Timeout): the caller-supplied deadline
///   ([`send_timeout`](crate::actor::handle::ActorHandle::send_timeout))
///   elapsed before a response arrived. `enqueued: false` behaves like
///   `Closed` (the request never entered the mailbox - safe to retry
///   unconditionally); `enqueued: true` behaves like `ResponseDropped` (the
///   request may already be processed - retry only if idempotent). The
///   handler is never cancelled by the deadline: it keeps running, and a
///   reply that arrives afterward lands in a dropped `oneshot` and is
///   silently discarded.
/// - [`CallingSelf`](AskError::CallingSelf): the call was made from the
///   target actor's own task, so the request was not sent: nothing could
///   answer it.
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
    /// The actor called itself: the request was made from the actor's own
    /// task, so the actor, busy running the callback that made it, could
    /// never have answered. The request was not sent. Erlang/OTP refuses the
    /// same call with `calling_self`.
    #[error("the actor called itself, so the request could never be answered")]
    CallingSelf,
}

/// Failures encountered when spawning a child actor.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// No Tokio runtime was found in the current context.
    #[error("tokio runtime handle not in scope")]
    MissingRuntime,
    /// The configured mailbox capacity, or the configured priority lane
    /// capacity, was zero. Every bounded queue a spawned actor owns - its
    /// mailbox, and its priority lane when one is enabled - must hold at
    /// least one message, so a zero capacity is refused before either queue is
    /// created.
    #[error("mailbox and priority lane capacities must be at least 1, got 0")]
    ZeroMailboxCapacity,
    /// An actor with this id is already registered: a child spec the
    /// supervisor holds under this id, whether the child runs, is being
    /// restarted or is stopped with its spec kept, or a second live
    /// registration of an id in its system.
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
    /// either shutting down (`shutdown`/`shutdown_with` in flight, or cut short
    /// before it completed (its runtime ended, a call that walks in its own
    /// future was dropped, or the call panicked at a wait that needs a driven
    /// runtime with the time driver), which leaves the
    /// system refusing registrations
    /// until a shutdown of the system has completed, an earlier one included)
    /// or has finished shutting down. A supervised
    /// restart is refused the same way, and charged to the restart budget like
    /// any failed restart, so a supervisor whose child dies in that phase and
    /// is due a restart exhausts a budget whose window holds its `max_restarts`
    /// restarts, while a zero window with a `max_restarts` above zero keeps
    /// retrying until a shutdown reaches the supervisor, unless the restarts
    /// read the same instant (see
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
    /// passed to `.start_timeout()`. The start is cut at once, as Erlang/OTP
    /// kills a process whose start times out: `Kill` is raised on the actor's
    /// stop lane and its task is aborted. The abort takes effect when the task
    /// next suspends; once the start has timed out no hook begins and no
    /// message is handled, while a hook that is already running finishes first
    /// if it never yields. It is also the answer when the system's task for the
    /// bound ended without deciding and the actor's end was not final within
    /// the abort settle time after the deadline.
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
    /// The actor that asked for a monitored or linked spawn is stopping or has
    /// stopped. Nothing was spawned: a stopping actor takes no new monitor or
    /// link, because the notices it is owed were fixed when its stop began.
    #[error("the spawning actor is stopping")]
    OwnerStopping,
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
    /// supervisor child's `Shutdown` window plus its kill grace, and for a
    /// worker that has called `trap_exit` its own `Shutdown` window as well),
    /// each followed by a short settle time, plus one more settle time, and
    /// unbounded only while a supervisor child under `Shutdown::Infinity` runs
    /// in it, or a worker under `Infinity` that traps exit signals, or a budget
    /// is too large to represent. Past that bound, the wait gives no verdict on
    /// an actor before the abort the system owes it has had the settle time to
    /// end it: an abort that is late holds the wait, for at most the bound once
    /// more plus one more settle time.
    #[error("a previous actor under `{0}`, or one it spawned, did not end in time")]
    ChildUnresponsive(ActorId),
    /// The supervision setup combines options that Erlang/OTP refuses: a
    /// non-`Never` [`AutoShutdown`](crate::actor::supervision::AutoShutdown)
    /// with `SimpleOneForOne`, a significant child under `AutoShutdown::Never`,
    /// or a significant `Permanent` child. The text names the combination.
    /// Nothing was started and no factory ran.
    #[error("invalid supervision setup: {0}")]
    InvalidSupervision(&'static str),
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

/// Errors emitted by the monitor subsystem.
///
/// # Examples
/// ```rust
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     ActorResult, MonitorError, StopReason,
/// };
///
/// struct Probe(tokio::sync::mpsc::UnboundedSender<Result<(), MonitorError>>);
///
/// impl Actor for Probe {
///     type Message = ();
///     type Response = ();
///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
///     async fn on_stopped(
///         &mut self,
///         _: &StopReason,
///         ctx: &mut ActorContext<Self>,
///     ) -> ActorResult<()> {
///         // The actor is stopping, so it takes no new monitor.
///         let me = ctx.self_handle();
///         let _ = self.0.send(ctx.monitor(&me, |_| ()).map(|_| ()));
///         Ok(())
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
/// let probe = Probe(tx).spawn().await?;
/// probe.stop(StopReason::Graceful).await?;
/// assert_eq!(rx.recv().await, Some(Err(MonitorError::Stopping)));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MonitorError {
    /// The actor is stopping or has stopped, so a new monitor is refused:
    /// the notices it still owes were fixed when its stop began, and one made
    /// now would have no turn to be delivered in.
    #[error("the actor is stopping, so it takes no new monitor")]
    Stopping,
}

/// Why [`ActorContext::request`](crate::actor::context::ActorContext::request)
/// made no request.
///
/// Nothing was sent in either case.
///
/// # Examples
/// ```rust
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     ActorHandle, ActorResult, RequestError,
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
///     type Response = Result<(), RequestError>;
///     async fn handle(
///         &mut self,
///         _: (),
///         ctx: &mut ActorContext<Self>,
///     ) -> ActorResult<Result<(), RequestError>> {
///         // At most one request may be in flight, so the second is refused.
///         ctx.set_request_cap(1);
///         ctx.request(&self.0, 1, |_| ()).expect("the first fits");
///         Ok(ctx.request(&self.0, 2, |_| ()).map(|_| ()))
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let echo = Echo.spawn().await?;
/// let asker = Asker(echo).spawn().await?;
/// assert_eq!(asker.send(()).await?, Err(RequestError::InFlightLimit));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// The actor is stopping or has stopped, so a new request is refused: a
    /// reply still pending when the stop began is dropped, and one made now
    /// would have no turn to be delivered in.
    #[error("the actor is stopping, so it makes no new request")]
    Stopping,
    /// The actor already has as many requests in flight as its cap allows:
    /// the capacity of its mailbox, or the value
    /// [`set_request_cap`](crate::actor::context::ActorContext::set_request_cap)
    /// gave. A request leaves the count when its reply reaches `handle` or it
    /// is cancelled.
    #[error("the actor has as many requests in flight as its cap allows")]
    InFlightLimit,
}

/// Errors emitted by the link subsystem.
///
/// # Examples
/// ```rust
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     ActorResult, LinkError, StopReason,
/// };
///
/// struct Probe(tokio::sync::mpsc::UnboundedSender<Result<(), LinkError>>);
///
/// impl Actor for Probe {
///     type Message = ();
///     type Response = ();
///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
///     async fn on_stopped(
///         &mut self,
///         _: &StopReason,
///         ctx: &mut ActorContext<Self>,
///     ) -> ActorResult<()> {
///         // The actor is stopping, so it takes no new link.
///         let other = ctx.actor_id().clone();
///         let _ = self.0.send(ctx.link(&other));
///         Ok(())
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
/// let probe = Probe(tx).spawn().await?;
/// probe.stop(StopReason::Graceful).await?;
/// assert_eq!(rx.recv().await, Some(Err(LinkError::Stopping)));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// The actor is stopping or has stopped, so a new link is refused: the
    /// exits it still owes were fixed when its stop began, and one made now
    /// would have no turn to be delivered in.
    #[error("the actor is stopping, so it takes no new link")]
    Stopping,
}

/// Errors an exit signal sent with
/// [`ActorContext::exit`](crate::actor::context::ActorContext::exit) or
/// [`exit_priority`](crate::actor::context::ActorContext::exit_priority)
/// returns.
///
/// Erlang/OTP: `exit/2` always delivers the signal; here the signals that
/// wait for a trapping actor are bounded by its mailbox capacity (64 for one
/// that traps without having called `trap_exit`, as a supervisor does).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExitError {
    /// The target traps exit signals, and as many signals already wait for
    /// it as its bound allows: the signal was not sent.
    #[error("the target's exit queue is full")]
    Full,
}

/// Errors a send through an [`Alias`](crate::Alias) returns.
///
/// Erlang/OTP: a message sent through an alias that is not active is dropped
/// without a word; here the send says so.
///
/// # Examples
/// ```rust
/// use tokio_actors::AliasError;
///
/// assert_eq!(AliasError::Inactive.to_string(), "the alias has ended");
/// ```
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AliasError {
    /// The alias has ended: its owner called `unalias`, or it is a reply alias
    /// that took its message. The message was dropped.
    #[error("the alias has ended")]
    Inactive,
    /// The actor's mailbox is closed: the actor is stopping or has stopped, and
    /// every alias of it ends with it.
    #[error("mailbox closed")]
    Closed,
    /// The queue the message needs is full, and the call does not wait: a
    /// `try_` send, or a waiting send made from the actor's own task, which
    /// is the only task that makes room.
    #[error("mailbox full")]
    Full,
}

/// Errors a process group operation returns.
///
/// Erlang/OTP: `pg:join/3` answers `ok` for any pid and removes a dead one a
/// moment later, and `pg:leave/3` answers `not_joined`; this crate refuses a
/// join that cannot succeed, with one outcome for each condition.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PgError {
    /// The handle belongs to an actor of another system than the scope's. A
    /// scope belongs to one system, and only actors of that system can be its
    /// members.
    #[error("the actor belongs to another system than the scope")]
    ForeignSystem,
    /// The run the handle names has ended, or ended while the join was made,
    /// so it was not added: a member that has ended is never listed.
    #[error("the actor has ended, so it cannot join a group")]
    NoProc,
    /// The actor is not a member of the group, so there is nothing to leave.
    #[error("the actor is not a member of the group")]
    NotJoined,
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
    /// child's `Shutdown` window plus its kill grace, and for a worker that has
    /// called `trap_exit` its own `Shutdown` window as well), each followed by
    /// a short settle time, plus one more settle time, and unbounded only while
    /// a supervisor child under `Shutdown::Infinity` runs in it, or a worker
    /// under `Infinity` that traps exit signals, or a budget is too large to
    /// represent. Past that bound, the wait gives no verdict on an actor before
    /// the abort the system owes it has had the settle time to end it: an abort
    /// that is late holds the wait, for at most the bound once more plus one
    /// more settle time. A child that has ended itself is named when an actor
    /// it spawned is the one that did not end.
    #[error("child `{0}` or an actor it spawned did not end in time")]
    ChildUnresponsive(ActorId),
    /// Returned by a supervision call made inside `on_child_stopped` while
    /// the supervisor itself is stopping.
    #[error("the supervisor is stopping")]
    SupervisorStopping,
}

/// Errors of a call made through a
/// [`SupervisorRef`](crate::actor::supervisor_ref::SupervisorRef).
///
/// - [`CallingSelf`](Self::CallingSelf): the call was made from the
///   supervisor's own task, so nothing was sent: the supervisor serves a
///   request only between its callbacks and could never answer it.
/// - [`Gone`](Self::Gone): the supervisor ended before it answered.
/// - [`Timeout`](Self::Timeout) and [`NoTimeDriver`](Self::NoTimeDriver): the
///   reference was given a bound with
///   [`timeout`](crate::actor::supervisor_ref::SupervisorRef::timeout); the
///   call was not answered within it, or the calling runtime has no time
///   driver to keep it.
/// - [`Supervision`](Self::Supervision) and [`Spawn`](Self::Spawn): the
///   supervisor answered with the error the same call has on its context.
///
/// # Examples
/// ```rust
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     ActorResult, StopReason, SupervisorRef, SupervisorRefError,
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
/// let boss = Boss.spawn().named("sref-error-doc").supervisor().await?;
/// let sup = SupervisorRef::of(&boss);
/// boss.stop(StopReason::Graceful).await?;
/// boss.wait_stopped().await;
/// assert_eq!(sup.count_children().await, Err(SupervisorRefError::Gone));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SupervisorRefError {
    /// The call was made from the supervisor's own task; nothing was sent.
    #[error("the call was made from the supervisor's own task")]
    CallingSelf,
    /// The supervisor ended before it answered.
    #[error("the supervisor ended")]
    Gone,
    /// The call was not answered within the bound set with
    /// [`timeout`](crate::actor::supervisor_ref::SupervisorRef::timeout). A
    /// request that was already queued is still served.
    #[error("the call was not answered within its bound")]
    Timeout,
    /// The reference has a bound and the calling runtime has no time driver to
    /// keep it; nothing was sent. Not returned with `panic = "abort"` on a
    /// runtime that lacks the driver, where the check cannot tell and the
    /// call ends the process; with no runtime current it is still returned.
    #[error("the runtime has no time driver")]
    NoTimeDriver,
    /// The child management call failed.
    #[error(transparent)]
    Supervision(#[from] SupervisionError),
    /// The child could not be started.
    #[error(transparent)]
    Spawn(#[from] SpawnError),
}
