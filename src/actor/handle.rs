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
use std::pin::Pin;
#[cfg(feature = "tracing")]
use std::sync::atomic::Ordering;
use std::sync::atomic::{fence, AtomicBool};
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::{mpsc, oneshot};
#[cfg(test)]
use tokio::time::Instant;

use crate::actor::envelope::{Envelope, SignalTag};
use crate::actor::incarnation::{IncarnationRef, RaiseResult, StatusRx, StopLane};
use crate::actor::mailbox::MailTx;
use crate::actor::panic::{reserve_held, WakeHold};
use crate::actor::{mailbox, Actor, ActorEnvelope};
use crate::error::{ActorError, AskError, SendError};
use crate::system::ActorSystem;
#[cfg(test)]
use crate::system::StopStep;
use crate::telemetry::wait_graph::{begin_wait, wait_wanted, WaitEdge};
use crate::telemetry::{EventKind, RaiseOutcome, StopOrigin, TelemetryEvent};
use crate::types::{ActorId, StopReason};

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
    record: &'a IncarnationRef,
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
    fn new(fut: Pin<&'a mut F>, record: &'a IncarnationRef) -> Self {
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
    /// Every part of the handle, shared by all its clones behind one count:
    /// a clone takes one reference and a drop gives one back. The mailbox
    /// closes when the last handle, wherever it is held, lets the block go.
    inner: Arc<HandleBlock<A>>,
}

/// What every clone of an actor's handle shares. The block holds the
/// mailbox's sender, so the mailbox stays open while any handle holds the
/// block and closes when the last one lets it go.
struct HandleBlock<A: Actor> {
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
            .field("id", &self.inner.id)
            .field(
                "system",
                &self
                    .inner
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
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<A: Actor> PartialEq for ActorHandle<A> {
    fn eq(&self, other: &Self) -> bool {
        self.inner.id == other.inner.id && self.system_generation() == other.system_generation()
    }
}

impl<A: Actor> Eq for ActorHandle<A> {}

impl<A: Actor> std::hash::Hash for ActorHandle<A> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.inner.id.hash(state);
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

    /// Reserves a slot in the mailbox for a message one of the actor's timers
    /// or streams sends, waiting while the mailbox is full; `None` once the
    /// mailbox is closed, and the message comes back to its sender, which
    /// releases it itself and reports that drop as made after the close.
    /// Nothing awaits between the reserve and the permit's send or drop, so a
    /// forwarder can decide at that instant whether its message is sent.
    pub(crate) async fn reserve_forward(&self) -> Option<ForwardPermit<'_, A>> {
        let tx = &self.inner.tx;
        tx.reserve().await.ok().map(ForwardPermit::Normal)
    }

    /// As [`reserve_forward`](Self::reserve_forward) for a monitor's notice:
    /// on the priority lane when `priority` is set and the actor has one,
    /// otherwise on the normal mailbox.
    pub(crate) async fn reserve_signal(&self, priority: bool) -> Option<ForwardPermit<'_, A>> {
        match (&self.inner.priority_tx, priority) {
            (Some(lane), true) => reserve_priority(
                lane,
                &self.inner.id,
                None,
                self.inner.stop_lane.abort_record(),
            )
            .await
            .map(|permit| ForwardPermit::Priority(permit, lane)),
            _ => self.reserve_forward().await,
        }
    }

    /// [`reserve_forward`](Self::reserve_forward) without waiting: a full or
    /// closed mailbox gives the message back.
    pub(crate) fn try_forward(
        &self,
        msg: A::Message,
    ) -> Result<(), mpsc::error::TrySendError<A::Message>> {
        self.inner
            .tx
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
        self.inner.tx.is_closed()
    }

    /// Completes once the actor's mailbox is closed: by its receiver's
    /// `close`, once `on_stopped` has returned, the initialization failed,
    /// or a `Kill` ended the run, or by the receiver's drop.
    pub(crate) async fn mailbox_close(&self) {
        self.inner.tx.closed().await;
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

/// Polls a cast's wait for a mailbox unit. A wait that parked on the
/// actor's own task is given up at once, with nothing reported as a wait, and
/// answers `Full`: the task that would wait is the one that frees units. A
/// poll that only yielded to the task's budget is not a park and goes on.
fn poll_cast_unit<'a, T>(
    mut reserve: Pin<&mut mailbox::Reserve<'a, T>>,
    cx: &mut Context<'_>,
    record: &IncarnationRef,
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
    record: &IncarnationRef,
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
// The parts of this module, in the order the generated docs list their methods.

mod access;

mod send;

mod stop;
#[cfg(test)]
use self::stop::*;

mod observe;

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
