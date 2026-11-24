//! Values the task owns to its end: the owner, the queue slot and drops made inside a catch.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::runtime::Handle;

use crate::actor::incarnation::IncarnationRef;
use crate::actor::mailbox::QueueRx;
use crate::actor::panic::{catch_callback, catch_sync, contain_waker_panic, payload_into_string};
use crate::helper::{EndGuard, HelperKind, HelperOwner};
use crate::system::ActorSystem;
use crate::types::ActorId;

/// Runs `drop` and catches a panic it raises.
fn catch_drop(drop: impl FnOnce()) -> Result<(), String> {
    catch_sync(drop).map_err(payload_into_string)
}

/// The actor a drop belongs to, as the log lines of a drop panic name it: its
/// id, and whether it is sensitive, so a line about a panic whose text the
/// crate does not know the origin of still hides that text for a sensitive
/// actor. It is made from one incarnation's record (see [`Owner::of`]) or at
/// that incarnation's spawn. A line about a handle dropped from a
/// process-group slot, whose text is the crate's own and never a member's
/// failure, builds it from the id alone, as not sensitive; so does the line of
/// a mailbox that holds no telemetry record, where nothing says whether the
/// actor is sensitive. The sensitive form holds the id behind a pointer, which
/// keeps the type the size of an `ActorId`.
#[derive(Clone)]
pub(crate) enum Owner {
    /// An actor whose failure text its log lines print.
    Open(ActorId),
    /// An actor whose failure text no log line prints.
    Sensitive(Arc<ActorId>),
}

impl Owner {
    /// The owner of an incarnation whose sensitivity the caller knows.
    #[inline]
    pub(crate) fn new(id: ActorId, sensitive: bool) -> Self {
        if sensitive {
            Self::Sensitive(Arc::new(id))
        } else {
            Self::Open(id)
        }
    }

    /// The owner of the incarnation `record` belongs to, named `id`.
    #[inline]
    pub(crate) fn of(id: &ActorId, record: &IncarnationRef) -> Self {
        Self::new(
            id.clone(),
            record.sys_bits() & crate::actor::incarnation::SYS_SENSITIVE != 0,
        )
    }

    /// The actor's id.
    pub(crate) fn id(&self) -> &ActorId {
        match self {
            Self::Open(id) => id,
            Self::Sensitive(id) => id,
        }
    }

    /// The identity fields of a line about this actor that carries a panic's
    /// text.
    #[cfg(feature = "tracing")]
    pub(crate) fn fields(&self) -> crate::telemetry::LogFields<'static> {
        crate::telemetry::LogFields::of_sensitive(matches!(self, Self::Sensitive(_)))
    }
}

impl std::ops::Deref for Owner {
    type Target = ActorId;

    fn deref(&self) -> &ActorId {
        self.id()
    }
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.id().fmt(f)
    }
}

impl std::fmt::Debug for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.id().fmt(f)
    }
}

/// Keeps the first of two panics of one step of the actor `owner`'s end. A
/// later one is discarded, and with the `tracing` feature it is logged,
/// naming the actor, so every panic leaves a record.
pub(crate) fn keep_first_of(first: &mut Option<String>, next: Option<String>, owner: &Owner) {
    match (first.is_some(), next) {
        (false, next) => *first = next,
        (true, Some(discarded)) => log_discarded(&discarded, owner),
        (true, None) => {}
    }
}

/// Logs, with the `tracing` feature, a panic a step discards because it
/// keeps an earlier one.
pub(crate) fn log_discarded(payload: &str, owner: &Owner) {
    #[cfg(feature = "tracing")]
    caught_log!(warn, target: "tokio_actors::actor::runtime",
        actor_id = %owner,
        panic = %owner.fields().panic_text(payload),
        "a drop panicked after an earlier panic of the same step was kept; this panic is discarded"
    );
    #[cfg(not(feature = "tracing"))]
    let _ = (payload, owner);
}

/// Something the actor owns, held until the end of its task drops it with
/// [`end`](Self::end). However else it is dropped (an abort before the first
/// poll or while suspended, or the task's own unwind), its drop goes through
/// the same `end`, which catches every panic, so drop glue never unwinds;
/// with the `tracing` feature such a panic is logged once with the actor's
/// id, and the task ends as the abort (`Kill`) or the unwind
/// (`Failure(Panic(..))`) does.
pub(crate) struct CaughtDrop<T> {
    value: Option<T>,
    end: fn(T, &Owner) -> Option<String>,
    /// The actor that owns the value, passed to `end` on every path (the
    /// end of the task, an abort or an unwind) so each log line names it.
    owner: Owner,
}

impl<T> CaughtDrop<T> {
    pub(crate) fn new(value: T, end: fn(T, &Owner) -> Option<String>, owner: Owner) -> Self {
        Self {
            value: Some(value),
            end,
            owner,
        }
    }

    pub(super) fn get_mut(&mut self) -> &mut T {
        self.value
            .as_mut()
            .expect("the value is held until its end")
    }

    /// The value, while it is held.
    pub(super) fn peek(&self) -> Option<&T> {
        self.value.as_ref()
    }

    /// Drops the value now; returns the first panic its drop raised.
    pub(super) fn end(&mut self) -> Option<String> {
        let end = self.end;
        self.value.take().and_then(|value| end(value, &self.owner))
    }
}

impl<T> Drop for CaughtDrop<T> {
    fn drop(&mut self) {
        let panic = self.end();
        #[cfg(feature = "tracing")]
        if let Some(payload) = &panic {
            caught_log!(warn, target: "tokio_actors::actor::runtime",
                actor_id = %self.owner,
                panic = %self.owner.fields().panic_text(payload),
                "a drop at the abort or the unwind of the actor's task panicked; the task ends as the abort or the unwind does"
            );
        }
        #[cfg(not(feature = "tracing"))]
        let _ = (panic, &self.owner);
    }
}

/// Drops a value, catching a panic its drop raises.
pub(crate) fn end_value<T>(value: T) -> Option<String> {
    catch_drop(move || drop(value)).err()
}

/// An actor's mailbox or priority lane as the run loop holds it, with the
/// system whose helper slot takes the receiver if the queue ends with a slot
/// still out. Dropped any way other than by [`end`](Self::end) (an abort or
/// the task's own unwind), it ends the queue the same way, every panic
/// caught, like a [`CaughtDrop`].
pub(crate) struct QueueSlot<R: QueueRx> {
    rx: Option<R>,
    /// Held only while there is a receiver to hand off.
    system: Option<Arc<ActorSystem>>,
    owner: Owner,
}

impl<R: QueueRx> QueueSlot<R> {
    pub(crate) fn new(rx: Option<R>, system: &Arc<ActorSystem>, owner: Owner) -> Self {
        let system = rx.is_some().then(|| system.clone());
        Self { rx, system, owner }
    }

    /// The mailbox's receiver, held until the end.
    pub(super) fn mailbox_mut(&mut self) -> &mut R {
        self.rx.as_mut().expect("the mailbox is held until its end")
    }

    /// The lane's receiver, which the loop lets go of when every sender of
    /// the lane is gone, and which an actor without the lane never had.
    pub(super) fn lane_mut(&mut self) -> &mut Option<R> {
        &mut self.rx
    }

    /// Ends the queue now; returns the first panic a drop raised. `record` is
    /// the actor's abort record when the caller has it.
    pub(super) fn end(&mut self, record: Option<&IncarnationRef>) -> Option<String> {
        let rx = self.rx.take()?;
        let panic = match self.system.take() {
            Some(system) => end_queue_late(rx, &system, &self.owner, record),
            None => end_queue(rx, &self.owner),
        };
        #[cfg(test)]
        crate::actor::handle::late_slot_tests::between_queue_ends(&self.owner);
        panic
    }
}

impl<R: QueueRx> Drop for QueueSlot<R> {
    fn drop(&mut self) {
        let panic = self.end(None);
        #[cfg(feature = "tracing")]
        if let Some(payload) = &panic {
            caught_log!(warn, target: "tokio_actors::actor::runtime",
                actor_id = %self.owner,
                panic = %self.owner.fields().panic_text(payload),
                "a drop at the abort or the unwind of the actor's task panicked; the task ends as the abort or the unwind does"
            );
        }
        #[cfg(not(feature = "tracing"))]
        let _ = panic;
    }
}

/// Closes a queue and drops the messages still in it one at a time, each
/// drop caught, so a second panicking message never panics inside the
/// unwinding of the first.
///
/// The close is followed by a full fence, then the drain. A requester pushes
/// its message and then reads whether the queue is closed behind the same
/// fence (see `ActorHandle::send`): in the order of the two fences, either
/// the requester's push comes before the end's fence and the drain sees the
/// message, or the end's close comes before the requester's fence and the
/// requester sees the queue closed. No request is left unseen by both.
///
/// The mailbox is covered the same way: its one queue is drained after the
/// end's fence, so a request racing the close is in the drain's reach or
/// sees the close.
///
/// Returns the first panic of the drain's drops and whether a value can still
/// arrive.
fn close_and_drain<R: QueueRx>(rx: &mut R, owner: &Owner) -> (Option<String>, bool) {
    // Closing a tokio channel wakes the senders parked for room, whose wakers
    // belong to their own tasks: a panic of one stays in this call, and the
    // drain below still runs. Tokio ends its wake at that waker, so the
    // senders behind it may stay unwoken and find the queue closed at their
    // next poll.
    contain_waker_panic("queue close", Some(owner.id().as_who()), || rx.close());
    std::sync::atomic::fence(Ordering::SeqCst);
    let mut first = None;
    loop {
        match rx.try_recv_one() {
            Ok(message) => keep_first_of(&mut first, end_value(message), owner),
            Err(open) => return (first, open),
        }
    }
}

/// Ends a queue that cannot have a slot out for a sender that is still to
/// push: closed, drained, dropped.
pub(crate) fn end_queue<R: QueueRx>(mut rx: R, owner: &Owner) -> Option<String> {
    let (mut first, _) = close_and_drain(&mut rx, owner);
    keep_first_of(&mut first, end_value(rx), owner);
    first
}

/// Ends a queue that senders may still push into through a slot reserved
/// before the close.
///
/// A request pushed through such a slot answers itself: its requester reads
/// the closed queue after its push and takes the actor's final reason from
/// the status plane (see `ActorHandle::send`), so no caller waits on this
/// end. What else arrives through a slot (a notification, a timer's message)
/// is dropped by the crate when the slot is used or given back, outside the end
/// order, and `wait_stopped` does not wait for it. The mailbox's queue
/// closes for good here, so a push from then on fails and its sender drops the
/// message in that push; a lane keeps taking what its permits send. While a
/// value can still arrive (a push already landing in the mailbox, a permit out
/// on a lane), the receiver goes to a helper task of the system that drops
/// what arrives. With no runtime to run that task, or a runtime that is
/// closing, the receiver drops here and a value that lands later is dropped
/// when the last sender is. On some Tokio releases
/// a blocked sender that is handed the slot the actor's last receive freed, and
/// is then dropped, does not wake the receiver, so the helper stays parked
/// until the queue's last sender is dropped.
pub(crate) fn end_queue_late<R: QueueRx>(
    mut rx: R,
    system: &ActorSystem,
    owner: &Owner,
    record: Option<&IncarnationRef>,
) -> Option<String> {
    let (mut first, open) = close_and_drain(&mut rx, owner);
    // The queue is empty, and a value can still arrive only through a push
    // that is landing (the mailbox, closed for good) or a permit that is out
    // (a lane): hand the receiver to a task that drops what arrives.
    if open {
        if let Ok(rt) = Handle::try_current() {
            // The slot records the task's end, whichever way it ends.
            let task_owner = owner.clone();
            let helper_owner = HelperOwner::new(owner.id().clone(), record.cloned());
            drop(
                system.spawn_helper(&rt, HelperKind::LateDrain, Some(helper_owner), |end| {
                    drain_late(LateDrain::new(rx, task_owner, end))
                }),
            );
            return first;
        }
    }
    keep_first_of(&mut first, end_value(rx), owner);
    first
}

/// Closes a queue, then drops what it holds one value at a time, each drop
/// caught; returns the first panic.
fn end_remaining<R: QueueRx>(mut rx: R, owner: &Owner) -> Option<String> {
    let (mut first, _) = close_and_drain(&mut rx, owner);
    keep_first_of(&mut first, end_value(rx), owner);
    first
}

/// What the drain of a closed queue holds: the receiver, dropped under catch
/// however the task ends. If the task is dropped before it ends, the values it
/// still holds go with it unsettled, and the slot records their number.
struct LateDrain<R: QueueRx> {
    rx: CaughtDrop<R>,
    end: Option<EndGuard>,
}

impl<R: QueueRx> LateDrain<R> {
    pub(super) fn new(rx: R, owner: Owner, end: EndGuard) -> Self {
        Self {
            rx: CaughtDrop::new(rx, end_remaining, owner),
            end: Some(end),
        }
    }
}

impl<R: QueueRx> Drop for LateDrain<R> {
    fn drop(&mut self) {
        // Runs before the fields drop, so the receiver still holds what the
        // task never took.
        if let Some(end) = self.end.take() {
            let dropped = self.rx.value.as_ref().map_or(0, QueueRx::queued);
            end.cancelled(dropped);
        }
    }
}

/// Drops what still arrives after its queue closed. On a lane: what a sender
/// pushes through a slot it reserved before the close, until every slot is
/// used or given back, or no sender is left. On the mailbox, whose end closed
/// its queue for good: the push that was landing, until none is.
/// It waits on the receiver alone, so it needs no timer and runs on a
/// runtime without a time driver. Every drop is caught and a panic is
/// logged with the actor's id, and whatever the receiver still holds when
/// the task ends, however it ends, is dropped under the same catch.
async fn drain_late<R: QueueRx>(mut late: LateDrain<R>) {
    let owner = late.rx.owner.clone();
    let rx = late.rx.get_mut();
    let drained = catch_callback(async {
        while let Some(message) = std::future::poll_fn(|cx| rx.poll_recv_one(cx)).await {
            if let Some(payload) = end_value(message) {
                log_late_drop(&payload, &owner);
            }
            if rx.free_units() == rx.all_units() {
                break;
            }
        }
    })
    .await;
    let end = late.end.take().expect("the guard is held until the end");
    match drained {
        Ok(()) => end.finished(),
        Err(payload) => {
            let payload = payload_into_string(payload);
            end.panicked(&payload);
        }
    }
}

/// Logs, with the `tracing` feature, a panic raised while dropping a value
/// that arrived through a slot reserved before its queue closed.
pub(crate) fn log_late_drop(payload: &str, owner: &Owner) {
    #[cfg(feature = "tracing")]
    caught_log!(warn, target: "tokio_actors::actor::runtime",
        actor_id = %owner,
        panic = %owner.fields().panic_text(payload),
        "a drop of a value that arrived after its queue closed panicked; the panic is contained"
    );
    #[cfg(not(feature = "tracing"))]
    let _ = (payload, owner);
}
