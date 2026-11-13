//! Handle-based communication API for actors.
//!
//! An [`ActorHandle`](crate::actor::handle::ActorHandle) exposes two
//! independent observability planes, after Erlang/OTP's split between
//! `sys:get_status/1` and `process_info/2`; unlike `sys:get_status`,
//! `get_status` is answered ahead of queued messages:
//! - The queue plane ([`get_status`](crate::actor::handle::ActorHandle::get_status))
//!   travels through the actor's system channel: it returns the richer
//!   [`ActorStatusInfo`](crate::types::ActorStatusInfo) snapshot, but hangs if
//!   the actor is stuck and never services that channel.
//! - The runtime plane ([`status`](crate::actor::handle::ActorHandle::status)
//!   and [`wait_stopped`](crate::actor::handle::ActorHandle::wait_stopped))
//!   reads a per-actor cell, with a version, written directly by the runtime:
//!   `status` answers without the actor's help, even for a hung actor, and
//!   `wait_stopped` resolves when the actor ends, an abort included. The cell
//!   carries only the bare [`ActorStatus`](crate::types::ActorStatus) and,
//!   once the actor has ended, the [`StopReason`] it ended with.
//!
//! `send` and `get_status` share the same self-call hazard: awaiting either
//! of them from inside the actor's own callback, targeting
//! `ctx.self_handle()`, deadlocks the actor against itself. See the
//! "Deadlock warning" section on each method.

use std::future::Future;
use std::pin::{pin, Pin};
#[cfg(feature = "tracing")]
use std::sync::atomic::Ordering;
use std::sync::atomic::{fence, AtomicBool};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, Instant};

use crate::actor::mailbox::{self, MailTx};
use crate::actor::panic::{
    contain_waker_panic, reserve_held, send_contained, try_reserve_held, WakeHold,
};
use crate::actor::runtime::saturating_deadline;
use crate::actor::subtree::{AbortRecord, SubtreeWait, SubtreeWatch};
use crate::actor::supervision::rules::{successor_wait_step, WaitAction};
use crate::actor::supervision::{ABORT_SETTLE, DEFAULT_KILL_GRACE};
use crate::actor::sys::Sys;
use crate::actor::{Actor, ActorEnvelope};
use crate::error::{ActorError, AskError, SendError, TrySendError};
use crate::helper::{HelperKind, HelperOwner};
use crate::system::{
    raise_kill_declared, read_status, terminal_outcome, wait_closed, wait_final_reason,
    wait_reason, wait_terminal, wait_terminal_until, ActorSystem, KillKey, LossOwes, ReaperEntry,
    StopStep,
};
use crate::telemetry::wait_graph::{begin_wait, wait_wanted, WaitEdge};
use crate::telemetry::{EventKind, RaiseOutcome, StopOrigin, TelemetryEvent};
use crate::types::{
    ActorId, ActorStatus, ActorStatusInfo, Envelope, Incarnation, RaiseResult, SignalTag, StatusRx,
    StopLane, StopOutcome, StopReason, SubtreeEnd,
};

/// Queues the event for a stop request a caller made, after the lane
/// answered: the answer is the lane's own, never a prediction of the actor's
/// fate. The incarnation is found through the lane in hand, in the system the
/// caller holds or the one the lane's record names; a lane with no identity
/// raises nothing. The caller goes on with the stop's own steps at once, since
/// no handler runs here.
///
/// `origin` says where the request came from, and `cause` is the reason a
/// `Kill` raised for an exit signal carries, which the actor ends with.
pub(crate) fn emit_stop_requested(
    system: Option<&ActorSystem>,
    lane: &StopLane,
    reason: &StopReason,
    result: RaiseResult,
    origin: StopOrigin,
    cause: Option<&StopReason>,
) {
    crate::telemetry::push_about(
        lane.abort_record(),
        system,
        EventKind::StopRequested,
        |actor| TelemetryEvent::StopRequested {
            actor: actor.clone(),
            reason: reason.clone(),
            outcome: RaiseOutcome::from(result),
            origin,
            cause: cause.cloned(),
        },
    );
}

/// Whether the queue is closed, read right after a request's push.
///
/// The full fence orders the push before the read. The end of the queue
/// closes it, runs the same fence, then drains it (`close_and_drain` in the
/// runtime): in the order of the two fences, either the push comes before the
/// end's fence and the drain takes the request, or the close comes before the
/// request's fence and this read sees the queue closed. No request is left
/// unseen by both.
fn queue_closed_after_push(is_closed: impl FnOnce() -> bool) -> bool {
    fence(std::sync::atomic::Ordering::SeqCst);
    is_closed()
}

/// The answer a read of a request's reply channel holds, if it holds one.
fn settled<R>(
    read: Result<Result<R, ActorError>, oneshot::error::TryRecvError>,
) -> Option<Result<R, AskError>> {
    match read {
        Ok(Ok(resp)) => Some(Ok(resp)),
        Ok(Err(err)) => Some(Err(AskError::Actor(err))),
        Err(_) => None,
    }
}

/// A wait on another actor that the wait graph may record: it polls the
/// future it wraps and, when that first returns `Pending`, asks the graph to
/// record an edge from the calling hook's actor to the actor behind `record`.
/// The edge goes when the wrapper drops, whether the wait completed, was
/// cancelled or was aborted with its task. It changes neither the result nor
/// the order of any poll of the wrapped future. A send with a deadline does
/// not use it.
struct Edged<'a, F> {
    fut: Pin<&'a mut F>,
    record: &'a AbortRecord,
    /// Set by the first `Pending`, whether or not an edge was recorded.
    parked: bool,
    edge: Option<WaitEdge<'a>>,
}

/// The wait-edge wrappers this thread has made, for tests that assert a send
/// with detection off makes none.
#[cfg(test)]
pub(crate) mod edge_probe {
    use std::cell::Cell;

    thread_local! {
        static MADE: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn made() -> u64 {
        MADE.with(Cell::get)
    }

    pub(super) fn note() {
        MADE.with(|made| made.set(made.get() + 1));
    }
}

impl<'a, F> Edged<'a, F> {
    fn new(fut: Pin<&'a mut F>, record: &'a AbortRecord) -> Self {
        #[cfg(test)]
        edge_probe::note();
        Self {
            fut,
            record,
            parked: false,
            edge: None,
        }
    }
}

impl<F: Future> Future for Edged<'_, F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let polled = self.fut.as_mut().poll(cx);
        if polled.is_pending() {
            if !self.parked {
                self.parked = true;
                if wait_wanted(self.record) {
                    self.edge = begin_wait(self.record);
                }
            }
        } else if self.parked {
            self.edge = None;
        }
        polled
    }
}

/// Cloneable handle that callers use to communicate with an actor.
///
/// Two handles are equal when they have the same [`ActorId`] and the actor
/// was spawned in the same system, which makes a supervised child's handle
/// equal to itself across a restart. The system is told apart by its identity,
/// not its name: a system that was shut down and dropped does not make a later
/// system of the same name its successor, so handles of the two are never equal.
/// A handle does not keep its system alive. A `Kill` through the handle of an
/// actor that has ended, one whose system has been freed included, is refused
/// with `SendError::Closed`.
pub struct ActorHandle<A: Actor> {
    id: ActorId,
    tx: MailTx<ActorEnvelope<A>>,
    /// Sender for the actor's priority lane. `None` on every actor spawned
    /// without one ([`MailboxConfig::with_priority_lane`](crate::actor::runtime::MailboxConfig::with_priority_lane)
    /// or the equivalent [`ActorConfig`](crate::actor::runtime::ActorConfig)
    /// builder), in which case the four `*_priority` methods below degrade
    /// to their normal-lane siblings.
    priority_tx: Option<mpsc::Sender<ActorEnvelope<A>>>,
    stop_lane: StopLane,
    mailbox_capacity: usize,
    status_rx: StatusRx,
    /// The state of the priority lane the handle shares with every clone:
    /// the "lane full" warning flag and the gate that leaves the lane to
    /// aliases. `None` on an actor without a lane. One pointer: the gate has
    /// no field of its own.
    lane_flags: Option<Arc<LaneFlags>>,
}

/// What the handles of an actor share about its priority lane.
#[derive(Debug)]
pub(crate) struct LaneFlags {
    /// Set once a priority send has already had to wait for capacity, so the
    /// "lane full" warning fires once per full spell instead of once per
    /// blocked send. Read and written only under the `tracing` feature.
    #[cfg_attr(not(feature = "tracing"), allow(dead_code))]
    warned: AtomicBool,
    /// The lane belongs to priority aliases: the four `*_priority` methods of
    /// a handle use the ordinary mailbox, as on an actor with no lane.
    by_alias: bool,
}

impl LaneFlags {
    /// The flags of a lane; `by_alias` leaves the lane to priority aliases.
    pub(crate) fn new(by_alias: bool) -> Self {
        Self {
            warned: AtomicBool::new(false),
            by_alias,
        }
    }
}

impl<A: Actor> std::fmt::Debug for ActorHandle<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorHandle")
            .field("id", &self.id)
            .field(
                "system",
                &self
                    .stop_lane
                    .abort_record()
                    .mode()
                    .map_or("", |mode| mode.system_name()),
            )
            .finish_non_exhaustive()
    }
}

impl<A: Actor> Clone for ActorHandle<A> {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            tx: self.tx.clone(),
            priority_tx: self.priority_tx.clone(),
            stop_lane: self.stop_lane.clone(),
            mailbox_capacity: self.mailbox_capacity,
            status_rx: self.status_rx.clone(),
            lane_flags: self.lane_flags.clone(),
        }
    }
}

impl<A: Actor> PartialEq for ActorHandle<A> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.system_generation() == other.system_generation()
    }
}

impl<A: Actor> Eq for ActorHandle<A> {}

impl<A: Actor> std::hash::Hash for ActorHandle<A> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.system_generation().hash(state);
    }
}

impl<A: Actor> ActorHandle<A> {
    /// A handle whose lane flags are made from the lane's warning flag, with
    /// the gate off; the form the in-crate tests build handles in.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: ActorId,
        tx: MailTx<ActorEnvelope<A>>,
        stop_lane: StopLane,
        mailbox_capacity: usize,
        status_rx: StatusRx,
        priority_tx: Option<mpsc::Sender<ActorEnvelope<A>>>,
        priority_full_warned: Option<Arc<AtomicBool>>,
    ) -> Self {
        let flags = priority_full_warned.map(|warned| {
            Arc::new(LaneFlags {
                warned: AtomicBool::new(warned.load(std::sync::atomic::Ordering::Relaxed)),
                by_alias: false,
            })
        });
        Self::with_lane_flags(
            id,
            tx,
            stop_lane,
            mailbox_capacity,
            status_rx,
            priority_tx,
            flags,
        )
    }

    /// As [`new`](Self::new), with the lane's shared flags made by the
    /// caller.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_lane_flags(
        id: ActorId,
        tx: MailTx<ActorEnvelope<A>>,
        stop_lane: StopLane,
        mailbox_capacity: usize,
        status_rx: StatusRx,
        priority_tx: Option<mpsc::Sender<ActorEnvelope<A>>>,
        lane_flags: Option<Arc<LaneFlags>>,
    ) -> Self {
        Self {
            id,
            tx,
            priority_tx,
            stop_lane,
            mailbox_capacity,
            status_rx,
            lane_flags,
        }
    }

    /// The priority lane a plain handle sends on: the lane's sender, unless
    /// the actor has none or leaves it to aliases.
    #[inline]
    fn open_lane(&self) -> Option<&mpsc::Sender<ActorEnvelope<A>>> {
        let lane = self.priority_tx.as_ref()?;
        match &self.lane_flags {
            Some(flags) if flags.by_alias => None,
            _ => Some(lane),
        }
    }

    /// The "lane full" warning flag, when the actor has a lane.
    fn lane_warned(&self) -> Option<&AtomicBool> {
        self.lane_flags.as_deref().map(|flags| &flags.warned)
    }

    /// The `sys` calls of this actor: statistics, log, trace, installed
    /// functions, suspension and, when the actor allows it, `inspect` and
    /// `replace_state`.
    pub fn sys(&self) -> Sys<'_, A> {
        Sys::new(self)
    }

    /// The incarnation's abort record, which carries the bits of the `sys`
    /// bundle and the system channel the calls ride.
    #[allow(dead_code)]
    pub(crate) fn sys_record(&self) -> &AbortRecord {
        self.stop_lane.abort_record()
    }

    /// The identity of the system this actor was spawned in: the address of
    /// the system's telemetry state, which the record keeps from being reused,
    /// so it is unique to one system for the life of the process; `0` for a
    /// handle no spawn made (only a unit test builds one).
    /// Two handles are equal when they have the same id and the same
    /// system.
    pub(crate) fn system_generation(&self) -> u64 {
        self.stop_lane.abort_record().system_identity()
    }

    /// The system this actor was spawned in, if it is still alive: `None`
    /// once every holder of it has let go, and for a handle no spawn made.
    /// Resolved by identity, never by name, so a later system of the same
    /// name is never taken for it.
    pub(crate) fn system(&self) -> Option<Arc<ActorSystem>> {
        self.stop_lane.abort_record().system()
    }

    /// Returns a clone of this actor's stop lane sender (for wiring the
    /// supervision escalation ladder and the system registry's shutdown
    /// sweep without a mailbox round-trip).
    pub(crate) fn stop_lane(&self) -> StopLane {
        self.stop_lane.clone()
    }

    /// This actor's stop lane, borrowed: for the caller that only reads or
    /// narrows it and needs no clone of the lane, let alone of the handle.
    pub(crate) fn stop_lane_ref(&self) -> &StopLane {
        &self.stop_lane
    }

    /// `Err(AskError::CallingSelf)` when the calling task is the task that
    /// runs this incarnation: a request it makes could never be answered. The
    /// check reads one thread-local, the key of the incarnation the thread is
    /// polling, and only a caller on that thread goes on to compare the task
    /// ids. A `spawn_blocking` thread, a task spawned from a callback, a task
    /// polled inside the actor's poll under an id of its own, another
    /// incarnation and an ended incarnation never match.
    #[inline]
    fn refuse_self_call(&self) -> Result<(), AskError> {
        if self.stop_lane.abort_record().is_self_call() {
            return Err(AskError::CallingSelf);
        }
        Ok(())
    }

    /// The identity fields of this actor's log statements.
    #[cfg(feature = "tracing")]
    pub(crate) fn log_fields(&self) -> crate::telemetry::LogFields<'static> {
        crate::telemetry::log_fields(self.stop_lane.abort_record(), None)
    }

    /// Returns a clone of the runtime status receiver (used by the system
    /// registry's shutdown cascade - see the module docs on the two
    /// observability planes).
    pub(crate) fn status_rx(&self) -> StatusRx {
        self.status_rx.clone()
    }

    /// Returns the unique identifier of the actor.
    pub fn id(&self) -> &ActorId {
        &self.id
    }

    /// Returns the [`Incarnation`] this handle reaches: the single run of the
    /// actor whose channels it holds.
    ///
    /// The [`ActorId`] stays the same across a supervised restart and the
    /// incarnation does not, so two handles to one actor's successive runs
    /// compare equal and report different incarnations.
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
    /// let handle = Idle.spawn().await?;
    /// assert_eq!(handle.get_status().await?.incarnation, handle.incarnation());
    /// # Ok(())
    /// # }
    /// ```
    pub fn incarnation(&self) -> Incarnation {
        self.status_rx.record().incarnation()
    }

    /// This incarnation as the owner of the drops its log lines name: its
    /// id, with the sensitivity its record carries.
    pub(crate) fn owner(&self) -> crate::actor::runtime::Owner {
        crate::actor::runtime::Owner::of(&self.id, self.stop_lane.abort_record())
    }

    /// The wait lists the actor's mailbox was made with.
    #[cfg(test)]
    pub(crate) fn mailbox_lanes(&self) -> usize {
        self.tx.lane_count()
    }

    /// Returns the total capacity of the actor's mailbox.
    pub fn mailbox_capacity(&self) -> usize {
        self.mailbox_capacity
    }

    /// Returns the number of mailbox slots in use: the messages queued plus
    /// the slots senders have reserved and not yet filled, so a slot handed to
    /// a waiting sender counts as in use from that moment.
    pub fn mailbox_len(&self) -> usize {
        self.mailbox_capacity - self.tx.capacity()
    }

    /// The senders parked on a full mailbox now.
    #[cfg(test)]
    pub(crate) fn mailbox_waiting(&self) -> usize {
        self.tx.waiting()
    }

    /// Returns the number of available slots in the mailbox.
    pub fn mailbox_available(&self) -> usize {
        self.tx.capacity()
    }

    /// Returns the priority lane's total capacity, or `0` if this actor has
    /// no priority lane (see
    /// [`MailboxConfig::with_priority_lane`](crate::actor::runtime::MailboxConfig::with_priority_lane)).
    ///
    /// This and [`priority_len`](Self::priority_len) are the two
    /// observability points [`MailboxConfig::with_priority_lane`](crate::actor::runtime::MailboxConfig::with_priority_lane)'s
    /// documented divergence from Erlang refers to: a plain Erlang process
    /// exposes only its total queue length, never a per-lane breakdown.
    pub fn priority_capacity(&self) -> usize {
        self.priority_tx.as_ref().map_or(0, |tx| tx.max_capacity())
    }

    /// Returns the number of priority-lane slots in use: the messages queued
    /// in the lane plus the slots senders have reserved and not yet filled, or
    /// `0` if this actor has no priority lane. Unaffected by traffic on the
    /// normal mailbox, and vice versa: see
    /// [`mailbox_len`](Self::mailbox_len) for that counter.
    pub fn priority_len(&self) -> usize {
        self.priority_tx
            .as_ref()
            .map_or(0, |tx| tx.max_capacity() - tx.capacity())
    }

    /// Returns `true` while the actor's mailbox is open.
    ///
    /// `false` with the actor's name still registered means the actor is
    /// stopping: it accepts nothing from here on (a send returns `Closed`, or
    /// `ResponseDropped` for a send that raced the close; the message is not
    /// processed either way), and the name is free only once its teardown has
    /// ended. A caller that must reach the actor by name waits with
    /// [`wait_stopped`](Self::wait_stopped) on the handle it holds, then looks
    /// the name up again; an actor a supervisor restarts is registered again
    /// as the next incarnation. A lookup that finds nothing is final for an
    /// actor that nothing restarts; for a supervised actor it means either
    /// that the next incarnation is not registered yet or that its supervisor
    /// will not restart it, so bound the retries.
    ///
    /// `true` does not promise that the actor will handle a message: while its
    /// `on_stopped` or a supervisor's final deliveries run it still accepts
    /// messages and drops them unhandled (a `send` then reads
    /// `ResponseDropped`).
    ///
    /// Once the actor has left its message loop for good (its `on_stopped` hook
    /// has returned, or a `Kill` ended the loop, which skips that hook), this
    /// reports `false` immediately, throughout the teardown of its children,
    /// not only once its task has fully exited. Right after
    /// `stop(StopReason::Kill)` returns it can still read `true`, until the
    /// actor reaches its next turn boundary; await `kill_by_id`,
    /// `stop_within(StopReason::Kill, d)` or `wait_stopped()` when the end
    /// matters.
    ///
    /// This differs from Erlang/OTP, where `is_process_alive/1` stays true
    /// until `terminate/2` returns and reads false right after
    /// `exit(Pid, kill)`: here `false` means that the actor has left its
    /// message loop for good and sends are refused, so it turns false before
    /// the end of the children's teardown, and `status()` keeps its own
    /// reading until the actor reaches `Stopped`.
    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed()
    }

    /// Returns the actor's current lifecycle status.
    ///
    /// This is the runtime observability plane (see the module docs):
    /// it reads a per-actor cell maintained directly by the runtime, so it
    /// answers without waiting for the actor, even while the actor is hung
    /// inside a callback and not servicing its mailbox or system channel.
    /// Contrast with
    /// [`get_status`](Self::get_status), the queue plane, which travels
    /// through the system channel and can hang on a stuck actor.
    ///
    /// The cell keeps only the latest value: a short-lived intermediate status
    /// (for example [`Stopping`](ActorStatus::Stopping)) can be missed if it
    /// changes again before this is read. [`Stopped`](ActorStatus::Stopped)
    /// is terminal, so it is never missed this way; see
    /// [`wait_stopped`](Self::wait_stopped).
    ///
    /// An actor whose task was aborted never writes a final status itself;
    /// its status sender closing is the terminal signal instead, and this
    /// reports [`Stopped`](ActorStatus::Stopped) for that case too.
    pub fn status(&self) -> ActorStatus {
        read_status(&self.status_rx)
    }

    /// Sends a message to the actor without waiting for a response (fire-and-forget).
    ///
    /// This method awaits if the mailbox is full.
    ///
    /// Once the actor has left its message loop for good (its `on_stopped` hook
    /// has returned, or a `Kill` ended the loop, which skips that hook), sends
    /// are refused throughout the teardown of its children, not only once its
    /// task has fully exited. A message accepted while the actor runs a
    /// supervisor's final deliveries or `on_stopped` is dropped unhandled.
    ///
    /// Two actors that each `notify` the other from `handle` while both
    /// mailboxes are full wait on each other for good, because a handler runs
    /// to its end before its actor receives again. A caller outside the cycle
    /// leaves it with [`try_notify`](Self::try_notify), which answers `Full`,
    /// or with [`send_timeout`](Self::send_timeout), which answers
    /// `Timeout { enqueued: false }`. A message whose `notify` has returned is
    /// received before any message that another actor notifies after that
    /// return; a `notify` still waiting for room promises no order against
    /// other waiting senders.
    ///
    /// A call made on the actor's own task (from a callback, or from a future
    /// the callback awaits in place) to its own full mailbox answers
    /// [`SendError::Full`] at once, since that task is the one that makes
    /// room; with room it sends as a call from any other task does. A task
    /// or thread the callback starts and waits for is not the actor's own
    /// task: its send to the full mailbox waits for room as any other does.
    ///
    /// # Errors
    /// - `SendError::Closed` if the actor has stopped.
    /// - `SendError::Full` if the call is made on the actor's own task and its
    ///   mailbox is full.
    pub async fn notify(&self, msg: A::Message) -> Result<(), SendError> {
        let record = self.stop_lane.abort_record();
        let mut reserve = pin!(self.tx.reserve());
        let wait = std::future::poll_fn(|cx| poll_cast_unit(reserve.as_mut(), cx, record));
        let permit = if wait_wanted(record) {
            Edged::new(pin!(wait), record).await
        } else {
            wait.await
        }?;
        permit.send(Envelope::Message {
            payload: msg,
            responder: None,
        });
        Ok(())
    }

    /// Sends a message on the priority lane without waiting for a response
    /// (fire-and-forget), awaiting if the lane is full.
    ///
    /// Erlang/OTP's own guidance applies: "You very seldom need to resort to
    /// usage of priority messages. Receiving processes have not been
    /// optimized for handling large amounts of priority messages." (Erlang
    /// Reference Manual, Processes)
    ///
    /// # Ordering
    /// Messages keep their send order within each lane. A priority message is
    /// received before every normal message still queued when the actor next
    /// picks a message, including normal messages this same sender sent
    /// earlier. No message, priority or not, interrupts a callback already in
    /// progress.
    ///
    /// On an actor with no priority lane this behaves exactly like
    /// [`notify`](Self::notify).
    ///
    /// # Divergences from Erlang
    /// The lane's length and capacity are observable here ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure by
    /// awaiting here where Erlang's single queue is unbounded. Any holder of
    /// this actor's handle can send on the lane unless the actor leaves it to
    /// its priority aliases
    /// ([`with_priority_by_alias`](crate::actor::runtime::ActorConfig::with_priority_by_alias)),
    /// where Erlang/OTP lets a sender use priority only through an alias the
    /// receiver created for it, so the receiver chooses who may.
    ///
    /// A call made on the actor's own task to its own full lane answers
    /// [`SendError::Full`] at once, as [`notify`](Self::notify) does for a
    /// full mailbox.
    ///
    /// # Errors
    /// - `SendError::Closed` if the actor has stopped.
    /// - `SendError::Full` if the call is made on the actor's own task and the
    ///   queue it uses is full.
    pub async fn notify_priority(&self, msg: A::Message) -> Result<(), SendError> {
        let Some(tx) = self.open_lane() else {
            return self.notify(msg).await;
        };
        let permit = self.reserve_lane_cast(tx).await?;
        permit.into_inner().send(Envelope::Message {
            payload: msg,
            responder: None,
        });
        Ok(())
    }

    /// Attempts to send a message without blocking.
    ///
    /// `Ok` means the mailbox accepted the message, not that it will be
    /// handled: one accepted while the actor runs its final deliveries or
    /// `on_stopped`, or as its end closes the queue after the check, is
    /// dropped unhandled. With an error the message is dropped; keep a copy
    /// to retry.
    ///
    /// # Errors
    /// - `TrySendError::Full` if the mailbox is full.
    /// - `TrySendError::Closed` if the actor has stopped.
    pub fn try_notify(&self, msg: A::Message) -> Result<(), TrySendError> {
        self.tx
            .try_send(Envelope::Message {
                payload: msg,
                responder: None,
            })
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => TrySendError::Full,
                mpsc::error::TrySendError::Closed(_) => TrySendError::Closed,
            })
    }

    /// Reserves a slot in the mailbox for a message one of the actor's timers
    /// or streams sends, waiting while the mailbox is full; `None` once the
    /// mailbox is closed, and the message comes back to its sender, which
    /// releases it itself and reports that drop as made after the close.
    /// Nothing awaits between the reserve and the permit's send or drop, so a
    /// forwarder can decide at that instant whether its message is sent.
    pub(crate) async fn reserve_forward(&self) -> Option<ForwardPermit<'_, A>> {
        self.tx.reserve().await.ok().map(ForwardPermit::Normal)
    }

    /// As [`reserve_forward`](Self::reserve_forward) for a monitor's notice:
    /// on the priority lane when `priority` is set and the actor has one,
    /// otherwise on the normal mailbox.
    pub(crate) async fn reserve_signal(&self, priority: bool) -> Option<ForwardPermit<'_, A>> {
        match (&self.priority_tx, priority) {
            (Some(lane), true) => {
                reserve_priority(lane, &self.id, None, self.stop_lane.abort_record())
                    .await
                    .map(|permit| ForwardPermit::Priority(permit, lane))
            }
            _ => self.reserve_forward().await,
        }
    }

    /// A slot of the priority lane for a cast: as the lane's wait, except that
    /// a call made on the actor's own task to its own full lane is refused
    /// with `Full` at the point where it would wait, since only that task
    /// makes room.
    async fn reserve_lane_cast<'a>(
        &'a self,
        lane: &'a mpsc::Sender<ActorEnvelope<A>>,
    ) -> Result<WakeHold<mpsc::Permit<'a, ActorEnvelope<A>>>, SendError> {
        let record = self.stop_lane.abort_record();
        match try_reserve_held("priority lane", Some(&self.id), lane) {
            Ok(permit) => {
                lane_room(self.lane_warned());
                return Ok(permit);
            }
            Err(mpsc::error::TrySendError::Closed(())) => return Err(SendError::Closed),
            Err(mpsc::error::TrySendError::Full(())) if record.is_self_call() => {
                return Err(SendError::Full);
            }
            Err(mpsc::error::TrySendError::Full(())) => {}
        }
        let wait = reserve_priority(lane, &self.id, self.lane_warned(), record);
        if wait_wanted(record) {
            Edged::new(pin!(wait), record).await
        } else {
            wait.await
        }
        .ok_or(SendError::Closed)
    }

    /// A slot for a cast through an alias: on the priority lane when
    /// `priority` is set and the actor has one, otherwise on the mailbox. A
    /// call made on the actor's own task that would wait for its own full
    /// queue is refused with `Full`; the slot of a forwarder is taken with
    /// [`reserve_signal`](Self::reserve_signal), which always waits.
    pub(crate) async fn reserve_cast(
        &self,
        priority: bool,
    ) -> Result<ForwardPermit<'_, A>, SendError> {
        match (&self.priority_tx, priority) {
            (Some(lane), true) => self
                .reserve_lane_cast(lane)
                .await
                .map(|permit| ForwardPermit::Priority(permit, lane)),
            _ => {
                let record = self.stop_lane.abort_record();
                let mut reserve = pin!(self.tx.reserve());
                std::future::poll_fn(|cx| poll_cast_unit(reserve.as_mut(), cx, record))
                    .await
                    .map(ForwardPermit::Normal)
            }
        }
    }

    /// As [`reserve_signal`](Self::reserve_signal) without waiting: a full or
    /// closed queue is refused. Used by aliases, which choose the lane
    /// themselves and so ignore the gate that leaves it to them.
    pub(crate) fn try_reserve_signal(
        &self,
        priority: bool,
    ) -> Result<ForwardPermit<'_, A>, TrySendError> {
        match (&self.priority_tx, priority) {
            (Some(lane), true) => try_reserve_held("priority lane", Some(&self.id), lane)
                .map(|permit| ForwardPermit::Priority(permit, lane))
                .map_err(|err| match err {
                    mpsc::error::TrySendError::Full(()) => TrySendError::Full,
                    mpsc::error::TrySendError::Closed(()) => TrySendError::Closed,
                }),
            _ => self.tx.try_reserve().map(ForwardPermit::Normal),
        }
    }

    /// [`reserve_forward`](Self::reserve_forward) without waiting: a full or
    /// closed mailbox gives the message back.
    pub(crate) fn try_forward(
        &self,
        msg: A::Message,
    ) -> Result<(), mpsc::error::TrySendError<A::Message>> {
        self.tx
            .try_send(Envelope::Message {
                payload: msg,
                responder: None,
            })
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(envelope) => {
                    mpsc::error::TrySendError::Full(own_message::<A>(envelope))
                }
                mpsc::error::TrySendError::Closed(envelope) => {
                    mpsc::error::TrySendError::Closed(own_message::<A>(envelope))
                }
            })
    }

    /// True once the actor's mailbox is closed (see
    /// [`mailbox_close`](Self::mailbox_close)).
    pub(crate) fn mailbox_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Completes once the actor's mailbox is closed: by its receiver's
    /// `close`, once `on_stopped` has returned, the initialization failed,
    /// or a `Kill` ended the run, or by the receiver's drop.
    pub(crate) async fn mailbox_close(&self) {
        self.tx.closed().await;
    }

    /// Attempts to send a message on the priority lane without blocking.
    ///
    /// Erlang/OTP's own guidance applies: "You very seldom need to resort to
    /// usage of priority messages. Receiving processes have not been
    /// optimized for handling large amounts of priority messages." (Erlang
    /// Reference Manual, Processes)
    ///
    /// # Ordering
    /// Messages keep their send order within each lane. A priority message is
    /// received before every normal message still queued when the actor next
    /// picks a message, including normal messages this same sender sent
    /// earlier. No message, priority or not, interrupts a callback already in
    /// progress.
    ///
    /// On an actor with no priority lane this behaves exactly like
    /// [`try_notify`](Self::try_notify).
    ///
    /// # Divergences from Erlang
    /// The lane's length and capacity are observable here ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure
    /// (`Full`, below) where Erlang's single queue is unbounded. Any holder of
    /// this actor's handle can send on the lane unless the actor leaves it to
    /// its priority aliases
    /// ([`with_priority_by_alias`](crate::actor::runtime::ActorConfig::with_priority_by_alias)),
    /// where Erlang/OTP lets a sender use priority only through an alias the
    /// receiver created for it, so the receiver chooses who may.
    ///
    /// `Ok` means the lane accepted the message, not that it will be
    /// handled: one accepted while the actor runs its final deliveries or
    /// `on_stopped` is dropped unhandled. With an error the message is
    /// dropped; keep a copy to retry.
    ///
    /// # Errors
    /// - `TrySendError::Full` if the priority lane is full.
    /// - `TrySendError::Closed` if the actor has stopped.
    pub fn try_notify_priority(&self, msg: A::Message) -> Result<(), TrySendError> {
        let Some(tx) = self.open_lane() else {
            return self.try_notify(msg);
        };
        tx.try_send(Envelope::Message {
            payload: msg,
            responder: None,
        })
        .map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => TrySendError::Full,
            mpsc::error::TrySendError::Closed(_) => TrySendError::Closed,
        })
    }

    /// Sends a message and waits for a response (request-response).
    ///
    /// To answer later, as a deferred reply does in Erlang/OTP, put a
    /// `tokio::sync::oneshot::Sender` in the message and answer it from a later
    /// turn.
    ///
    /// # Calling oneself
    /// A call from the actor's own task to its own handle (from `handle`, a
    /// start or stop hook, `on_child_stopped` or a stop-time delivery) returns
    /// `AskError::CallingSelf` at once and sends nothing: the actor is a
    /// single task that is busy running the callback, so it could never reach
    /// its mailbox to answer its own request, as in Erlang/OTP, where
    /// `gen_server:call` to oneself fails with `calling_self`. The check
    /// refuses only the actor's own task. A task spawned from a callback, a
    /// `spawn_blocking` thread, another actor and another or an ended
    /// incarnation are not refused, so a callback that awaits such a task
    /// which then calls this actor still parks forever, and so do two actors
    /// that `send` each other from their callbacks: [`send_timeout`](Self::send_timeout)
    /// is the way out of that cycle. `notify` and the other casts wait for
    /// no answer, so they are not refused as a call to oneself; one made on
    /// the actor's own task that would have to wait for room in its own full
    /// queue answers [`SendError::Full`] at once
    /// ([`AliasError::Full`](crate::AliasError::Full) through an alias).
    ///
    /// # Errors
    /// - `AskError::CallingSelf` if the call is made from the actor's own
    ///   task (nothing was sent).
    /// - `AskError::Closed` if the actor was already stopped (the message was
    ///   NOT processed).
    /// - `AskError::ResponseDropped { reason }` if the actor stopped after
    ///   accepting the message but before replying (unknown whether it was
    ///   processed); `reason` is why the actor stopped. The error is
    ///   returned once that reason is final: the actor makes it final when it
    ///   seals its stop lane, after its children's teardown and its
    ///   `on_stopped`, and before its queued messages, its value and its
    ///   context are dropped. That wait has no bound of its own: when the
    ///   teardown of the target may not return, bound the call with
    ///   `tokio::time::timeout`.
    /// - `AskError::Actor(err)` if the handler returned an error, or panicked
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor.
    ///   The caller reads it the way an Erlang/OTP `gen_server:call` caller
    ///   reads a crash, after the process is dead: the error is returned when
    ///   the actor seals its stop lane, after its `on_stopped` and its
    ///   children's teardown, with its name already free and before its queued
    ///   messages, its value and its context are dropped. A lookup by name
    ///   right after it finds no registration, or the next incarnation if a
    ///   supervisor restarts the actor, never the one that crashed: retry with
    ///   a fresh [`ActorSystem::get`](crate::system::ActorSystem::get); `None`
    ///   is final for an actor that nothing restarts, and for a supervised one
    ///   it means either that the next incarnation is not registered yet or
    ///   that its supervisor will not restart it, so bound the retries. The
    ///   wait for the seal has no bound of its own, like the one for
    ///   `ResponseDropped`: when the teardown of the target may not return,
    ///   bound the call with `tokio::time::timeout`. A child of the target
    ///   whose handler awaits this call cannot take its parent's stop until
    ///   the call returns: the target's shutdown window for it runs out and it
    ///   is killed instead of stopped in order (under `Shutdown::Infinity` the
    ///   window never runs out). A `Kill`, an abort or the end of the actor's
    ///   runtime that ends the actor first makes the call return
    ///   `ResponseDropped` with the final reason instead.
    pub async fn send(&self, msg: A::Message) -> Result<A::Response, AskError> {
        self.refuse_self_call()?;
        let (tx, mut rx) = oneshot::channel();
        let record = self.stop_lane.abort_record();
        let edged = wait_wanted(record);
        let permit = if edged {
            Edged::new(pin!(self.tx.reserve()), record).await
        } else {
            self.tx.reserve().await
        }
        .map_err(|_| AskError::Closed)?;
        #[cfg(test)]
        late_slot_tests::between_reserve_and_push();
        permit.send(Envelope::Message {
            payload: msg,
            responder: Some(tx),
        });
        if queue_closed_after_push(|| self.tx.is_closed()) {
            return self.answer_after_close(rx).await;
        }

        // Decided again here: a send first polled outside a hook, or with
        // detection off, can begin its reply wait inside a hook with it on.
        let reply = if wait_wanted(record) {
            Edged::new(Pin::new(&mut rx), record).await
        } else {
            rx.await
        };
        match reply {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(err)) => Err(AskError::Actor(err)),
            Err(_) => Err(self.response_dropped().await),
        }
    }

    /// The answer to a request that found its queue closed after its push:
    /// the actor ended while the request was on its way. A reply the actor
    /// sent before it closed the queue is read here; otherwise the request
    /// was dropped unhandled or is left in the queue unseen, and the caller
    /// reads the actor's final reason from the status plane instead of
    /// waiting on a reply nobody will send. The actor sends every reply
    /// before it closes the queue, except the answer to a request whose
    /// handler failed. That one is sent at the lane's seal, before the final
    /// reason is written, so it is read once more after that reason: no
    /// answer is missed.
    async fn answer_after_close(
        &self,
        mut rx: oneshot::Receiver<Result<A::Response, ActorError>>,
    ) -> Result<A::Response, AskError> {
        if let Some(answer) = settled(rx.try_recv()) {
            return answer;
        }
        let dropped = self.response_dropped().await;
        settled(rx.try_recv()).unwrap_or(Err(dropped))
    }

    /// The error for a request whose reply channel closed: the actor ended
    /// before it replied. Reads the reason the actor ended with from the
    /// runtime status plane, waiting until the reason is final (written at
    /// the actor's lane seal) or the plane closes.
    async fn response_dropped(&self) -> AskError {
        let mut rx = self.status_rx.clone();
        AskError::ResponseDropped {
            reason: wait_final_reason(&mut rx).await,
        }
    }

    /// Sends a message on the priority lane and waits for a response
    /// (request-response), awaiting if the lane is full.
    ///
    /// Erlang/OTP's own guidance applies: "You very seldom need to resort to
    /// usage of priority messages. Receiving processes have not been
    /// optimized for handling large amounts of priority messages." (Erlang
    /// Reference Manual, Processes)
    ///
    /// # Ordering
    /// Messages keep their send order within each lane. A priority message is
    /// received before every normal message still queued when the actor next
    /// picks a message, including normal messages this same sender sent
    /// earlier. No message, priority or not, interrupts a callback already in
    /// progress.
    ///
    /// On an actor with no priority lane this behaves exactly like
    /// [`send`](Self::send).
    ///
    /// # Divergences from Erlang
    /// The lane's length and capacity are observable here ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure by
    /// awaiting here where Erlang's single queue is unbounded. Any holder of
    /// this actor's handle can send on the lane unless the actor leaves it to
    /// its priority aliases
    /// ([`with_priority_by_alias`](crate::actor::runtime::ActorConfig::with_priority_by_alias)),
    /// where Erlang/OTP lets a sender use priority only through an alias the
    /// receiver created for it, so the receiver chooses who may.
    ///
    /// # Calling oneself
    /// As for [`send`](Self::send): a call from the actor's own task returns
    /// `AskError::CallingSelf` at once, and a task spawned from a callback
    /// that calls this actor while the callback awaits it still parks forever.
    ///
    /// # Errors
    /// - `AskError::CallingSelf` if the call is made from the actor's own
    ///   task (nothing was sent).
    /// - `AskError::Closed` if the actor was already stopped (the message was
    ///   NOT processed).
    /// - `AskError::ResponseDropped { reason }` if the actor stopped after
    ///   accepting the message but before replying (unknown whether it was
    ///   processed); `reason` is why the actor stopped. The error is
    ///   returned once that reason is final: the actor makes it final when it
    ///   seals its stop lane, after its children's teardown and its
    ///   `on_stopped`, and before its queued messages, its value and its
    ///   context are dropped. That wait has no bound of its own: when the
    ///   teardown of the target may not return, bound the call with
    ///   `tokio::time::timeout`.
    /// - `AskError::Actor(err)` if the handler returned an error, or panicked
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor.
    ///   The caller reads it the way an Erlang/OTP `gen_server:call` caller
    ///   reads a crash, after the process is dead: the error is returned when
    ///   the actor seals its stop lane, after its `on_stopped` and its
    ///   children's teardown, with its name already free and before its queued
    ///   messages, its value and its context are dropped. A lookup by name
    ///   right after it finds no registration, or the next incarnation if a
    ///   supervisor restarts the actor, never the one that crashed: retry with
    ///   a fresh [`ActorSystem::get`](crate::system::ActorSystem::get); `None`
    ///   is final for an actor that nothing restarts, and for a supervised one
    ///   it means either that the next incarnation is not registered yet or
    ///   that its supervisor will not restart it, so bound the retries. The
    ///   wait for the seal has no bound of its own, like the one for
    ///   `ResponseDropped`: when the teardown of the target may not return,
    ///   bound the call with `tokio::time::timeout`. A child of the target
    ///   whose handler awaits this call cannot take its parent's stop until
    ///   the call returns: the target's shutdown window for it runs out and it
    ///   is killed instead of stopped in order (under `Shutdown::Infinity` the
    ///   window never runs out). A `Kill`, an abort or the end of the actor's
    ///   runtime that ends the actor first makes the call return
    ///   `ResponseDropped` with the final reason instead.
    pub async fn send_priority(&self, msg: A::Message) -> Result<A::Response, AskError> {
        let Some(tx) = self.open_lane() else {
            return self.send(msg).await;
        };
        self.refuse_self_call()?;
        let (resp_tx, mut resp_rx) = oneshot::channel();
        let record = self.stop_lane.abort_record();
        let edged = wait_wanted(record);

        let permit = if edged {
            Edged::new(
                pin!(reserve_priority(tx, &self.id, self.lane_warned(), record)),
                record,
            )
            .await
        } else {
            reserve_priority(tx, &self.id, self.lane_warned(), record).await
        }
        .ok_or(AskError::Closed)?
        .into_inner();
        #[cfg(test)]
        late_slot_tests::between_reserve_and_push();
        permit.send(Envelope::Message {
            payload: msg,
            responder: Some(resp_tx),
        });
        if queue_closed_after_push(|| tx.is_closed()) {
            return self.answer_after_close(resp_rx).await;
        }

        // Decided again here: a send first polled outside a hook, or with
        // detection off, can begin its reply wait inside a hook with it on.
        let reply = if wait_wanted(record) {
            Edged::new(Pin::new(&mut resp_rx), record).await
        } else {
            resp_rx.await
        };
        match reply {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(err)) => Err(AskError::Actor(err)),
            Err(_) => Err(self.response_dropped().await),
        }
    }

    /// Sends a message and waits for a response, bounded by a single deadline
    /// that spans both enqueueing the message and awaiting the reply. A timeout
    /// beyond 30 years is treated as 30 years.
    ///
    /// Mirrors `gen_server:call/3`: one clock for the whole request. The
    /// deadline is armed BEFORE the mailbox send, so a request parked on a
    /// full mailbox still times out instead of waiting indefinitely.
    ///
    /// # Calling oneself
    /// A call from the actor's own task returns `AskError::CallingSelf` at
    /// once, as [`send`](Self::send) does. A cycle of two actors that call each
    /// other from their callbacks is not detected: the deadline of this call is
    /// the way out of it.
    ///
    /// # Errors
    /// - `AskError::CallingSelf` if the call is made from the actor's own
    ///   task (nothing was sent).
    /// - `AskError::Closed` if the actor was already stopped (the message was
    ///   NOT processed).
    /// - `AskError::Timeout { enqueued: false }` if the deadline elapsed
    ///   before the message could be enqueued. The message was NOT sent -
    ///   unconditionally safe to retry.
    /// - `AskError::Timeout { enqueued: true }` if the message was enqueued
    ///   but the deadline elapsed before a reply arrived. The handler is NOT
    ///   cancelled - it keeps running to completion, and a late reply lands
    ///   in a dropped `oneshot` and is discarded. Retry only if the operation
    ///   is idempotent.
    /// - `AskError::ResponseDropped { reason }` if the actor stopped after
    ///   accepting the message but before replying (unknown whether it was
    ///   processed); `reason` is why the actor stopped. The error is
    ///   returned once that reason is final: the actor makes it final when it
    ///   seals its stop lane, after its children's teardown and its
    ///   `on_stopped`, and before its queued messages, its value and its
    ///   context are dropped. That wait counts against this call's own
    ///   deadline.
    /// - `AskError::Actor(err)` if the handler returned an error, or panicked
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor.
    ///   The caller reads it the way an Erlang/OTP `gen_server:call` caller
    ///   reads a crash, after the process is dead: the error is returned when
    ///   the actor seals its stop lane, after its `on_stopped` and its
    ///   children's teardown, with its name already free and before its queued
    ///   messages, its value and its context are dropped. A lookup by name
    ///   right after it finds no registration, or the next incarnation if a
    ///   supervisor restarts the actor, never the one that crashed: retry with
    ///   a fresh [`ActorSystem::get`](crate::system::ActorSystem::get); `None`
    ///   is final for an actor that nothing restarts, and for a supervised one
    ///   it means either that the next incarnation is not registered yet or
    ///   that its supervisor will not restart it, so bound the retries. The
    ///   wait for the seal counts against this call's own deadline: a deadline
    ///   that passes first reads `Timeout { enqueued: true }`. A child of the
    ///   target whose handler awaits this call takes its parent's stop only
    ///   once the call returns, at the latest at its deadline. A `Kill`, an
    ///   abort or the end of the actor's runtime that ends the actor first
    ///   makes the call return `ResponseDropped` with the final reason
    ///   instead.
    pub async fn send_timeout(
        &self,
        msg: A::Message,
        timeout: Duration,
    ) -> Result<A::Response, AskError> {
        self.refuse_self_call()?;
        let deadline = saturating_deadline(Instant::now(), timeout);
        let (tx, rx) = oneshot::channel();

        let permit = match time::timeout_at(deadline, self.tx.reserve()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(AskError::Closed),
            Err(_) => return Err(AskError::Timeout { enqueued: false }),
        };
        #[cfg(test)]
        late_slot_tests::between_reserve_and_push();
        permit.send(Envelope::Message {
            payload: msg,
            responder: Some(tx),
        });
        if queue_closed_after_push(|| self.tx.is_closed()) {
            return time::timeout_at(deadline, self.answer_after_close(rx))
                .await
                .unwrap_or(Err(AskError::Timeout { enqueued: true }));
        }

        let reply = time::timeout(deadline.saturating_duration_since(Instant::now()), async {
            match rx.await {
                Ok(reply) => Ok(reply),
                Err(_) => Err(self.response_dropped().await),
            }
        })
        .await
        .map_err(|_| AskError::Timeout { enqueued: true })??;

        match reply {
            Ok(resp) => Ok(resp),
            Err(err) => Err(AskError::Actor(err)),
        }
    }

    /// Sends a message on the priority lane and waits for a response, bounded
    /// by a single deadline that spans both enqueueing the message and awaiting
    /// the reply. A timeout beyond 30 years is treated as 30 years.
    ///
    /// Erlang/OTP's own guidance applies: "You very seldom need to resort to
    /// usage of priority messages. Receiving processes have not been
    /// optimized for handling large amounts of priority messages." (Erlang
    /// Reference Manual, Processes)
    ///
    /// # Ordering
    /// Messages keep their send order within each lane. A priority message is
    /// received before every normal message still queued when the actor next
    /// picks a message, including normal messages this same sender sent
    /// earlier. No message, priority or not, interrupts a callback already in
    /// progress.
    ///
    /// On an actor with no priority lane this behaves exactly like
    /// [`send_timeout`](Self::send_timeout).
    ///
    /// # Divergences from Erlang
    /// The lane's length and capacity are observable here ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure (a
    /// timeout, below) where Erlang's single queue is unbounded. Any holder of
    /// this actor's handle can send on the lane unless the actor leaves it to
    /// its priority aliases
    /// ([`with_priority_by_alias`](crate::actor::runtime::ActorConfig::with_priority_by_alias)),
    /// where Erlang/OTP lets a sender use priority only through an alias the
    /// receiver created for it, so the receiver chooses who may.
    ///
    /// # Calling oneself
    /// A call from the actor's own task returns `AskError::CallingSelf` at
    /// once, as [`send`](Self::send) does.
    ///
    /// # Errors
    /// - `AskError::CallingSelf` if the call is made from the actor's own
    ///   task (nothing was sent).
    /// - `AskError::Closed` if the actor was already stopped (the message was
    ///   NOT processed).
    /// - `AskError::Timeout { enqueued: false }` if the deadline elapsed
    ///   before the message could be enqueued (for example, the priority
    ///   lane stayed full for the whole deadline). The message was NOT sent -
    ///   unconditionally safe to retry.
    /// - `AskError::Timeout { enqueued: true }` if the message was enqueued
    ///   but the deadline elapsed before a reply arrived. The handler is NOT
    ///   cancelled - it keeps running to completion, and a late reply lands
    ///   in a dropped `oneshot` and is discarded. Retry only if the operation
    ///   is idempotent.
    /// - `AskError::ResponseDropped { reason }` if the actor stopped after
    ///   accepting the message but before replying (unknown whether it was
    ///   processed); `reason` is why the actor stopped. The error is
    ///   returned once that reason is final: the actor makes it final when it
    ///   seals its stop lane, after its children's teardown and its
    ///   `on_stopped`, and before its queued messages, its value and its
    ///   context are dropped. That wait counts against this call's own
    ///   deadline.
    /// - `AskError::Actor(err)` if the handler returned an error, or panicked
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor.
    ///   The caller reads it the way an Erlang/OTP `gen_server:call` caller
    ///   reads a crash, after the process is dead: the error is returned when
    ///   the actor seals its stop lane, after its `on_stopped` and its
    ///   children's teardown, with its name already free and before its queued
    ///   messages, its value and its context are dropped. A lookup by name
    ///   right after it finds no registration, or the next incarnation if a
    ///   supervisor restarts the actor, never the one that crashed: retry with
    ///   a fresh [`ActorSystem::get`](crate::system::ActorSystem::get); `None`
    ///   is final for an actor that nothing restarts, and for a supervised one
    ///   it means either that the next incarnation is not registered yet or
    ///   that its supervisor will not restart it, so bound the retries. The
    ///   wait for the seal counts against this call's own deadline: a deadline
    ///   that passes first reads `Timeout { enqueued: true }`. A child of the
    ///   target whose handler awaits this call takes its parent's stop only
    ///   once the call returns, at the latest at its deadline. A `Kill`, an
    ///   abort or the end of the actor's runtime that ends the actor first
    ///   makes the call return `ResponseDropped` with the final reason
    ///   instead.
    pub async fn send_priority_timeout(
        &self,
        msg: A::Message,
        timeout: Duration,
    ) -> Result<A::Response, AskError> {
        let Some(tx) = self.open_lane() else {
            return self.send_timeout(msg, timeout).await;
        };
        self.refuse_self_call()?;
        let deadline = saturating_deadline(Instant::now(), timeout);
        let (resp_tx, resp_rx) = oneshot::channel();

        let permit = match time::timeout_at(
            deadline,
            reserve_priority(tx, &self.id, None, self.stop_lane.abort_record()),
        )
        .await
        {
            Ok(Some(permit)) => permit.into_inner(),
            Ok(None) => return Err(AskError::Closed),
            Err(_) => return Err(AskError::Timeout { enqueued: false }),
        };
        #[cfg(test)]
        late_slot_tests::between_reserve_and_push();
        permit.send(Envelope::Message {
            payload: msg,
            responder: Some(resp_tx),
        });
        if queue_closed_after_push(|| tx.is_closed()) {
            return time::timeout_at(deadline, self.answer_after_close(resp_rx))
                .await
                .unwrap_or(Err(AskError::Timeout { enqueued: true }));
        }

        let reply = time::timeout(deadline.saturating_duration_since(Instant::now()), async {
            match resp_rx.await {
                Ok(reply) => Ok(reply),
                Err(_) => Err(self.response_dropped().await),
            }
        })
        .await
        .map_err(|_| AskError::Timeout { enqueued: true })??;

        match reply {
            Ok(resp) => Ok(resp),
            Err(err) => Err(AskError::Actor(err)),
        }
    }

    /// Signals the actor to stop.
    ///
    /// A stop request is read at the next turn boundary, ahead of any message
    /// queued before it. Unless `pre_stop` vetoes it (only `Graceful`, `Shutdown`
    /// and `ParentRequest` can be vetoed), the actor ends without handling those
    /// messages, and a queued `send` returns `AskError::ResponseDropped`.
    /// Erlang/OTP receives `gen_server:stop` as a system message in arrival
    /// order, so earlier messages are handled first. To get that order, send a
    /// message whose handler calls `ctx.self_handle().stop(..)`.
    ///
    /// Delivered through the stop lane: a synchronous signal that never waits
    /// for capacity, which the run loop observes ahead of everything else - the
    /// system channel and the user mailbox alike - so it is never delayed by a
    /// full mailbox or a backlog of supervision traffic. Like every signal on
    /// the lane, it is only ever observed at a turn boundary: the signal itself
    /// never interrupts a callback already in progress; with
    /// [`StopReason::Kill`], the abort described below cuts a callback still
    /// awaiting once the actor's kill grace has passed.
    ///
    /// Escalation only ever increases: calling this with a lower-severity
    /// `reason` (for example `Graceful` after a `Kill` already landed) has no
    /// effect, and calling it again with the SAME severity - most commonly a
    /// retry after [`pre_stop`](crate::actor::Actor::pre_stop) vetoed the
    /// first request - always lands and, unless the
    /// [`pre_stop`](crate::actor::Actor::pre_stop) in flight accepts or
    /// panics, is guaranteed a fresh
    /// [`pre_stop`](crate::actor::Actor::pre_stop) invocation, even though the
    /// requested reason did not change tier. A request that lands while an
    /// accepting `pre_stop` runs is not asked again: the actor ends with the
    /// strongest request that landed. Within
    /// one severity the pending request keeps the higher rank: a parent's
    /// request is kept over a later shutdown or graceful one, a shutdown the
    /// actor asked for itself over a later graceful one, and a pending
    /// failure over a later cancellation. Calling
    /// this several times in a row before the actor gets back to its
    /// `select!` coalesces into a single observation of the highest severity
    /// requested: the actor sees at most one turn-boundary check per turn,
    /// never one per call.
    ///
    /// With [`StopReason::Kill`] the system also schedules the abort of the
    /// actor's task: if the actor does not reach a turn boundary within its
    /// kill grace (120 ms unless its spawn builder set another), its task is
    /// aborted. The abort is scheduled when the `Kill` is accepted while the
    /// actor is still registered, and never depends on this future. Once the
    /// actor has left the registry, its one remaining await is the end of
    /// its already aborted timer and stream tasks, which waits on their
    /// drops (user code, outside the end's guarantees, as the note on timers
    /// and streams says), so its end never precedes theirs and no abort is
    /// scheduled. A handler that raises `Kill` on itself
    /// through `ctx.self_handle().stop(StopReason::Kill)` and then awaits for
    /// longer than its kill grace is aborted at that await.
    ///
    /// For `Kill`, `Ok` means the request is accepted and the actor will end;
    /// the call does not wait for the end, and [`is_alive`](Self::is_alive)
    /// can still read `true` for a moment. [`wait_stopped`](Self::wait_stopped)
    /// waits for the end and reads `Kill`.
    ///
    /// # Errors
    /// Returns `SendError::Closed` if the actor does not accept the
    /// request any more: its task has ended (an aborted one included), it has
    /// sealed its stop lane, or it has left its message loop for good (its
    /// `on_stopped` has returned, a `Kill` ended the loop, which skips that
    /// hook, or its start failed) and the request is not [`StopReason::Kill`].
    /// A request other than `Kill` made while it runs its final deliveries or
    /// `on_stopped` is accepted and has no effect, and a `Kill` made then ends
    /// the deliveries and becomes the final reason. Through this handle a
    /// `Kill` is
    /// accepted until the actor seals its stop lane, just before its queued
    /// messages and its value are dropped; after that it answers
    /// `SendError::Closed`. [`ActorSystem::stop`](crate::system::ActorSystem::stop)
    /// and [`ActorSystem::kill`](crate::system::ActorSystem::kill) answer
    /// `NotFound` once the actor has left the registry.
    pub async fn stop(&self, reason: StopReason) -> Result<(), SendError> {
        if matches!(reason, StopReason::Kill) {
            // The deadline, the raise and the backstop are one call: the
            // system's, or none when it is gone.
            let system = self.system();
            let raised = raise_kill_declared(
                system.as_deref(),
                KillKey::Id(&self.id),
                &self.status_rx,
                &self.stop_lane,
                None,
                None,
                || {},
                None,
            );
            // The shared raise queued the request's event.
            if raised.result == RaiseResult::Refused {
                return Err(SendError::Closed);
            }
            return Ok(());
        }
        if crate::telemetry::engaged_for(self.stop_lane.abort_record(), None) {
            return self.stop_noted(reason);
        }
        if self.raise_request(reason) == RaiseResult::Refused {
            return Err(SendError::Closed);
        }
        Ok(())
    }

    /// The raise of a request that is not a `Kill`, the one site of
    /// [`stop`](Self::stop) and its telemetry tail.
    #[inline(always)]
    fn raise_request(&self, reason: StopReason) -> RaiseResult {
        self.stop_lane.try_raise(reason)
    }

    /// The tail of [`stop`](Self::stop) for a system that takes telemetry
    /// events or holds an identity: the request is raised as it is and the
    /// lane's answer is queued as an event.
    #[inline(never)]
    fn stop_noted(&self, reason: StopReason) -> Result<(), SendError> {
        let kept = reason.clone();
        let raised = self.raise_request(reason);
        emit_stop_requested(
            None,
            &self.stop_lane,
            &kept,
            raised,
            StopOrigin::Requested,
            None,
        );
        if raised == RaiseResult::Refused {
            return Err(SendError::Closed);
        }
        Ok(())
    }

    /// Stops this actor, bounded by `within`, and reports how far the
    /// escalation ladder had to go to get there.
    ///
    /// This stops THIS incarnation only. If the target is a supervised
    /// child, its supervisor may restart it per its own restart policy the
    /// moment it reaches a terminal status - `within` bounds this call's
    /// wait, not the child's overall lifetime. To stop a supervised child for
    /// good, use
    /// [`ActorContext::terminate_child`](crate::actor::context::ActorContext::terminate_child)
    /// from the parent instead.
    ///
    /// Raises `reason` on the stop lane, exactly like [`stop`](Self::stop),
    /// then waits up to `within` for a terminal status - including when
    /// `reason` is already [`StopReason::Kill`]: the call waits up to
    /// `within` for the actor to reach its next turn boundary and observe
    /// the kill before its task is aborted, since `Kill`, like every other
    /// reason, cannot interrupt a callback already in progress; for that
    /// call `within` replaces the actor's kill grace. If `reason` was not
    /// already `Kill` and the deadline passes first, the system escalates
    /// to `Kill` and waits the actor's kill grace; if the actor still has not
    /// reached a terminal status by then, its task is aborted directly and
    /// the system waits a short settle time before giving up. The window
    /// counts from the first poll of the call.
    ///
    /// A handler that never yields back to the run loop defeats every bound
    /// here - `Kill` is only ever observed at a turn boundary, and an abort
    /// only takes effect at an `.await` - and the call reports it as
    /// [`StopOutcome::Unresponsive`] when the runtime it is made on has a
    /// thread the handler does not hold. On a `current_thread` runtime that
    /// also runs the handler, the call makes no progress until the handler
    /// returns.
    ///
    /// Like Erlang/OTP's `gen_server:stop/3`, the wait is bounded; where that
    /// call exits the caller with `timeout` and does not kill the process, this
    /// one escalates when the window passes, first to `Kill` and then to an
    /// abort of the task, as a supervisor does with a child whose shutdown time
    /// ran out (its shutdown signal, then `kill`).
    ///
    /// # Dropping the future
    /// The first poll of the call commits the system to every step of the
    /// ladder, whether or not this future is still polled. For a reason other
    /// than `Kill`, while the actor is registered, a task of the system on the
    /// runtime that made the call takes the `Kill` step at `within` and the
    /// abort after the kill grace. For `Kill`, the raise itself arms the abort
    /// (or the attach does, when the abort handle is not stored yet), which the
    /// actor's own runtime takes after `within`. Dropping the future
    /// ends only the caller's wait. The stop
    /// request already raised stays raised (a `pre_stop` may still veto a
    /// `Graceful` one until the ladder's `Kill`). If the runtime that made the
    /// call ends before the task does, the system takes the same two steps from
    /// the actor's own runtime at the same instants, unless the actor's runtime
    /// has no time driver (then only a `Kill` whose window has already passed
    /// is taken, and it aborts the target at once, as every `Kill` on such a
    /// runtime does) or the actor's abort handle is not attached yet (then
    /// the `Kill` is taken at once). A pending call with a reason other than
    /// `Kill` holds that task, a small heap allocation, until the call
    /// returns or, once the call is dropped, until its ladder or its target
    /// ends; a `Kill` request holds
    /// none. A call that is kept alive and never polled again
    /// releases it once its window has passed and its ladder has ended. No
    /// public operation reaches the next case: a handle reaches the system it
    /// was spawned in through the stop lane it holds, and that system is held
    /// by the actor until the actor has sealed its stop lane, so a request
    /// that lane still takes always finds its system. For a target with no
    /// system, there is no system to own the steps and no registry to arm:
    /// the ladder runs in this future, raises its `Kill` and has no abort
    /// handle, so it aborts nothing; dropping the future ends it.
    ///
    /// OTP parity: like the shutdown that `supervisor:terminate_child` runs
    /// inside the supervisor, the ladder goes on when the caller gives up
    /// waiting. Where OTP waits for the `'DOWN'` after its kill without
    /// limit, the system waits a short settle time and the call may report
    /// [`StopOutcome::Unresponsive`].
    ///
    /// OTP parity: `proc_lib:stop/3`, which `gen_server:stop/3` uses, runs the
    /// stop in the caller and, when its timeout passes, exits the caller with
    /// `timeout` without killing the target. Here, for a reason other than
    /// `Kill`, the system takes the `Kill` at the window and the abort after
    /// the kill grace (for `Kill`, the abort after the window), whether or not
    /// the caller goes on waiting: a declared departure.
    ///
    /// For a reason other than `Kill`, the call returns when the stop task has
    /// answered or as soon as the actor reaches a terminal status, whichever
    /// comes first: a graceful stop completes without the task being polled. A
    /// `Kill` call has no stop task and returns when the actor reaches a
    /// terminal status, or reports `Unresponsive` after `within` and the settle
    /// time.
    ///
    /// A zero window acts at once: `Duration::ZERO` raises the `Kill` inside
    /// the call, right after the request, so an actor that has not run since
    /// the request skips `pre_stop` and `on_stopped` (on a multi-thread
    /// runtime an actor already running can take the request first).
    ///
    /// # Panics
    /// Panics when no Tokio runtime is current on the calling thread, or the
    /// current one was built without the time driver (`Builder::enable_time`
    /// or `enable_all`), before the call changes anything; that is the only
    /// runtime the check refuses, and a calling runtime that is shutting down
    /// or has ended passes it; an actor that no
    /// longer takes the request is answered with `Err` first, on any
    /// runtime. The call needs a
    /// runtime that is driven, not only entered: for a reason other than
    /// `Kill`, its escalation steps run on the calling runtime, so a caller
    /// polled by a foreign executor under a runtime nothing drives completes
    /// only when the actor ends without the ladder (by itself, or from the
    /// `Kill` that a zero window raises inside the call). A `Kill` call
    /// completes when the actor ends, from the `Kill` or from the abort the
    /// system armed on the actor's own runtime; its `Unresponsive` answer
    /// needs the calling runtime to be driven. If the
    /// calling runtime ends, the system hands the steps of a call with a reason
    /// other than `Kill` to the actor's own runtime (with the exceptions
    /// above), which ends the actor; a call of either kind that is polled by a
    /// foreign executor with the ended runtime still entered panics at its next
    /// timed wait, unless the actor has reached a terminal status by then (a
    /// poll with no runtime context panics at a non-`Kill` call's closing wait
    /// whatever the actor's status).
    /// With `panic = "abort"` the entry cannot tell whether the runtime has the
    /// time driver, so a call on a runtime without it ends the process at the
    /// first timer it uses.
    ///
    /// # Calling oneself
    /// Called on oneself from one of the actor's own callbacks the call is not
    /// refused, and it does not return to the callback: the callback waits for
    /// the actor's end, which the waiting callback holds up. Only the ladder's
    /// `Kill` and the abort the system arms end the actor, and that drops the
    /// callback and the call with it.
    ///
    /// # Errors
    /// Returns `SendError::Closed` if the actor does not accept the
    /// request any more, the same case [`stop`](Self::stop) reports.
    pub async fn stop_within(
        &self,
        reason: StopReason,
        within: Duration,
    ) -> Result<StopOutcome, SendError> {
        // An actor that does not take the request is answered first, as
        // `stop` answers it: that needs no timer. The timer check comes
        // next, and the raise, which changes state, after it.
        if self.stop_lane.refuses(&reason) {
            emit_stop_requested(
                None,
                &self.stop_lane,
                &reason,
                RaiseResult::Refused,
                StopOrigin::Requested,
                None,
            );
            return Err(SendError::Closed);
        }
        crate::time_probe::require_caller_timers("ActorHandle::stop_within");
        let caller_runtime = Handle::current();
        // Every deadline of the call counts from this instant.
        let call = Instant::now();
        let mut rx = self.status_rx.clone();
        let is_kill = matches!(reason, StopReason::Kill);
        // The incarnation's own kill grace, read while its registry entry
        // still holds it: the entry is freed before the actor publishes
        // `Stopped`, and the Kill step's wait must hold after that. An actor
        // already out of the registry refuses every reason but `Kill` below.
        let system = self.system();
        let kill_grace = system
            .as_ref()
            .and_then(|system| system.kill_target_for(&self.id, &self.status_rx))
            .map_or(DEFAULT_KILL_GRACE, |(_, grace, _)| grace);
        let timing = (call, within, kill_grace);
        // A `Kill` queues its request event in the shared raise.
        let kept = (!is_kill && crate::telemetry::engaged_for(self.stop_lane.abort_record(), None))
            .then(|| reason.clone());
        // A call made from inside a task spawns the stop task before it
        // raises the first request, so the actor that the request wakes
        // keeps the worker's next-task slot and runs on the worker that
        // raised it, and the stop task stays asleep until the call is over
        // or the window ends, and takes no step until the raise is decided.
        // A request the lane refuses ends the task before it takes any step.
        // A call made from outside every task has no such slot to keep: it
        // raises first, so the actor is the first to be picked up, and its
        // stop task watches the status plane itself. An actor already out of
        // its status plane refuses every request, so it gets no task.
        let in_task = tokio::task::try_id().is_some();
        let mut ladder = match &system {
            Some(system) if !is_kill && in_task && read_status(&rx) != ActorStatus::Stopped => {
                Some(spawn_ladder(
                    system,
                    &caller_runtime,
                    &self.id,
                    &self.stop_lane,
                    &rx,
                    (timing, in_task),
                    false,
                ))
            }
            _ => None,
        };
        // Decides the first request as refused if the call unwinds before it
        // decides: a panic inside the raise after the lane stored the request
        // counts as refused too (the raise has no such panic site outside
        // the tests' hooks).
        let first_request = FirstRequest(ladder.as_ref().map(|ladder| ladder.step.clone()));
        #[cfg(test)]
        if let Some(system) = &system {
            system.run_ladder_hook(crate::system::LadderPoint::StopBeforeRaise, &self.id);
        }
        let raised = if is_kill {
            raise_kill_declared(
                system.as_deref(),
                KillKey::Id(&self.id),
                &self.status_rx,
                &self.stop_lane,
                Some(within),
                None,
                || {},
                None,
            )
            .result
        } else {
            self.stop_lane.try_raise(reason)
        };
        #[cfg(test)]
        if let Some(system) = &system {
            system.run_ladder_hook(crate::system::LadderPoint::StopRaised, &self.id);
        }
        // The decision follows the raise with nothing awaited in between.
        if raised == RaiseResult::Refused {
            if let Some(step) = &first_request.0 {
                step.decide_refused();
            }
            #[cfg(test)]
            if let Some(system) = &system {
                system.run_ladder_hook(crate::system::LadderPoint::StopDecided, &self.id);
            }
            if let Some(kept) = &kept {
                emit_stop_requested(
                    system.as_deref(),
                    &self.stop_lane,
                    kept,
                    raised,
                    StopOrigin::Requested,
                    None,
                );
            }
            return Err(SendError::Closed);
        }
        let task_lost = first_request
            .0
            .as_ref()
            .is_some_and(|step| step.decide_accepted());
        #[cfg(test)]
        if let Some(system) = &system {
            system.run_ladder_hook(crate::system::LadderPoint::StopDecided, &self.id);
        }
        if task_lost {
            // The stop task was lost before it could know the request was
            // accepted, so it handed nothing over: the caller hands over the
            // steps the task owed, before it awaits anything.
            if let (Some(system), Some(ladder)) = (&system, &ladder) {
                hand_off_steps(
                    system,
                    &self.id,
                    &self.stop_lane,
                    &rx,
                    &ladder.step,
                    (call, within),
                );
            }
        }
        if let Some(kept) = &kept {
            emit_stop_requested(
                system.as_deref(),
                &self.stop_lane,
                kept,
                raised,
                StopOrigin::Requested,
                None,
            );
        }

        if is_kill {
            // The `Kill` and its backstop are the system's from the raise.
            if wait_terminal(&mut rx, within).await {
                return Ok(terminal_outcome(&rx, StopOutcome::Killed));
            }
            // The abort after the window is the system's backstop, armed above:
            // the caller only waits for the end and classifies it.
            if wait_terminal(&mut rx, ABORT_SETTLE).await {
                return Ok(terminal_outcome(&rx, StopOutcome::Killed));
            }
            return Ok(StopOutcome::Unresponsive);
        }

        let Some(system) = system else {
            // A target with no system has no helper slot and no registry:
            // the ladder runs in the caller's own future.
            let mut task = StopTask::new(
                None,
                self.id.clone(),
                self.stop_lane.clone(),
                rx,
                Arc::new(StopStep::accepted()),
                timing,
                None,
            );
            return Ok(task.run().await);
        };
        let Ladder {
            answer: mut answer_rx,
            step,
        } = ladder.take().unwrap_or_else(|| {
            spawn_ladder(
                &system,
                &caller_runtime,
                &self.id,
                &self.stop_lane,
                &rx,
                (timing, in_task),
                true,
            )
        });

        // A window that has passed at the call leaves nothing to wait for:
        // the `Kill` is raised here, in the call that raised the request,
        // not one scheduler hop later in the stop task, so an actor that has
        // not run since the request reads the `Kill` first.
        if saturating_deadline(call, within) <= call {
            raise_kill_declared(
                Some(&system),
                KillKey::Id(&self.id),
                &rx,
                &self.stop_lane,
                None,
                Some(&step),
                || {},
                None,
            );
        }

        // The caller only waits, for the task's answer and for the actor's
        // end together: whichever comes first decides, and both read the
        // same step cell, so one condition gets one outcome.
        let mut plane = rx.clone();
        let outcome = tokio::select! {
            biased;
            answered = &mut answer_rx => match answered {
                Ok(outcome) => outcome,
                Err(_) => {
                    closed_answer_outcome(
                        &system,
                        &self.id,
                        &mut rx,
                        &step,
                        timing,
                    )
                    .await
                }
            },
            _ = wait_reason(&mut plane) => terminal_outcome(&rx, step.requested().await),
        };
        Ok(outcome)
    }

    /// Requests a status snapshot from the actor.
    ///
    /// This bypasses the mailbox and uses the system channel, so it
    /// responds even when the mailbox is full.
    ///
    /// # Calling oneself
    /// A call from the actor's own task, such as a callback on
    /// `ctx.self_handle()`, returns [`AskError::CallingSelf`] at once, as
    /// [`send`](Self::send) does: the actor cannot service its system channel
    /// while it is busy running that very callback. A task spawned from a
    /// callback that calls this actor while the callback awaits it still parks
    /// forever. A descendant that queries this
    /// actor while this actor is stopping has the same shape: the query is
    /// answered with the actor's end, and the actor's teardown waits for the
    /// descendant, so under [`Shutdown::Infinity`](crate::types::Shutdown::Infinity)
    /// neither returns. For a hung actor's status without these risks, use
    /// the always-answering [`status`](Self::status).
    ///
    /// # Errors
    /// - [`AskError::CallingSelf`] if the call is made from the actor's own
    ///   task (nothing was sent).
    /// - [`AskError::Closed`] if the actor's system channel is closed: the
    ///   actor has ended, or is past its message loop.
    /// - [`AskError::ResponseDropped`] if the actor stopped after accepting
    ///   the query but before answering. It carries the reason the actor
    ///   ended with, and is returned once that reason is final, which waits
    ///   through the rest of the actor's teardown (the stop of its children
    ///   included) and has no bound of its own: bound the call with
    ///   `tokio::time::timeout` when the target's teardown may not return.
    pub async fn get_status(&self) -> Result<ActorStatusInfo, AskError> {
        self.refuse_self_call()?;
        let (tx, rx) = oneshot::channel();
        self.stop_lane
            .abort_record()
            .submit_status(tx)
            .await
            .map_err(|()| AskError::Closed)?;
        match rx.await {
            Ok(info) => Ok(*info),
            Err(_) => Err(self.response_dropped().await),
        }
    }

    /// Waits until the actor reaches its terminal [`ActorStatus::Stopped`]
    /// state and returns the reason it stopped with.
    ///
    /// Called on oneself from one of the actor's own callbacks it never
    /// completes: the actor cannot end while the callback waits for its end.
    /// It is not refused, since a waiter on another task is legitimate.
    ///
    /// Resolves for every way an actor can die: a graceful stop, a kill, and
    /// even a task abort. An abort drops the status sender without a final
    /// status write (nobody is left to report `Stopped`), so an actor-side
    /// sender drop is also treated as terminal here: the wait still returns
    /// instead of hanging forever, with [`StopReason::Kill`], the reason of an
    /// aborted actor. For an actor that is already stopped the call returns
    /// at once.
    ///
    /// The reason is the one the actor's supervisor receives when it has one:
    /// every party waiting on the actor's end reads one terminal reason. A
    /// start cut short by its own `start_timeout` reads `Kill` here, since
    /// the cut that ends it is a kill.
    ///
    /// Uses the same runtime status plane as [`status`](Self::status), so it
    /// resolves even for an actor hung inside a callback (for any caller but
    /// that actor's own callback, see above).
    ///
    /// This resolves when THIS actor has stopped. A supervising parent
    /// observes the same death separately, in its own run loop, so
    /// [`on_child_stopped`](crate::actor::Actor::on_child_stopped) may run
    /// slightly after this call returns.
    ///
    /// # Wakers
    /// A waker belongs to the task that polls with it, and the crate wakes it
    /// on that task's behalf when the actor's status changes. A panic of that
    /// waker is caught where the crate wakes it: it never reaches the actor,
    /// which ends with the reason it has. Tokio ends its wake of a list of
    /// waiting tasks at the waker that panics, so the tasks behind it in that
    /// list may be left unwoken, and the waits on one actor's status share one
    /// list: this call, the first part of
    /// [`SubtreeObserver::wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended)
    /// and the crate's own waits on the actor. A call behind a waker that
    /// panics may therefore stay pending after the actor has stopped, and
    /// returns when its task is next polled, not before. Poll it from a task
    /// whose waker does not panic.
    pub async fn wait_stopped(&self) -> StopReason {
        let mut rx = self.status_rx.clone();
        wait_reason(&mut rx).await
    }

    /// Returns an observer of the subtree of this incarnation: the actor and
    /// every actor below it. It works before and after the actor's end, so it
    /// needs no ordering with a stop or a kill. It names the incarnation this
    /// handle names; a handle kept across a supervised restart names the
    /// earlier one.
    ///
    /// Await it with
    /// [`SubtreeObserver::wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended).
    pub fn observe_subtree(&self) -> SubtreeObserver {
        SubtreeObserver::new(self.status_rx.clone())
    }
}

/// Observes the end of one actor and of every actor below it: the children its
/// supervision spawned, and theirs, at any depth.
///
/// Take it with
/// [`ActorSystem::observe_subtree`](crate::system::ActorSystem::observe_subtree)
/// or [`ActorHandle::observe_subtree`] and await it with
/// [`wait_ended`](Self::wait_ended). An observer reads the actor's runtime
/// status plane, which also records the actor's subtree. It holds no
/// [`ActorHandle`], mailbox sender or registry entry, so it keeps neither the
/// actor nor any actor below it alive, and cloning it is cheap. It names one
/// incarnation: a supervised child that is killed and restarted has a new
/// subtree, which an observer of the earlier incarnation does not wait for.
///
/// # Wakers
/// A waker belongs to the task that polls with it, and the crate wakes it on
/// that task's behalf when the actor or its subtree changes. A panic of that
/// waker is caught where the crate wakes it: it never reaches the actors, their
/// supervisors or the crate's own waits on the subtree (a restart's wait for
/// the earlier incarnation, `terminate_child`, `spawn_child`). What that catch
/// does not do for the tasks that wait behind such a waker is stated on
/// [`wait_ended`](Self::wait_ended).
#[derive(Clone)]
pub struct SubtreeObserver {
    status_rx: StatusRx,
}

impl std::fmt::Debug for SubtreeObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubtreeObserver").finish_non_exhaustive()
    }
}

impl SubtreeObserver {
    pub(crate) fn new(status_rx: StatusRx) -> Self {
        Self { status_rx }
    }

    /// Waits until the observed actor and every actor below it have ended, and
    /// returns [`SubtreeEnd::Ended`]. When they have ended already it returns
    /// at once, with no timer. When it returns `Ended`, the names and ids
    /// those actors held are free.
    ///
    /// The wait has two parts. The first is the actor's own end: it goes on
    /// once the actor has ended and let go of everything it held, its hold on
    /// its subtree last, which is after [`ActorHandle::wait_stopped`] would
    /// have returned. It needs no timer and has no bound of its own: wrap the
    /// call in `tokio::time::timeout` to bound it. The
    /// second is the subtree, bounded from the moment the call sees the
    /// actor's end by the longest chain of stop budgets below the actor (a
    /// worker's kill grace, a supervisor child's `Shutdown` window plus its
    /// kill grace, and for a worker that has called `trap_exit` its own
    /// `Shutdown` window as well), each followed by a short settle time, plus
    /// one more settle time. Past that bound the wait gives no verdict on an
    /// actor before the abort the system owes it has had the settle time to
    /// end it. The bound is absent while a supervisor child under
    /// [`Shutdown::Infinity`](crate::types::Shutdown::Infinity) runs below the
    /// actor, or a worker under `Infinity` that traps exit signals (from its
    /// call to `trap_exit` until it ends), whose orderly stop has none, and
    /// when a budget is too large to count (a kill grace of `Duration::MAX`, say): the second part then ends
    /// only when the actors below do. When the bound runs out with an actor
    /// below still running the answer is [`SubtreeEnd::Unresponsive`]; the wait
    /// stops nothing, and a later call can answer `Ended`.
    ///
    /// [`ActorSystem::kill_by_id`] reports the killed actor when it itself
    /// ended, and the actors below it end after it; this wait is how a caller
    /// waits for them.
    ///
    /// The call only reads: dropping it, or the observer, cancels nothing and
    /// leaves nothing owed. Its answer is for the caller alone: it records no
    /// verdict on any supervisor and logs nothing. Called from a callback of
    /// the observed actor or of an actor below it, it waits for the caller's
    /// own end and cannot answer `Ended`.
    ///
    /// # Panics
    /// Panics when the call needs a timer and the calling thread cannot give
    /// it one: the observed actor has ended and let go of everything it held,
    /// an actor below it has not ended, and either no Tokio runtime is
    /// current or the current runtime was built without the time driver
    /// (`Builder::enable_time` or `enable_all`). A runtime that is shutting
    /// down or has ended passes the check, and a call polled by a foreign
    /// executor with such a runtime still entered then panics at its next
    /// timed wait, unless the subtree has ended by then. A call that finds the
    /// actor still running, or still letting go of what it held, waits for it
    /// without a timer, and a call that finds its subtree already ended
    /// answers without one: that wait does not panic, on any runtime or none. With
    /// `panic = "abort"` the check cannot tell whether the runtime has the
    /// time driver, so a call on a runtime without it ends the process at the
    /// first timer it uses.
    ///
    /// # Wakers
    /// Tokio ends its wake of a list of waiting tasks at the waker that
    /// panics, so the tasks behind it in that list may be left unwoken. The
    /// observers of one subtree share a list for the wait on the subtree, and
    /// the wait on the actor's own end shares the actor's status list with
    /// [`ActorHandle::wait_stopped`] and the crate's own waits on the actor. A
    /// `wait_ended` call behind a waker that panics, and a `wait_stopped` call
    /// behind the waker of an observer, may stay pending after the end they
    /// wait for, and return when their task is next polled, not before. Poll an
    /// observer from a task whose waker does not panic.
    ///
    /// # Calling oneself
    /// Called on an observer of one's own incarnation from one of the actor's
    /// own callbacks, the call never completes: the end it waits for cannot
    /// come while the callback waits. It is not refused, since a waiter on
    /// another task is legitimate.
    pub async fn wait_ended(&self) -> SubtreeEnd {
        // The actor's own end comes first and needs no timer. The plane
        // closes once the task has dropped what it owned, and the actor's
        // hold on its subtree is released a step after it, so the bound below
        // never counts time the actor's own end took.
        let mut rx = self.status_rx.clone();
        wait_closed(&mut rx).await;
        // The link is read after the end, never when the observer is built:
        // the registry shows an actor before its subtree is linked, and the
        // link is set before the task is spawned, so a task that ended has one
        // unless it never ran.
        let Some(watch) = self
            .status_rx
            .subtree_watch()
            .map(SubtreeWatch::for_observer)
        else {
            return SubtreeEnd::Ended;
        };
        // The hold is the last thing the actor lets go of. Until it is
        // released the subtree cannot be told from one with an actor still
        // running below, and the answer for an actor with nothing below it
        // needs no timer, so the wait goes on without one.
        watch.released().await;
        if watch.is_gone() {
            return SubtreeEnd::Ended;
        }
        // The timer check comes at the first moment a timer is needed, after
        // everything that needs none has been answered.
        crate::time_probe::require_caller_timers("SubtreeObserver::wait_ended");
        let mut wait = SubtreeWait::new(&watch);
        loop {
            match successor_wait_step(wait.gone(), false, wait.expired()) {
                WaitAction::Keep => wait.changed().await,
                WaitAction::Escalate => return SubtreeEnd::Unresponsive,
                // A cancel needs an order, and this wait never has one.
                WaitAction::Proceed | WaitAction::Cancel => return SubtreeEnd::Ended,
            }
        }
    }
}

/// The last steps of every bounded stop: aborts the task when the registry
/// still holds its abort handle, then waits the settle time. `requested` is
/// what a `Stopped` the actor publishes reads as.
async fn abort_and_settle(
    system: Option<&ActorSystem>,
    id: &ActorId,
    lane: &StopLane,
    rx: &mut StatusRx,
    requested: StopOutcome,
) -> StopOutcome {
    if let Some(system) = system {
        if let Some((Some(abort), _, _)) = system.kill_target_for(id, rx) {
            lane.abort_record().abort(&abort);
        }
    }
    if wait_terminal(rx, ABORT_SETTLE).await {
        return terminal_outcome(rx, requested);
    }
    StopOutcome::Unresponsive
}

/// The instant at which the whole ladder of a call made at `call` is over:
/// the window, the kill grace and the settle wait, each added with its own
/// cap so that no sum is capped as a whole.
fn ladder_end(call: Instant, within: Duration, kill_grace: Duration) -> Instant {
    saturating_deadline(
        saturating_deadline(saturating_deadline(call, within), kill_grace),
        ABORT_SETTLE,
    )
}

/// The outcome of a bounded stop whose stop task ended without answering,
/// classified from the step cell and the status plane: the plane reads
/// `Stopped` gives what that step asked for, a plane closed without it
/// gives `Aborted`, and a plane that is not terminal when the ladder's own
/// bound has passed gives `Unresponsive`.
async fn closed_answer_outcome(
    system: &ActorSystem,
    id: &ActorId,
    rx: &mut StatusRx,
    step: &StopStep,
    (call, within, kill_grace): (Instant, Duration, Duration),
) -> StopOutcome {
    #[cfg(test)]
    system.run_ladder_hook(crate::system::LadderPoint::StopClosedSeen, id);
    #[cfg(not(test))]
    let _ = (system, id);
    if wait_terminal_until(rx, ladder_end(call, within, kill_grace)).await {
        terminal_outcome(rx, step.requested().await)
    } else {
        StopOutcome::Unresponsive
    }
}

/// The task of one bounded stop, as its caller holds it: the receiver of the
/// task's answer and the step cell both read.
struct Ladder {
    answer: oneshot::Receiver<StopOutcome>,
    step: Arc<StopStep>,
}

/// The caller's hold on the decision of its first request: dropping it
/// decides the request as refused when the call unwound before deciding.
struct FirstRequest(Option<Arc<StopStep>>);

impl Drop for FirstRequest {
    fn drop(&mut self) {
        if let Some(step) = &self.0 {
            step.decide_refused();
        }
    }
}

/// Spawns the stop task of a call on `rt` through the system's helper slot.
/// With `accepted` false the task takes no step until the caller decides the
/// call's first request.
/// The task holds the stop lane, the status receiver, the system and plain
/// values, never an `ActorHandle` and never a mailbox sender.
fn spawn_ladder(
    system: &Arc<ActorSystem>,
    rt: &Handle,
    id: &ActorId,
    lane: &StopLane,
    rx: &StatusRx,
    (timing, deferred): ((Instant, Duration, Duration), bool),
    accepted: bool,
) -> Ladder {
    let step = Arc::new(if accepted {
        StopStep::accepted()
    } else {
        StopStep::default()
    });
    let (answer_tx, answer_rx) = oneshot::channel();
    let owner = HelperOwner::new(id.clone(), Some(lane.abort_record().clone()));
    drop(
        system.spawn_helper(rt, HelperKind::StopLadder, Some(owner), {
            let task_system = system.clone();
            let (id, lane, rx, step) = (id.clone(), lane.clone(), rx.clone(), step.clone());
            move |end| {
                // Built here, outside the task's future, so a task that is
                // dropped before its first poll is covered too.
                let mut task = StopTask::new(
                    Some(task_system),
                    id,
                    lane,
                    rx,
                    step,
                    timing,
                    Some(answer_tx),
                );
                task.deferred = deferred;
                end.observe(async move {
                    let outcome = task.run().await;
                    task.answer(outcome);
                    #[cfg(test)]
                    task.hook(crate::system::LadderPoint::StopAnswered);
                })
            }
        }),
    );
    Ladder {
        answer: answer_rx,
        step,
    }
}

/// The ladder of one bounded stop whose first request is raised. It is the
/// stop task of the system and the guard of that task in one value: the
/// task takes the steps, answers the caller through `answer`, and when the
/// task is lost before its last wait, by a panic or because its runtime
/// ended, dropping the value hands the steps that are still owed to the
/// reaper of the actor's own runtime, before the answer channel closes.
struct StopTask {
    system: Option<Arc<ActorSystem>>,
    id: ActorId,
    lane: StopLane,
    rx: StatusRx,
    step: Arc<StopStep>,
    /// The instant of the call, the window and the kill grace.
    call: Instant,
    within: Duration,
    kill_grace: Duration,
    /// Set once the ladder has run to its end.
    done: bool,
    /// Whether the task sleeps until the window ends or the call goes away
    /// before it reads the status plane.
    deferred: bool,
    answer: Option<oneshot::Sender<StopOutcome>>,
}

impl StopTask {
    fn new(
        system: Option<Arc<ActorSystem>>,
        id: ActorId,
        lane: StopLane,
        rx: StatusRx,
        step: Arc<StopStep>,
        (call, within, kill_grace): (Instant, Duration, Duration),
        answer: Option<oneshot::Sender<StopOutcome>>,
    ) -> Self {
        Self {
            system,
            id,
            lane,
            rx,
            step,
            call,
            within,
            kill_grace,
            done: false,
            deferred: false,
            answer,
        }
    }

    #[cfg(test)]
    fn hook(&self, point: crate::system::LadderPoint) {
        if let Some(system) = &self.system {
            system.run_ladder_hook(point, &self.id);
        }
    }

    /// Waits up to the window for a terminal status, raises `Kill` and arms
    /// its backstop, waits the kill grace, then aborts and settles. A `Kill`
    /// the lane refuses belongs to an actor already past its last read of
    /// it: it ends with its own reason, and the outcome reads `Graceful`.
    async fn run(&mut self) -> StopOutcome {
        #[cfg(test)]
        self.hook(crate::system::LadderPoint::StopHelperStart);
        let window_end = saturating_deadline(self.call, self.within);
        // A deferred task needs nothing of the call before the window ends:
        // the caller reads the status plane itself and returns, or is
        // dropped, which closes the answer. The task sleeps until the first
        // of the two and reads the plane then.
        if let Some(answer) = self.answer.as_mut().filter(|_| self.deferred) {
            tokio::select! {
                biased;
                () = answer.closed() => {}
                () = time::sleep_until(window_end) => {}
            }
        }
        if !self.step.first_request_accepted().await {
            // The lane refused the call's first request: no step is owed.
            self.done = true;
            return StopOutcome::Graceful;
        }
        if wait_terminal_until(&mut self.rx, window_end).await {
            self.done = true;
            return terminal_outcome(&self.rx, self.step.requested().await);
        }
        #[cfg(test)]
        self.hook(crate::system::LadderPoint::StopWithinKill);
        #[cfg(test)]
        let stored = || self.hook(crate::system::LadderPoint::StopKillStored);
        #[cfg(not(test))]
        let stored = || {};
        // A call whose window had passed at the call took this step itself.
        if !self.step.kill_started() {
            raise_kill_declared(
                self.system.as_deref(),
                KillKey::Id(&self.id),
                &self.rx,
                &self.lane,
                None,
                Some(&self.step),
                stored,
                None,
            );
        }
        #[cfg(test)]
        self.hook(crate::system::LadderPoint::StopAfterKill);
        let outcome = if wait_terminal(&mut self.rx, self.kill_grace).await {
            terminal_outcome(&self.rx, self.step.requested().await)
        } else {
            let requested = self.step.requested().await;
            abort_and_settle(
                self.system.as_deref(),
                &self.id,
                &self.lane,
                &mut self.rx,
                requested,
            )
            .await
        };
        self.done = true;
        outcome
    }

    /// Sends the outcome to the caller, if it still waits.
    fn answer(&mut self, outcome: StopOutcome) {
        self.done = true;
        if let Some(answer) = self.answer.take() {
            let _ = send_contained(answer, outcome);
        }
    }

    /// The steps a lost task still owed (see [`hand_off_steps`]).
    fn carry_on(&self) {
        let Some(system) = &self.system else { return };
        hand_off_steps(
            system,
            &self.id,
            &self.lane,
            &self.rx,
            &self.step,
            (self.call, self.within),
        );
    }
}

/// The steps a lost stop task still owed: the `Kill` at the end of the window
/// and the abort after the kill grace, both counted from the call's first poll, as
/// one entry of the reaper on the actor's own runtime. A target whose
/// abort handle is not attached yet gets its `Kill` at once, and the
/// attach arms the abort. In that window the `Kill` comes when the task
/// is lost, which can be before the end of the window the call was given,
/// and a `pre_stop` veto of the earlier request is cut short by that
/// much. A target out of the registry is past the point where either
/// step could matter. A target on a runtime without the time driver gets
/// no entry, which that runtime's reaper could not hold: its steps are
/// not taken, except a `Kill` whose window has already passed.
fn hand_off_steps(
    system: &Arc<ActorSystem>,
    id: &ActorId,
    lane: &StopLane,
    rx: &StatusRx,
    step: &Arc<StopStep>,
    (call, within): (Instant, Duration),
) {
    let Some((abort, grace, runtime)) = system.kill_target_for(id, rx) else {
        return;
    };
    match abort {
        Some(abort) => {
            let kill_at = saturating_deadline(call, within);
            if !crate::time_probe::runtime_has_timers(&runtime) {
                // The reaper of a runtime without the time driver cannot
                // hold an entry, and the entry's drop would abort the
                // target before its window: the lost step does nothing,
                // unless the window has already passed.
                // The `Kill` is due, and the reaper of such a runtime ends
                // the target at once, as it does for every `Kill` there.
                if Instant::now() >= kill_at {
                    raise_kill_declared(
                        Some(system),
                        KillKey::Id(id),
                        rx,
                        lane,
                        None,
                        Some(step),
                        || {},
                        None,
                    );
                }
                return;
            }
            let entry = ReaperEntry::two_stage(
                kill_at,
                lane.clone(),
                saturating_deadline(kill_at, grace),
                abort,
            )
            .with_step(step.clone());
            system.feed_reaper(&runtime, entry);
        }
        None => {
            raise_kill_declared(
                Some(system),
                KillKey::Id(id),
                rx,
                lane,
                None,
                Some(step),
                || {},
                None,
            );
        }
    }
}

impl Drop for StopTask {
    fn drop(&mut self) {
        if !self.done {
            // A first request not decided yet leaves the loss to the caller,
            // which hands the steps over when it finds the request accepted.
            match self.step.lose_before_the_last_wait() {
                LossOwes::Steps => {
                    self.carry_on();
                    #[cfg(test)]
                    self.hook(crate::system::LadderPoint::StopHelperLost);
                }
                LossOwes::Nothing =>
                {
                    #[cfg(test)]
                    if !self.step.refused() {
                        self.hook(crate::system::LadderPoint::StopHelperLost);
                    }
                }
            }
        }
        // The caller sees the closed answer only once the steps are handed
        // over, so the answer sender is dropped last. Dropping it wakes the
        // caller of the stop, whose waker belongs to the caller's task, and
        // this drop can run while the task unwinds: a panic of that waker
        // stays out of it.
        if let Some(answer) = self.answer.take() {
            contain_waker_panic("stop answer", Some(self.id.as_who()), move || drop(answer));
        }
    }
}

/// Polls a cast's wait for a mailbox unit. A wait that parked on the
/// actor's own task is given up at once, with nothing reported as a wait, and
/// answers `Full`: the task that would wait is the one that frees units. A
/// poll that only yielded to the task's budget is not a park and goes on.
fn poll_cast_unit<'a, T>(
    mut reserve: Pin<&mut mailbox::Reserve<'a, T>>,
    cx: &mut Context<'_>,
    record: &AbortRecord,
) -> Poll<Result<mailbox::Permit<'a, T>, SendError>> {
    match reserve.as_mut().poll(cx) {
        Poll::Ready(result) => Poll::Ready(result.map_err(|_| SendError::Closed)),
        Poll::Pending if reserve.is_parked() && record.is_self_call() => {
            reserve.withdraw();
            Poll::Ready(Err(SendError::Full))
        }
        Poll::Pending => Poll::Pending,
    }
}

/// A reserve of the priority lane found room: the next wait on a full lane
/// warns again, with the `tracing` feature.
fn lane_room(warned: Option<&AtomicBool>) {
    #[cfg(feature = "tracing")]
    if let Some(warned) = warned {
        warned.store(false, Ordering::Relaxed);
    }
    #[cfg(not(feature = "tracing"))]
    let _ = warned;
}

/// Reserves a slot of the priority lane, waiting while the lane is full; `None`
/// once the lane is closed. The permit and the pending wait are held as
/// [`reserve_held`] holds them. With the `tracing` feature a wait on a full
/// lane warns once, until a reserve finds room again, using the lane's
/// `warned` flag.
async fn reserve_priority<'a, M>(
    lane: &'a mpsc::Sender<M>,
    owner: &ActorId,
    warned: Option<&AtomicBool>,
    record: &AbortRecord,
) -> Option<WakeHold<mpsc::Permit<'a, M>>> {
    reserve_held("priority lane", Some(owner), lane, |full| {
        #[cfg(feature = "tracing")]
        if !full {
            if let Some(warned) = warned {
                warned.store(false, Ordering::Relaxed);
            }
        } else if !warned.is_some_and(|warned| warned.swap(true, Ordering::Relaxed)) {
            let fields = crate::telemetry::log_fields(record, None);
            caught_log!(warn,
                actor_id = %owner,
                actor_name = fields.name(),
                incarnation = fields.incarnation(),
                system = fields.system(),
                "priority lane full: a priority send is waiting for capacity"
            );
        }
        #[cfg(not(feature = "tracing"))]
        let _ = (full, warned, record);
    })
    .await
}

/// The payload of the message `try_forward` put in, which comes back from a
/// refused send as the envelope it was sent in.
fn own_message<A: Actor>(envelope: ActorEnvelope<A>) -> A::Message {
    match envelope {
        Envelope::Message { payload, .. } | Envelope::Signal { payload, .. } => payload,
    }
}

/// A slot reserved in an actor's mailbox or priority lane by
/// [`reserve_forward`](ActorHandle::reserve_forward) or
/// [`reserve_signal`](ActorHandle::reserve_signal): sending through it cannot
/// wait or fail, and dropping it frees the slot.
pub(crate) enum ForwardPermit<'a, A: Actor> {
    /// A slot of the normal mailbox.
    Normal(mailbox::Permit<'a, ActorEnvelope<A>>),
    /// A slot of the priority lane, with the lane's sender, which reports
    /// whether the lane was still open once the message was put in.
    Priority(
        WakeHold<mpsc::Permit<'a, ActorEnvelope<A>>>,
        &'a mpsc::Sender<ActorEnvelope<A>>,
    ),
}

impl<A: Actor> ForwardPermit<'_, A> {
    /// Puts `envelope` in the queue the permit was reserved on. Returns
    /// whether it landed: `false` means the queue was closed and the message
    /// was dropped.
    fn put(self, envelope: ActorEnvelope<A>) -> bool {
        match self {
            ForwardPermit::Normal(permit) => permit.send(envelope),
            ForwardPermit::Priority(permit, lane) => {
                permit.into_inner().send(envelope);
                // Tokio's permit cannot say whether the receiver took the
                // value; a lane closed after the send dropped it with the
                // queue, and one still open holds it.
                !lane.is_closed()
            }
        }
    }

    /// Puts `msg` in the queue the permit was reserved on.
    pub(crate) fn send(self, msg: A::Message) {
        self.put(Envelope::Message {
            payload: msg,
            responder: None,
        });
    }

    /// Puts `msg`, tagged as a monitor's notice, in the queue the permit was
    /// reserved on. Returns whether it landed.
    pub(crate) fn send_signal(self, tag: SignalTag, msg: A::Message) -> bool {
        self.put(Envelope::Signal { tag, payload: msg })
    }
}

#[cfg(test)]
#[path = "handle_late_slot_tests.rs"]
pub(crate) mod late_slot_tests;

#[cfg(test)]
#[path = "stop_ladder_tests.rs"]
mod stop_ladder_tests;

#[cfg(test)]
#[path = "bounded_stop_hold_tests.rs"]
mod bounded_stop_hold_tests;

#[cfg(test)]
#[path = "handle_subtree_observer_tests.rs"]
mod subtree_observer_tests;
