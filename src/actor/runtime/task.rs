//! The task marker and the abort reason, which tell a task's own unwind from an abort.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;

use super::{aborted_reason, emit_actor_ended};
use crate::actor::incarnation::{StatusTx, StatusValue};
#[cfg(test)]
use crate::actor::runtime::end_hooks;
use crate::actor::task_key::PollScope;
use crate::error::ActorError;
#[cfg(test)]
use crate::types::ActorId;
use crate::types::{Incarnation, StopReason};

/// The run loop's hold on its waker slot: clears the slot when it drops, so
/// a waker registered by the loop never outlives the loop.
pub(super) struct SlotHold(pub(super) crate::actor::incarnation::IncarnationRef);

impl Drop for SlotHold {
    fn drop(&mut self) {
        self.0.reader().clear();
    }
}

/// The reason, `Failure(Panic(UNCAUGHT_PANIC))`, written for a task whose own
/// poll unwound past every catch: every call of user code is caught, so only
/// the crate's own code unwinds there. The task ends in the order of an abort
/// while suspended, with this reason instead of `Kill`: its status plane, the
/// runner or restart attempt that joins it, and an awaited spawn whose
/// initialization the unwind cut (as `SpawnError::Init`) read it. The end is
/// recorded on the status plane as a panic with this message. With the
/// `tracing` feature the supervisor that joins the task also logs the payload
/// in a warning; without it the crate does not log the payload. A process
/// that wants every panic payload in its log installs a hook with
/// `std::panic::set_hook`.
pub(crate) const UNCAUGHT_PANIC: &str = "the actor's task panicked outside every catch";

/// Set while this actor task's own future is being polled, and cleared after
/// each poll, an unwind included ([`MarkedRun`]). One per task: one thread
/// may poll or drop another actor's task inside a callback. A drop of the
/// task's state that writes the reason reads it to tell this task's own
/// unwind, inside its poll, from an abort, a shutdown, or a runtime dropped
/// during an unrelated unwind, which drop the task outside its poll.
///
/// Every access is `Relaxed`. Every clone lives inside the task's one future
/// and none leaves it, so each read is either on the thread that is polling
/// the task, where it sees that thread's own store, or in the future's drop
/// glue outside a poll, which the `Send` contract of the future orders after
/// the last poll's clearing store. No other thread reads the flag, and
/// nothing waits on it, so no stronger order has a reader to serve.
#[derive(Clone, Default)]
pub(crate) struct TaskMarker(Arc<AtomicBool>);

impl TaskMarker {
    /// The reason of a task dropped before its reason was written: a panic
    /// when this task's own poll is unwinding, `Kill` otherwise.
    pub(crate) fn abort_reason(&self) -> StopReason {
        if self.0.load(Ordering::Relaxed) && std::thread::panicking() {
            StopReason::Failure(ActorError::Panic(UNCAUGHT_PANIC.to_string()))
        } else {
            StopReason::Kill
        }
    }
}

/// The actor task's future: sets the task's marker before each poll of
/// `run_actor` and clears it after, through a guard that also clears it on
/// an unwind. On this task's own panic, everything inside the future drops
/// during the unwind of the inner poll, while the marker is still set.
///
/// It also records, for the length of each poll, which incarnation the thread
/// is running and in which task (`PollScope`), so a request the actor sends to
/// itself from its own task is refused (`AskError::CallingSelf`). The record
/// is put back by a guard on every path out of the poll. The future's state
/// drops inside the same record when the future is dropped before it ended (an
/// abort, a runtime shutting down, an unwind), so a drop that calls back into
/// the actor is refused as well.
pub(super) struct MarkedRun<F> {
    marker: TaskMarker,
    /// The incarnation this future runs.
    incarnation: Incarnation,
    /// The id of the task that polls this future, read at its first poll
    /// inside a task and never again: a future is polled by one task for its
    /// whole life.
    task: Option<tokio::task::Id>,
    /// Taken when the future ends, inside the record of its last poll.
    inner: Option<Pin<Box<F>>>,
}

impl<F: Future> MarkedRun<F> {
    pub(super) fn new(marker: TaskMarker, incarnation: Incarnation, inner: F) -> Self {
        Self {
            marker,
            incarnation,
            task: None,
            inner: Some(Box::pin(inner)),
        }
    }
}

impl<F> Drop for MarkedRun<F> {
    fn drop(&mut self) {
        // An ended future has nothing left. One cut short drops its state
        // here, in the record of its incarnation and its task.
        if let Some(inner) = self.inner.take() {
            // A future that was never polled has read no id yet: the drop runs
            // in the task that was to poll it.
            let task = self.task.or_else(tokio::task::try_id);
            let _scope = PollScope::enter(self.incarnation, task);
            drop(inner);
        }
    }
}

/// Clears a task's marker when it drops.
struct ClearMarker<'a>(&'a AtomicBool);

impl Drop for ClearMarker<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl<F: Future> Future for MarkedRun<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        if this.task.is_none() {
            this.task = tokio::task::try_id();
        }
        let _scope = PollScope::enter(this.incarnation, this.task);
        this.marker.0.store(true, Ordering::Relaxed);
        let _clear = ClearMarker(&this.marker.0);
        let inner = this
            .inner
            .as_mut()
            .expect("a future is not polled after it ended");
        let output = std::task::ready!(inner.as_mut().poll(cx));
        // Its state drops here, inside the record and the marker.
        this.inner = None;
        Poll::Ready(output)
    }
}

/// Writes the abort reason into the status plane when it drops and no reason
/// is written: `Kill`, or a panic for this task's own unwind
/// ([`TaskMarker::abort_reason`]). See `run_actor`'s `_abort_reason`.
pub(crate) struct AbortReason {
    status: StatusTx,
    marker: TaskMarker,
    /// The actor, for the hook tests observe the write through. A build that
    /// is not a test build keeps no id here, so a spawn takes no reference
    /// count for it.
    #[cfg(test)]
    owner: ActorId,
}

impl AbortReason {
    #[cfg(test)]
    pub(super) fn new(status: StatusTx, marker: TaskMarker, owner: ActorId) -> Self {
        Self {
            status,
            marker,
            owner,
        }
    }

    #[cfg(not(test))]
    pub(super) fn new(status: StatusTx, marker: TaskMarker) -> Self {
        Self { status, marker }
    }
}

impl Drop for AbortReason {
    fn drop(&mut self) {
        if self.status.reason_written() || self.status.borrow().reason.is_some() {
            return;
        }
        #[cfg(test)]
        end_hooks::run(end_hooks::REASON, &self.owner);
        let reason = aborted_reason(&self.status, &self.marker);
        if crate::telemetry::engaged_for(self.status.abort_record(), None) {
            self.write_reason_noted(reason);
            return;
        }
        StatusValue::write_reason(&self.status, reason, false);
    }
}

impl AbortReason {
    /// The tail of the drop for a system that takes telemetry events or holds
    /// an identity: the reason is copied for the end event.
    #[inline(never)]
    fn write_reason_noted(&self, reason: StopReason) {
        let kept = reason.clone();
        if StatusValue::write_reason(&self.status, reason, false) {
            emit_actor_ended(None, self.status.abort_record(), &kept);
        }
    }
}
