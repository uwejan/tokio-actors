//! Core actor traits and identity types.

/// Aliases: names for sending to one run of an actor that its owner can end.
pub(crate) mod alias;
#[cfg(test)]
mod alias_tests;
/// Actor execution context.
pub mod context;
/// Actor handle for external communication.
pub mod handle;
#[cfg(test)]
mod idle_tests;
#[cfg(test)]
mod late_map_panic_tests;
/// Links and the trap flag.
pub(crate) mod link;
#[cfg(test)]
mod link_tests;
/// The actor's normal mailbox.
pub(crate) mod mailbox;
#[cfg(doctest)]
mod message_bounds;
/// Peer monitors: the entries an actor holds and the tasks that watch for
/// the end of the actors they name.
pub(crate) mod monitor;
#[cfg(test)]
mod monitor_tests;
/// Panic capture for actor callbacks.
pub(crate) mod panic;
/// Process groups: scopes, join, leave and members.
pub(crate) mod pg;
/// Group and scope monitors.
pub(crate) mod pg_monitor;
/// Requests in flight.
pub(crate) mod request;
#[cfg(test)]
mod request_tests;
/// Runtime configuration and spawning logic.
pub mod runtime;
#[cfg(test)]
mod size_tests;
#[cfg(test)]
mod spawned_monitor_tests;
/// The start bound of a spawn, owned by the system.
mod start_bound;
/// Subtree tokens: the end of an actor and of everything it spawned.
pub(crate) mod subtree;
/// Supervision configuration and child management.
pub mod supervision;
/// A typed reference to a supervisor, for managing its children from outside.
pub mod supervisor_ref;
#[cfg(test)]
mod supervisor_ref_tests;
#[allow(dead_code)]
pub mod sys;
#[allow(dead_code)]
pub(crate) mod sys_debug;
#[allow(dead_code)]
pub(crate) mod sys_loop;
#[allow(dead_code)]
pub(crate) mod sys_serve;
#[allow(dead_code)]
pub(crate) mod sys_state;
#[allow(dead_code)]
pub(crate) mod sys_suspend;
/// The key of an actor's own task, for the self-call check.
pub(crate) mod task_key;
/// The receive timeout, the idle hook and the message to handle next.
pub(crate) mod turn;

#[cfg(test)]
mod sys_options_tests;

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::error::{ActorResult, SpawnError};
use crate::system::ActorSystem;
use crate::telemetry::Birth;
use crate::types::{ActorId, ChildEvent, Down, Envelope, MonitorId, StopReason};
use context::ActorContext;
use monitor::MapFn;
use runtime::ActorConfig;
use supervision::{SupervisionConfig, DEFAULT_KILL_GRACE};
use sys::{DebugOptions, SysOptions};

/// Primary trait implemented by all actors.
///
/// An actor is a stateful entity that processes messages sequentially.
/// Each actor runs in its own Tokio task. The actor's own typed fields hold
/// what a process dictionary would in Erlang/OTP.
///
/// # The end of an actor
///
/// When an actor's task runs to its end, a supervisor's child factories drop
/// with its children's teardown; then its name is freed, then the answer to a
/// request whose handler failed goes out, then everything else it owned is
/// dropped (its queued messages, its value), and only then does its status
/// read [`ActorStatus::Stopped`](crate::types::ActorStatus::Stopped).
/// Its mailbox stays open while `on_stopped` runs, so a message sent then is
/// accepted and then dropped unhandled with the queued messages; a send after
/// `on_stopped` returns is refused. A
/// [`send`](crate::actor::handle::ActorHandle::send),
/// [`send_priority`](crate::actor::handle::ActorHandle::send_priority),
/// [`send_timeout`](crate::actor::handle::ActorHandle::send_timeout) or
/// [`send_priority_timeout`](crate::actor::handle::ActorHandle::send_priority_timeout)
/// whose slot was taken before the actor ended, and that the actor did not
/// reply to, is answered with
/// [`AskError::ResponseDropped`](crate::error::AskError::ResponseDropped) and
/// the actor's final reason, once that reason is final (a timed form waits for
/// it within its own deadline, and answers
/// `AskError::Timeout { enqueued: true }` if the deadline passes first); one
/// still waiting for room when the mailbox closes is answered
/// [`AskError::Closed`](crate::error::AskError::Closed); one
/// the actor had already replied to reads that reply; one whose handler failed
/// (an `Err` or a panic) is answered
/// [`AskError::Actor`](crate::error::AskError::Actor) when the actor seals its
/// stop lane, with its name already free, or
/// [`AskError::ResponseDropped`](crate::error::AskError::ResponseDropped) with
/// the final reason when a `Kill`, an abort or the end of the actor's runtime
/// ends the actor first. A message that was not handled is dropped at the end,
/// or by its sender as it arrives just after it, also when the actor's runtime
/// has shut down; one that was already landing in the mailbox as the runtime
/// shut down, or that reaches the priority lane through a slot reserved
/// before, is dropped once the last handle to the actor is gone. A reply
/// channel that a caller made itself and put inside a message (not the one
/// send makes) is not answered by the crate: it closes when the message is
/// dropped.
/// The actor's timers and streams are released from
/// the close of its mailbox on, which comes once `on_stopped` has returned, the
/// initialization failed, or a `Kill` ended the run (a `Kill` skips
/// `on_stopped`): each one when its forwarder next runs, and what a forwarder
/// that has not run still owns is dropped by the end, before `Stopped`. The
/// name stays registered while the actor's children may still run, so an
/// observer of such a release may still find the name registered: only
/// `Stopped`, or a closed status plane, means the name is free. The value's
/// `Drop` therefore runs inside the stop bound of whoever stops the actor, and
/// a `Drop` that waits for its own actor's end, through
/// [`wait_stopped`](crate::actor::handle::ActorHandle::wait_stopped) for
/// example, waits forever. The actor's reason is final once its stop lane is
/// sealed, after its children's teardown and its timers' and streams' end and
/// before its queued messages, its value and its context are dropped: it is
/// written into its status plane then, and never changes. A panic in a drop
/// before that point (a timer or stream still registered at the end, a child
/// factory or the supervision state at the teardown, a factory whose last
/// reference a restart in progress then drops) is caught: it keeps an earlier
/// `Kill` or failure as the reason and makes any other reason the panic. An
/// attempt whose spec is removed during its build starts no incarnation; one
/// that finished its build before the removal starts it, and the teardown stops
/// it, or under `Kill` its link guard kills it. A timer or stream cancelled
/// before the end (in `on_stopped`, for example) belongs to the run, and a
/// panic in its drop never changes the reason. A panic in the drop of a queued
/// message, the value or the context comes after the reason is final: it is
/// caught, the end goes on, and `Stopped` is still published. With the
/// `tracing` feature, every caught drop panic that leaves the reason as it
/// stands is logged as a warning naming the actor, a later panic that a step of
/// the end discards beside the one it keeps included. Every log line of the
/// crate is written inside a catch, so a `tracing` subscriber that panics is
/// contained there: it never changes an actor's reason, never cuts its end
/// short, and never aborts the process from a drop. Two aborts are beyond
/// any catch, and Rust always turns them into an abort: a panic hook that
/// itself panics, and a subscriber that panics again while its own panic
/// unwinds (a value in its `event` whose `Drop` panics during that
/// unwind).
///
/// During the run, the drops the actor's loop makes of user values outside a
/// callback are caught too: a response nobody receives (a `notify`, or a
/// `send` whose caller is gone) and the factory of a child whose spec the
/// loop removes (the end of a `Temporary` child, or of a `SimpleOneForOne`
/// child that is not restarted, a group's absorption of a member's end, the
/// classification of a manual stop whose call was dropped). A panic in one of
/// them stops the actor as a panic in `handle` does: with
/// `Failure(Panic(..))`, through `on_stopped` and the ordered stop of its
/// children; a child's end the turn took and did not deliver is
/// delivered once, with the child's name, before `on_stopped`. A factory a
/// supervision call removes (`delete_child`, a `stop_child` or
/// `terminate_child` that removes the spec) drops after that call's last
/// write to the supervisor's state, so a panic there is a panic of the
/// callback that made the call. The drop
/// of a callback's own future once it has completed is part of the
/// callback: a panic there is a panic of the callback, whatever the
/// callback returned.
///
/// When an actor's task is aborted, its status never reads `Stopped`: its
/// status channel closes, which reads as ended, only once its name is free and
/// everything it owned is dropped, the message a callback in progress was
/// handling included. Its reason, `Kill`, is written once its name is free and
/// before its queued messages drop, so a request queued there, or an awaited
/// spawn whose initialization the abort cut, is answered without waiting for
/// those drops. A task aborted before its first poll is the exception: its
/// context writes `Kill` after its value drops and while its name is still
/// registered, so a request queued there can be answered before the name is
/// free, and its awaited spawn is answered last. A panic in one of those drops
/// is caught (and, with the `tracing` feature, logged naming the actor), and
/// the actor ends as killed. An abort drops the timers' and streams' tasks with
/// the actor.
///
/// A panic of a user call in a timer's or stream's task (the timer's
/// factory, the stream's poll, a conversion into the actor's message) or of
/// the runtime itself happens in a poll: it ends that task, is recorded as
/// the actor's
/// [`last_forwarder_error`](crate::actor::context::ActorContext::last_forwarder_error),
/// and never changes the actor's reason. Within the drops of an abort at the
/// actor's end, the first drop panic is that task's panic, classified by
/// registration as above, and a recurring timer's factory or a stream whose
/// drop panics beside it is logged, naming the actor; an abort of the
/// actor's task leaves the first unlogged. A stream that ended during the
/// run has its own drop panic logged when its forwarder does not report it:
/// when a later panic, of the conversion into its `Finished` message or of
/// that message's drop, is the one reported, or when an abort at the
/// actor's end drops the forwarder while its `Finished` waits for room and
/// nothing later panics. The drops of the timers' and streams' tasks at an
/// abort must finish within the short wait that confirms an abort: one that
/// takes longer leaves the actor reading as one that does not yield, so a
/// restart waiting for it ends with `ChildUnresponsive` and `kill_by_id`
/// reports `Unresponsive`. A timer's or stream's `Drop` should therefore not
/// block. A task whose own poll unwinds
/// outside every catch ends in the same order as an aborted one, with the
/// reason `Failure(Panic(..))` instead of `Kill`: its status plane, its
/// supervisor's report of its end and an awaited spawn whose initialization the
/// unwind cut (as [`SpawnError::Init`]) read that one reason.
///
/// A message sent to an actor's mailbox through a slot reserved before the end
/// and pushed after it is dropped by its sender, in that push; a panic of that
/// drop is contained, and logged with the actor's id with the `tracing`
/// feature. A value already landing in the mailbox as the actor ends, or one
/// that reaches the priority lane through a slot reserved before the end, is
/// dropped by a helper task of its system on the actor's runtime while that
/// runtime runs; with no runtime to run the task, it is dropped when the last
/// handle to the actor is. Like any
/// task, that drop runs on one of the runtime's workers and must not block: on
/// a runtime with one worker, a blocking `Drop` delays everything else the
/// runtime runs. The end of every helper task is
/// counted by its system whichever way the task ends; an end by a panic, or by
/// a cancellation that dropped values, is logged only with the `tracing`
/// feature. A priority lane's helper task that was handed a slot which its
/// sender then gave back can stay parked until the last sender of that lane is
/// dropped, which is bounded by the life of the actor's handles and costs no
/// CPU. On some Tokio releases a blocked sender that is
/// handed the slot the actor's last receive freed, and is then dropped, does
/// not wake the receiver.
///
/// # Compile-time contracts
///
/// Every actor, and every message and response type it declares, must be
/// `Send`: an actor can be scheduled onto any worker thread the runtime
/// chooses, so nothing it owns can be tied to a single thread.
///
/// ```compile_fail
/// // error[E0277]: `Rc<()>` cannot be sent between threads safely
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// struct RcActor(std::rc::Rc<()>);
///
/// impl Actor for RcActor {
///     type Message = ();
///     type Response = ();
///
///     async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
/// ```
///
/// ```rust
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// struct ArcActor(std::sync::Arc<()>);
///
/// impl Actor for ArcActor {
///     type Message = ();
///     type Response = ();
///
///     async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
/// ```
///
/// An actor, and the message and response types it declares, must also be
/// `'static`: nothing borrowed from outside can be named in `Self`,
/// `Message`, or `Response`.
///
/// An actor's own fields are reachable only from its task. A message type must
/// be `Send + 'static`, which still admits shared state such as
/// `Arc<Mutex<T>>`, and handler code can reach statics and thread-locals;
/// state shared that way is shared.
///
/// ```compile_fail
/// // error[E0477]: the type `&'a str` does not fulfill the required lifetime
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// struct Holder<'a>(std::marker::PhantomData<&'a ()>);
///
/// impl<'a> Actor for Holder<'a> {
///     type Message = &'a str;
///     type Response = ();
///
///     async fn handle(&mut self, _msg: Self::Message, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
/// ```
///
/// ```rust
/// # use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
/// #
/// struct StaticHolder;
///
/// impl Actor for StaticHolder {
///     type Message = &'static str;
///     type Response = ();
///
///     async fn handle(&mut self, _msg: Self::Message, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
/// ```
pub trait Actor: Sized + Send + 'static {
    /// The type of message this actor receives.
    type Message: Send + 'static;

    /// The type of response this actor produces.
    ///
    /// Use `()` if the actor does not return a response.
    type Response: Send + 'static;

    /// Phase 1: Validation gate. Return Err to prevent actor from starting.
    /// OTP equivalent: `init/1` returning `{stop, Reason}`.
    ///
    /// Implementations that acquire resources must clean up before returning `Err`
    /// (self-cleaning pattern), as `on_stopped` will not be called on init failure.
    ///
    /// A `Kill` already on the actor's stop lane when its task first runs ends
    /// the actor before any start hook begins, so an incarnation's `pre_start`
    /// may never begin; a `Kill` raised while `pre_start` runs skips
    /// `on_started`. A counter in `pre_start` can therefore read fewer runs
    /// than the instances a supervised child's factory built: count the
    /// instances in the factory instead.
    fn pre_start(
        &mut self,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
    }

    /// Phase 2: Post-init setup. Schedule timers, spawn children, acquire resources.
    /// OTP equivalent: `init/1` returning `{ok, State}`.
    ///
    /// Called after `pre_start` succeeds, before the actor enters the message loop.
    fn on_started(
        &mut self,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
    }

    /// Runs when the actor has been idle for the length set with
    /// [`idle_after`](ActorContext::idle_after), once per idle spell.
    ///
    /// A spell starts when the run loop is about to wait with the normal
    /// mailbox and the priority lane empty, and ends when a message starts a
    /// turn. A request on the system channel, such as a status read, neither
    /// ends it nor restarts it. The hook runs on the actor's task, takes no
    /// mailbox slot and leaves the status `Running`; an `Err` or a panic from
    /// it ends the actor as one from `handle` does. The default does nothing.
    ///
    /// Erlang/OTP: the `hibernate_after` trigger without the hibernation. Here
    /// a status read does not restart the spell, where a system message
    /// restarts OTP's `hibernate_after` wait as well as its legacy time-out;
    /// its documentation calls the legacy time-out's restart a flaw.
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// struct Cache {
    ///     flushed: u32,
    /// }
    ///
    /// impl Actor for Cache {
    ///     type Message = ();
    ///     type Response = u32;
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.idle_after(Duration::from_secs(30));
    ///         Ok(())
    ///     }
    ///
    ///     async fn on_idle(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         self.flushed += 1;
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
    ///         Ok(self.flushed)
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let cache = Cache { flushed: 0 }.spawn().await?;
    /// assert_eq!(cache.send(()).await?, 0);
    /// # Ok(())
    /// # }
    /// ```
    fn on_idle(
        &mut self,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
    }

    /// Handles a message sent to the actor.
    ///
    /// This method is called sequentially for each message in the mailbox.
    ///
    /// # The `Err` contract
    ///
    /// A recoverable, expected condition is an `Ok`-encoded domain value in
    /// `Self::Response`, not an `Err`. Returning `Err` means this actor's
    /// invariants are broken: it stops with `StopReason::Failure` on BOTH the
    /// `send` and `notify` paths (cast-exception parity, matching OTP: an
    /// uncaught exception kills the process the same way whether it came from
    /// a `call` or a `cast`). Crash cleanup belongs in [`on_stopped`](Self::on_stopped),
    /// not in `handle` itself.
    ///
    /// To pipeline, as the actor model's `become` does, update state, hand the
    /// rest of the work to a task or a child actor, and take its result later
    /// as a message. To postpone a message, push it onto a bounded queue in the
    /// actor's own fields and replay it from `handle` once the state allows;
    /// the queue sits outside the mailbox bound, so cap it yourself.
    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<Self::Response>> + Send;

    /// Phase 4a: Stop gate. Return `false` to reject graceful, shutdown and parent-requested stops.
    /// Forced stops (`Failure`, `ParentKilled`, `Cancelled`) bypass this hook entirely.
    ///
    /// If it panics, the actor's exit reason is that panic as a `Failure`,
    /// unless a `Failure` request raised while it ran already stands as the
    /// reason, which then stays, or a `Kill` raised while it ran ends the actor
    /// with `Kill`; the panic is recorded as the last error in every case but
    /// the `Kill`.
    fn pre_stop(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = bool> + Send {
        async { true }
    }

    /// Phase 4b: Cleanup. Release resources, notify peers.
    /// OTP equivalent: `terminate(Reason, State)`.
    ///
    /// If it returns `Err` or panics, the actor's exit reason is that failure,
    /// unless a `Failure` already stands as the reason, which then stays, or a
    /// `Kill` raised while it ran, which is the final reason.
    ///
    /// A caller whose request failed in `handle`, and one whose request is
    /// still queued when the actor stops, are answered only after this hook
    /// returns and after the teardown of the actor's children: awaiting such a
    /// caller from here waits on an answer this hook holds back. A child of
    /// this actor that is such a caller cannot take this actor's stop request
    /// while its handler waits for the answer, so this actor's shutdown window
    /// for it runs out and it is killed instead of stopped in order (under
    /// `Shutdown::Infinity` the window never runs out).
    fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
    }

    /// Called when a supervised child actor stops.
    ///
    /// The [`ChildEvent`] contains the child's identity, stop reason, and the
    /// [`SupervisionAction`](crate::types::SupervisionAction) taken by the runtime.
    ///
    /// A child's death, once the task that watches the child has reported it to
    /// the supervisor's death plane, is received ahead
    /// of the messages that child sent before it ended and that are still
    /// queued, because the death plane outranks the priority lane and the
    /// mailbox: this can run before `handle` sees the child's last messages,
    /// where Erlang/OTP delivers a child's last messages to a trapping parent
    /// before its exit by default; an OTP 28 parent that links with the
    /// priority option gets this order.
    ///
    /// Returning `Err` stops this supervisor with that failure, the same
    /// contract [`handle`](Self::handle) has (OTP: a callback that fails
    /// terminates the process).
    ///
    /// A supervisor that stops for any reason other than a kill first
    /// receives, once each, the child deaths still pending when its message
    /// loop ended, with
    /// [`SupervisionAction::SupervisorStopping`](crate::types::SupervisionAction::SupervisorStopping)
    /// and `ctx.status()` reading `Stopping`, before `on_stopped`. No restart
    /// follows, a kill that arrives meanwhile ends the remaining deliveries,
    /// supervision calls made here return
    /// [`SupervisionError::SupervisorStopping`](crate::error::SupervisionError::SupervisorStopping)
    /// (a spawn returns
    /// [`SpawnError::SupervisorStopping`]),
    /// and an `Err` returned here, or a panic, makes the supervisor's exit
    /// reason `Failure` once `on_stopped` has returned, unless a `Failure`
    /// already stands as its exit reason, as an `Err` from `on_stopped` does,
    /// or a `Kill` raised meanwhile ends the actor with `Kill` and no
    /// `on_stopped`; the failure is recorded as its last error in every case,
    /// and the remaining deliveries still run unless that `Kill` ends them. A
    /// child whose exit reason becomes final after the supervisor has
    /// collected the deaths still pending when its message loop ended, one
    /// stopped by the supervisor's own teardown included, is not reported
    /// here; with the `tracing` feature, such an end with an
    /// unexpected reason is logged as an error. A supervisor whose
    /// `pre_start` or `on_started` fails receives none: it never ran its
    /// message loop, as it never receives `on_stopped`; with the `tracing`
    /// feature, a child that ended with an unexpected reason during that
    /// initialization is logged as an error.
    ///
    /// A `SupervisorStopping` error from a supervision call made here is an
    /// `Err` like any other: returned with `?`, it makes the supervisor's
    /// exit reason `Failure`, so a `Transient` supervisor stopped through its
    /// handle, by its parent's `stop_child` or by its exhausted restart
    /// budget is then restarted by its parent (one its parent removes with
    /// `terminate_child` or stops in its own shutdown is not restarted
    /// whatever it returns). Check `event.action` for
    /// `SupervisionAction::SupervisorStopping` before calling.
    fn on_child_stopped(
        &mut self,
        _event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
    }

    /// Whether this actor is sensitive. A sensitive actor refuses
    /// [`inspect`](crate::actor::sys::Sys::inspect) and
    /// [`replace_state`](crate::actor::sys::Sys::replace_state) even when it
    /// was spawned with
    /// [`allow_state_access`](crate::actor::SpawnBuilder::allow_state_access),
    /// reports no [`state`](crate::types::ActorStatusInfo::state), and its
    /// log lines print the kind of an error and never the text derived from
    /// it. [`SpawnBuilder::sensitive`](crate::actor::SpawnBuilder::sensitive)
    /// makes one spawn sensitive without this constant.
    const SENSITIVE: bool = false;

    /// The actor's own account of its state, for
    /// [`ActorStatusInfo::state`](crate::types::ActorStatusInfo::state) and
    /// for the report of a crash. The default is `None`.
    ///
    /// It is a query on `&self`, not a lifecycle hook. It runs on the actor's
    /// task, between two turns, and a panic in it is caught and shown as a
    /// placeholder; with `panic = "abort"` it ends the process. A sensitive
    /// actor's `format_status` is never called.
    /// Erlang/OTP's `format_status/1` returns a map; this returns text.
    fn format_status(&self) -> Option<String> {
        None
    }
}

/// Type alias tying the strongly typed envelope to a concrete actor.
#[doc(hidden)]
pub type ActorEnvelope<A> = Envelope<<A as Actor>::Message, <A as Actor>::Response>;

// ---------------------------------------------------------------------------
// SpawnBuilder - builder chain with IntoFuture
// ---------------------------------------------------------------------------

/// How a [`SpawnBuilder`] observes the actor's initialization outcome.
///
/// Default is [`Await`](SpawnAck::Await): OTP's `start_link` is synchronous
/// with an infinity default timeout, so `.await` on the builder waits for
/// `pre_start` and `on_started` with no deadline unless one is set.
enum SpawnAck {
    /// Await the init ack with no deadline.
    Await,
    /// Skip the ack entirely: `.await` returns as soon as the task is
    /// spawned, before `pre_start` ever runs.
    Detached,
    /// Await the init ack, bounded by a deadline; on expiry the start is cut:
    /// `Kill` is raised on the actor's stop lane and the task is aborted.
    Timeout(Duration),
}

/// Builder for spawning an actor with optional name, system, config, and supervision.
///
/// Created by [`ActorExt::spawn`]. Implements [`IntoFuture`] so you can `.await` it directly.
///
/// # Runtime
///
/// The actor runs on the Tokio runtime current at the spawn, and a spawn with
/// none current returns [`SpawnError::MissingRuntime`] (a configuration with a
/// zero mailbox capacity is refused first, on any runtime). A runtime built
/// without the time driver (`Builder::enable_time` or `enable_all`) runs
/// plain actors (actors with no timer and no supervised child): spawn, send
/// and a graceful stop need no timer. Everything that sleeps needs the
/// driver, and on a runtime without it behaves as
/// follows:
///
/// - a recurring or one-shot timer of an actor panics in its own task at its
///   first poll, and the actor keeps running without it;
/// - a `Kill` aborts the actor's task at once, with no kill grace, because the
///   system's reaper panics as soon as it is given an entry and aborts the
///   tasks it holds as it unwinds;
/// - a call that bounds its wait on the calling runtime, such as
///   [`start_timeout`](Self::start_timeout),
///   [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within)
///   and [`ActorSystem::kill_by_id`](crate::system::ActorSystem::kill_by_id),
///   panics before it changes anything when the calling runtime has no time
///   driver (`stop_within` and `kill_by_id` also panic when no runtime is
///   current); a call that is refused for a reason that needs no timer, or has
///   no target to act on, answers its error, `Err` or `None`, first, on any
///   runtime. Two calls panic later.
///   [`ActorSystem::shutdown_with`](crate::system::ActorSystem::shutdown_with)
///   on a system with a root panics at its first wait on that root, after the
///   system has stopped taking registrations.
///   [`SubtreeObserver::wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended)
///   changes nothing, and with no runtime current or a runtime without the
///   time driver it panics at the first moment it needs a timer: the observed
///   actor has ended and let go of everything it held, and an actor below it
///   has not ended. While the actor still runs, or is still letting go of
///   what it held, it waits without a timer, and a subtree that has ended is
///   answered without one.
///
/// A build with `panic = "abort"` cannot recover from any of these panics:
/// the first timer of the crate on a runtime without the driver ends the
/// process. Build every runtime that runs more than plain actors with
/// `enable_all`.
///
/// # Examples
/// ```no_run
/// # use tokio_actors::{actor::{Actor, ActorExt}, ActorResult, ActorContext};
/// # struct MyActor;
/// # impl Actor for MyActor {
/// #     type Message = ();
/// #     type Response = ();
/// #     fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> impl std::future::Future<Output = ActorResult<()>> + Send { async { Ok(()) } }
/// # }
/// # async fn run() {
/// // Anonymous
/// let h = MyActor.spawn().await.unwrap();
/// // Named
/// let h = MyActor.spawn().named("my-actor").await.unwrap();
/// // Named supervisor (can spawn supervised children via ctx.spawn_child)
/// let h = MyActor.spawn().named("my-actor").supervisor().await.unwrap();
/// # }
/// ```
pub struct SpawnBuilder<A: Actor> {
    actor: A,
    name: Option<String>,
    system: Option<Arc<ActorSystem>>,
    config: ActorConfig,
    ack: SpawnAck,
    kill_grace: Duration,
    sys: SysOptions,
}

impl<A: Actor> std::fmt::Debug for SpawnBuilder<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnBuilder")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl<A: Actor> SpawnBuilder<A> {
    /// Assigns a name to the actor. The name also serves as the [`ActorId`].
    /// Named actors are automatically registered in the actor system.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Targets a specific [`ActorSystem`] for registration.
    pub fn on_system(mut self, system: &Arc<ActorSystem>) -> Self {
        self.system = Some(Arc::clone(system));
        self
    }

    /// Overrides the default [`ActorConfig`]. A supervisor role chosen on
    /// this builder with [`supervisor`](Self::supervisor) or
    /// [`with_supervision`](Self::with_supervision) before this call stays,
    /// unless `config` carries a supervision config of its own, which then
    /// wins; a role chosen after this call replaces whatever `config`
    /// carried.
    pub fn with_config(mut self, config: ActorConfig) -> Self {
        let role = self.config.supervision.take();
        self.config = config;
        if self.config.supervision.is_none() {
            self.config.supervision = role;
        }
        self
    }

    /// Makes this actor a SUPERVISOR with the default config (OneForOne,
    /// 3 restarts / 5s window). A supervisor can spawn supervised children via
    /// [`ActorContext::spawn_child`](crate::actor::context::ActorContext::spawn_child).
    ///
    /// The actor takes the supervisor role; its children are the supervised
    /// ones.
    pub fn supervisor(mut self) -> Self {
        self.config = self.config.supervisor();
        self
    }

    /// Enables supervision with a custom [`SupervisionConfig`].
    pub fn with_supervision(mut self, sup: SupervisionConfig) -> Self {
        self.config = self.config.with_supervision(sup);
        self
    }

    /// Skips the initialization ack entirely: `.await` returns as soon as the
    /// task is spawned, before `pre_start` ever runs. An init failure is
    /// then silent to this caller: the actor
    /// still stops, but nobody here is told why. This does not change how a
    /// supervised child reports init failure to its parent (the runner and
    /// restart budget already cover that); it only changes what an
    /// unsupervised, top-level caller of `.spawn()` observes.
    pub fn detached(mut self) -> Self {
        self.ack = SpawnAck::Detached;
        self
    }

    /// Bounds how long `.await` waits for `pre_start`/`on_started` to finish. A
    /// timeout beyond 30 years is treated as 30 years.
    ///
    /// On expiry the system cuts the start, as Erlang/OTP kills a process
    /// whose start times out: it raises `Kill` on the actor's stop lane and
    /// requests an abort of the task. The abort takes effect when the task
    /// next suspends, and the task is dropped there. The `Kill` is read before
    /// each start hook and at each turn boundary, so once the start has timed
    /// out no hook begins and no message is handled; a hook that is already
    /// running finishes first if it never yields, and the actor then ends with
    /// [`StopReason::Kill`]. Before the caller gets
    /// [`SpawnError::StartTimeout`], this bounded-waits for the aborted task's
    /// own teardown (including its registry guard drop) to finish, so an
    /// immediate retry under the same name does not race a predecessor whose
    /// teardown ends within that short wait.
    ///
    /// The bound holds whether or not the caller keeps waiting: the cut is
    /// carried out by a task of the system on the actor's runtime, so a caller
    /// that drops the future, or stores it and never polls it again, still
    /// finds the actor cut once the timeout has passed if its acknowledgment
    /// was not ready when the system's task checked for it, and its name free
    /// as soon as the task suspends or reads the `Kill`, whichever comes first:
    /// the system waits at most 100 ms for that, and a task that does neither
    /// in that time, one that never yields included, keeps its name longer. A
    /// caller is told [`SpawnError::StartTimeout`] only for an actor whose
    /// start the system cut, or when the system's task ended without deciding
    /// and the actor's end was not final within the abort settle time after the
    /// deadline; the handle is returned only for an actor that runs.
    ///
    /// The caller only waits for the system's answer and has no bound of its
    /// own. The task of the system runs on the caller's runtime, which is the
    /// runtime the actor runs on, so the call needs that runtime to be driven,
    /// not only entered. If that task panics before it answers, the start is
    /// cut before the answer channel closes and the caller is told
    /// `StartTimeout`, unless the task had already accepted the actor's
    /// acknowledgment: then the actor keeps running and the caller receives the
    /// handle. If the runtime ends first, the actor ends with it and nothing is
    /// left to abort.
    ///
    /// OTP parity: `proc_lib:start` with a timeout (`sync_start` in
    /// `proc_lib.erl`) kills the new process in the caller's own receive once
    /// the timeout passes. A caller that died first is covered only by the link
    /// `start_link` makes, which `proc_lib:start` does not. Here the system
    /// takes that step, so it holds whether or not the caller goes on waiting,
    /// and its wait for the aborted task's teardown is at most 100 ms where OTP
    /// waits for the `'DOWN'` without a limit: two declared departures.
    ///
    /// A zero bound acts at once: `Duration::ZERO` spawns the actor, has the
    /// system cut its start as soon as the system's task for the bound runs,
    /// whatever the actor's ack, and answers [`SpawnError::StartTimeout`] once
    /// the abort has settled;
    /// `pre_start` and `on_started` may already have run.
    ///
    /// # Panics
    /// The `.await` panics when it is first polled on a runtime without the
    /// time driver (build it with `Builder::enable_time` or `enable_all`),
    /// before the actor is spawned or registered: the bound sleeps on the
    /// calling runtime's timers. That is the only runtime the check
    /// refuses: a calling runtime that is shutting down or has ended passes
    /// it. A spawn that is refused for a zero mailbox
    /// capacity, a system that is shutting down or a name already taken is
    /// answered with that error first, on any runtime; a name taken between
    /// that look and the claim still panics on a runtime without the driver.
    /// With no runtime current the spawn returns [`SpawnError::MissingRuntime`]
    /// as any spawn does. A caller polled by a foreign executor whose runtime
    /// has ended before the system answered is answered, with a runtime that
    /// has the time driver current, from what the system had decided (the
    /// handle if the start was acknowledged, `SpawnError::StartTimeout` if it
    /// was cut, otherwise the actor's end as `SpawnError::Killed` or
    /// `SpawnError::Init`, or `SpawnError::StartTimeout` if that end is still
    /// not final when the abort settle time after the deadline has passed) and
    /// waits on that runtime's timers only for an end that is not yet final. A
    /// poll with no runtime context panics, as any Tokio timer does, unless
    /// the start was acknowledged.
    /// With `panic = "abort"` there is no check at the entry: the first
    /// timer the call uses on a runtime without the driver ends the process.
    pub fn start_timeout(mut self, timeout: Duration) -> Self {
        self.ack = SpawnAck::Timeout(timeout);
        self
    }

    /// Sets how long this actor is given, after a `Kill` is raised on it, to
    /// reach its next turn boundary before its task is aborted. Default:
    /// 120 ms.
    ///
    /// `Duration::ZERO` aborts the task as soon as the `Kill` is raised, so a
    /// callback in progress is cut at its next `.await` and never observes
    /// the `Kill` itself. The grace applies to every kill of this actor:
    /// [`ActorSystem::kill`](crate::system::ActorSystem::kill),
    /// [`ActorSystem::kill_by_id`](crate::system::ActorSystem::kill_by_id),
    /// [`ActorHandle::stop`](crate::actor::handle::ActorHandle::stop) with
    /// [`StopReason::Kill`], the kill step of
    /// [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within)
    /// and of system shutdown, its supervisor's stop of it, and the kill a
    /// worker that does not trap exit signals receives when its own supervisor
    /// is killed. An actor that is itself a supervisor, or a worker that traps
    /// exit signals, stops in order when its supervisor is killed,
    /// bounded by its own [`Shutdown`](crate::types::Shutdown), and the
    /// grace applies to the `Kill` raised once that window has passed;
    /// [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within)
    /// called with `StopReason::Kill` waits its own `within` instead, for
    /// that call. A grace beyond 30 years is capped there, so in practice it
    /// never aborts. A callback that never yields survives the abort as well
    /// and is reported unresponsive.
    ///
    /// `kill_grace(Duration::ZERO)` gives a kill that cuts a callback at its
    /// next await, the nearest the runtime offers to Erlang/OTP's immediate
    /// kill; a grace beyond any practical bound keeps every turn
    /// whole, and makes every wait that runs on that grace, its parent's stop
    /// budget included, last for the grace, which in practice is as long as
    /// under `Shutdown::Infinity`, while `stop_within(StopReason::Kill, d)`
    /// still aborts after `d`.
    pub fn kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = grace;
        self
    }

    /// Starts the actor with the given debug options on: statistics, the log
    /// and the trace, as the [`sys`] calls would turn them on. A supervised
    /// child restarts with the same options, and each incarnation starts from
    /// them, not from the toggles of the one before.
    pub fn debug(mut self, options: DebugOptions) -> Self {
        self.sys.debug = options;
        self
    }

    /// Lets [`inspect`](sys::Sys::inspect) and
    /// [`replace_state`](sys::Sys::replace_state) reach the actor's value.
    /// State access is closed otherwise. A sensitive actor stays closed.
    pub fn allow_state_access(mut self) -> Self {
        self.sys.allow_state_access = true;
        self
    }

    /// Makes this actor sensitive, as [`Actor::SENSITIVE`] does for every
    /// spawn of its type.
    pub fn sensitive(mut self) -> Self {
        self.sys.sensitive = true;
        self
    }

    /// Monitors the actor for `ctx`'s actor from its first moment, as Erlang/OTP's
    /// `spawn_monitor` does: the notice is made as the spawn returns, so an
    /// actor that ends at once is never missed. `.await` resolves to the
    /// handle and the [`MonitorId`], and the notice reaches `ctx`'s actor
    /// through `map`, as for [`ActorContext::monitor`].
    ///
    /// The monitor is made after the start is acknowledged, and a start that
    /// fails has one outcome: the `.await` returns the [`SpawnError`], no
    /// monitor exists and no notice follows. An actor that ends before the
    /// monitor is made still sends its notice with its own reason, never
    /// `NoProc`. A spawn made with
    /// `.detached()` has no acknowledgment to wait for, so its
    /// monitor is made as soon as the task is spawned, and a start that fails
    /// afterwards is reported to `map` as any other end.
    ///
    /// Chain it after the other options (`named`, `on_system`, `with_config`,
    /// `start_timeout`), as the last call before `.await`; add
    /// `.linked()` on the result to link as well.
    ///
    /// A spawn from an actor that is stopping or has stopped is refused with
    /// [`SpawnError::OwnerStopping`] before anything is spawned, because the
    /// notices a stopping actor is owed were fixed when its stop began.
    ///
    /// Only the root builder has this form. A supervised child has none: after
    /// [`ActorContext::spawn_child`] returns, `ctx.monitor(&child, map)` is
    /// the same monitor, and a child whose first start fails still sends its
    /// notice, because that start is not awaited.
    ///
    /// Erlang/OTP: `erlang:spawn_monitor/1`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Down, DownReason, StopReason,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         // The worker's work is done as soon as it starts.
    ///         ctx.stop(StopReason::Graceful).expect("the lane takes the request");
    ///         Ok(())
    ///     }
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Start,
    ///     Gone(Down),
    /// }
    ///
    /// struct Parent(mpsc::UnboundedSender<Down>);
    ///
    /// impl Actor for Parent {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         match msg {
    ///             Msg::Start => {
    ///                 Worker.spawn().monitored(ctx, Msg::Gone).await.expect("the worker starts");
    ///             }
    ///             Msg::Gone(down) => {
    ///                 let _ = self.0.send(down);
    ///             }
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let (tx, mut downs) = mpsc::unbounded_channel();
    /// let parent = Parent(tx).spawn().await?;
    /// parent.notify(Msg::Start).await?;
    /// let down = downs.recv().await.expect("the monitor fires");
    /// assert_eq!(down.reason, DownReason::Stopped(StopReason::Graceful));
    /// # Ok(())
    /// # }
    /// ```
    pub fn monitored<W, F>(self, ctx: &mut ActorContext<W>, map: F) -> MonitoredSpawn<'_, A, W>
    where
        W: Actor,
        F: Fn(Down) -> W::Message + Send + Sync + 'static,
    {
        MonitoredSpawn {
            inner: self,
            ctx,
            map: Box::new(map),
            linked: false,
        }
    }

    /// Links the actor to `ctx`'s actor from its first moment, as Erlang/OTP's
    /// `spawn_link` does: the link is made as the spawn returns, so an actor
    /// that ends at once is never missed, and one that has already ended by
    /// then delivers its own end reason through the link rules, as if the
    /// link had been there before it ran. `.await` resolves to the handle.
    ///
    /// The link is made after the start is acknowledged, and a start that
    /// fails has one outcome: the `.await` returns the [`SpawnError`], no link
    /// exists and no exit signal follows. A spawn made with
    /// `.detached()` has no acknowledgment to wait for, so its
    /// link is made as soon as the task is spawned.
    ///
    /// An actor that ended before `ctx`'s actor resumed from the start
    /// acknowledgment has already sent its exit: [`ActorContext::unlink`]
    /// answers `false` for it and the exit still arrives.
    ///
    /// Chain it after the other options (`named`, `on_system`, `with_config`,
    /// `start_timeout`), as the last call before `.await`. A spawn from an
    /// actor that is stopping or has stopped is refused with
    /// [`SpawnError::OwnerStopping`] before anything is spawned.
    ///
    /// Only the root builder has this form. After
    /// [`ActorContext::spawn_child`] returns, `ctx.link(&child)` is the same
    /// link.
    ///
    /// Erlang/OTP: `erlang:spawn_link/1`.
    pub fn linked<W: Actor>(self, ctx: &mut ActorContext<W>) -> LinkedSpawn<'_, A, W> {
        LinkedSpawn { inner: self, ctx }
    }
}

/// Whether an actor still takes new monitors and links: the notices a
/// stopping actor is owed were fixed when its stop began.
fn owner_takes_registrations<W: Actor>(ctx: &ActorContext<W>) -> bool {
    !matches!(
        ctx.status(),
        crate::types::ActorStatus::Stopping | crate::types::ActorStatus::Stopped
    )
}

/// A root spawn that monitors the new actor for the spawning actor.
///
/// Created by [`SpawnBuilder::monitored`]. Awaiting it spawns the actor and,
/// once the start is acknowledged, monitors it. It resolves to the handle and
/// the [`MonitorId`], or to the spawn's error with nothing monitored.
pub struct MonitoredSpawn<'ctx, A: Actor, W: Actor> {
    inner: SpawnBuilder<A>,
    ctx: &'ctx mut ActorContext<W>,
    map: Box<MapFn<W::Message>>,
    linked: bool,
}

impl<A: Actor, W: Actor> std::fmt::Debug for MonitoredSpawn<'_, A, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitoredSpawn")
            .field("inner", &self.inner)
            .field("linked", &self.linked)
            .finish_non_exhaustive()
    }
}

impl<A: Actor, W: Actor> MonitoredSpawn<'_, A, W> {
    /// Links the new actor as well as monitoring it, as Erlang/OTP's
    /// `spawn_opt` with the `link` and `monitor` options does. The link is
    /// made right after the monitor, on the same terms: an actor that has
    /// already ended delivers its own end reason through the link rules, and
    /// [`ActorContext::unlink`] answers `false` for it.
    pub fn linked(mut self) -> Self {
        self.linked = true;
        self
    }
}

impl<'ctx, A: Actor, W: Actor> IntoFuture for MonitoredSpawn<'ctx, A, W> {
    type Output = Result<(handle::ActorHandle<A>, MonitorId), SpawnError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'ctx>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Self {
                inner,
                ctx,
                map,
                linked,
            } = self;
            if !owner_takes_registrations(ctx) {
                return Err(SpawnError::OwnerStopping);
            }
            let handle = inner.await?;
            // The spawning actor's own task is the one awaiting, so its stop
            // stage cannot begin between the check above and these calls.
            let made = ctx.monitor_spawned(&handle, map);
            let made = made.map_err(|_| ()).and_then(|id| {
                if linked {
                    ctx.link_spawned(&handle).map_err(|_| ())?;
                }
                Ok(id)
            });
            match made {
                Ok(id) => Ok((handle, id)),
                Err(()) => {
                    debug_assert!(
                        false,
                        "an actor that took a spawn was refused a registration"
                    );
                    let _ = handle.stop(StopReason::Graceful).await;
                    Err(SpawnError::OwnerStopping)
                }
            }
        })
    }
}

/// A root spawn that links the new actor to the spawning actor.
///
/// Created by [`SpawnBuilder::linked`]. Awaiting it spawns the actor and, once
/// the start is acknowledged, links it. It resolves to the handle, or to the
/// spawn's error with nothing linked.
pub struct LinkedSpawn<'ctx, A: Actor, W: Actor> {
    inner: SpawnBuilder<A>,
    ctx: &'ctx mut ActorContext<W>,
}

impl<A: Actor, W: Actor> std::fmt::Debug for LinkedSpawn<'_, A, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkedSpawn")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<'ctx, A: Actor, W: Actor> IntoFuture for LinkedSpawn<'ctx, A, W> {
    type Output = Result<handle::ActorHandle<A>, SpawnError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'ctx>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Self { inner, ctx } = self;
            if !owner_takes_registrations(ctx) {
                return Err(SpawnError::OwnerStopping);
            }
            let handle = inner.await?;
            // As for a monitored spawn, the owner's stop stage cannot begin
            // here, so the link is not refused.
            if ctx.link_spawned(&handle).is_err() {
                debug_assert!(
                    false,
                    "an actor that took a spawn was refused a registration"
                );
                let _ = handle.stop(StopReason::Graceful).await;
                return Err(SpawnError::OwnerStopping);
            }
            Ok(handle)
        })
    }
}

impl<A: Actor> IntoFuture for SpawnBuilder<A> {
    type Output = Result<handle::ActorHandle<A>, SpawnError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            // The caller's `String` is copied once into one shared allocation,
            // which the id, the registry key, the guard and the context all
            // hold.
            let name: Option<Arc<str>> = self.name.map(Arc::from);
            let id_str: ActorId = match &name {
                Some(name) => ActorId::from_shared(name.clone()),
                None => ActorId::anonymous(),
            };

            match self.ack {
                SpawnAck::Detached => runtime::into_actor(
                    id_str,
                    self.actor,
                    self.config,
                    name,
                    self.system,
                    self.kill_grace,
                    self.sys,
                ),
                SpawnAck::Await => {
                    let (tx, rx) = oneshot::channel();
                    let (handle, _join, _subtree) = runtime::spawn_actor_in(
                        id_str,
                        self.actor,
                        self.config,
                        name,
                        self.system,
                        Some(tx),
                        true,
                        self.kill_grace,
                        None,
                        None,
                        Birth::root(),
                        self.sys,
                    )?;
                    match rx.await {
                        Ok(Ok(())) => Ok(handle),
                        Ok(Err(err)) => Err(SpawnError::Init(Box::new(err))),
                        // The ack sender dropped without a reply: the actor
                        // stopped before it finished initializing.
                        Err(_) => Err(closed_ack_error(&handle).await),
                    }
                }
                SpawnAck::Timeout(timeout) => {
                    // What needs no timer is answered first; the timer check
                    // comes next, and everything that changes state after it.
                    runtime::spawn_precheck(&self.config, self.system.as_ref(), name.as_deref())?;
                    crate::time_probe::require_timers_if_in_runtime("SpawnBuilder::start_timeout");
                    let requested = self.system.clone();
                    let (tx, rx) = oneshot::channel();
                    let (handle, join, _subtree) = runtime::spawn_actor_in(
                        id_str.clone(),
                        self.actor,
                        self.config,
                        name,
                        self.system,
                        Some(tx),
                        true,
                        self.kill_grace,
                        None,
                        None,
                        Birth::root(),
                        self.sys,
                    )?;
                    let target = requested.unwrap_or_else(ActorSystem::default);
                    // Boxed, so only a timed spawn carries the bound's state.
                    Box::pin(start_bound::bounded_start(
                        &target, id_str, handle, join, rx, timeout,
                    ))
                    .await
                }
            }
        })
    }
}

/// The error for a spawn whose ack sender dropped without a reply, read from
/// the reason the actor's status plane carries, so the spawn and the plane
/// give one reason. A `Kill` cut the initialization, or reached the end of a
/// failed initialization before the reason was final, and the actor answers
/// it right after sealing its stop lane, before its queued messages and its
/// value drop; or the task was aborted, and the plane reads `Kill`: the spawn
/// is [`SpawnError::Killed`]. The task's own poll unwound outside every catch
/// during the initialization, and the plane reads that panic as a failure:
/// the spawn is [`SpawnError::Init`] with it. The wait ends as soon as the
/// actor's reason is final, so it never waits through those drops.
async fn closed_ack_error<A: Actor>(handle: &handle::ActorHandle<A>) -> SpawnError {
    let mut status_rx = handle.status_rx();
    match crate::system::wait_final_reason(&mut status_rx).await {
        StopReason::Failure(err) => SpawnError::Init(Box::new(err)),
        _ => SpawnError::Killed,
    }
}

// ---------------------------------------------------------------------------
// ActorExt - single spawn() method returning SpawnBuilder
// ---------------------------------------------------------------------------

/// Convenience trait for spawning actors directly from their implementations.
pub trait ActorExt: Actor + Sized {
    /// Creates a [`SpawnBuilder`] for this actor.
    ///
    /// The builder implements [`IntoFuture`], so you can `.await` it directly
    /// for an anonymous spawn, or chain `.named()`, `.on_system()`, `.supervisor()`,
    /// `.with_config()` before awaiting.
    ///
    /// By default `.await` acks the actor's initialization: it does not return
    /// until `pre_start` and `on_started` have both run, with no deadline
    /// unless one is set. An `Err` from either surfaces
    /// here as [`SpawnError::Init`], unless a `Kill` reaches the actor's end
    /// before its reason is final: the spawn then reads [`SpawnError::Killed`],
    /// as the actor's status plane reads `Kill`. Chain `.detached()` for a
    /// fire-and-forget spawn that returns before initialization runs, or
    /// `.start_timeout(dur)` to bound the wait.
    ///
    /// To spawn without waiting, as Erlang/OTP's `spawn_request` does,
    /// `tokio::spawn` the spawn future (it is `Send`) and `notify` its result
    /// back to the actor that asked.
    fn spawn(self) -> SpawnBuilder<Self> {
        SpawnBuilder {
            actor: self,
            name: None,
            system: None,
            config: ActorConfig::default(),
            ack: SpawnAck::Await,
            kill_grace: DEFAULT_KILL_GRACE,
            sys: SysOptions::default(),
        }
    }
}

impl<T> ActorExt for T where T: Actor {}
