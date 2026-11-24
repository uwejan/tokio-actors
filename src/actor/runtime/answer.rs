//! The answer of a failed request, held until the stop lane is sealed.

use tokio::sync::oneshot;

use super::end_value;
use crate::actor::incarnation::IncarnationRef;
use crate::actor::panic::{contain_waker_panic, send_contained, AnswerHold};
use crate::error::ActorError;
use crate::types::{ActorId, StopReason};

/// The answer to the one request whose handler failed (an `Err` or a panic),
/// or whose handler stopped its own actor and replied, held by the run loop
/// to the seal of its stop lane.
///
/// A caller reads a crash the way an Erlang/OTP `gen_server:call` caller does,
/// through its monitor, after the process is dead: the answer goes out when
/// the actor's reason is final, with its name free and its lane sealed, so a
/// caller that looks the actor up by name on reading it finds no registration
/// or the next incarnation, never this one. Until then the request is
/// unanswered, like a request that is still queued. A `Kill` that ends the
/// actor first, and a task that ends without reaching the seal (an abort, the
/// end of its runtime, its own unwind), drop the sender unanswered, and the
/// caller reads the final reason from the status plane. The sender is held in
/// an [`AnswerHold`], so the drop that wakes the caller never panics into an
/// unwind of the task, and the answer's value is dropped inside a catch on
/// every path, the task's end before the seal included, a panic of that drop
/// logged with the actor's id. The slot is boxed, so an actor that never fails
/// a request holds one pointer.
pub(super) struct HeldAnswer<R> {
    slot: Option<Box<HeldReply<R>>>,
}

/// A held request's reply channel and the answer its caller is answered with:
/// the handler's error, or the reply of a handler that stopped its actor; with
/// the actor's id and record, which name the actor in the log line of a panic
/// from dropping the answer.
struct HeldReply<R> {
    sender: AnswerHold<Result<R, ActorError>>,
    answer: Result<R, ActorError>,
    owner: ActorId,
    record: IncarnationRef,
}

impl<R> HeldAnswer<R> {
    pub(super) const fn new() -> Self {
        Self { slot: None }
    }

    /// Keeps the answer for the seal. A failed request ends the loop at that
    /// request, and a self-stop is read by the loop before the next message
    /// is taken, so one answer is held at a time; an earlier one is sent
    /// first, so no caller is left unanswered. Returns the payload of a panic
    /// from dropping that earlier answer, which the caller logs: the answer
    /// that is held now decides the turn.
    pub(super) fn hold(
        &mut self,
        tx: oneshot::Sender<Result<R, ActorError>>,
        answer: Result<R, ActorError>,
        owner: &ActorId,
        record: &IncarnationRef,
    ) -> Option<String> {
        let discarded = self.release_now();
        self.slot = Some(Box::new(HeldReply {
            sender: AnswerHold::new(Some(tx)),
            answer,
            owner: owner.clone(),
            record: record.clone(),
        }));
        discarded
    }

    /// Sends the held answer at once, whatever the actor's end: the stop that
    /// held it was vetoed, and the actor keeps running. Returns the payload of
    /// a panic from dropping an answer nobody received.
    pub(super) fn release_now(&mut self) -> Option<String> {
        let HeldReply {
            mut sender, answer, ..
        } = *self.slot.take()?;
        let Some(tx) = sender.take() else {
            return end_value(answer);
        };
        send_contained(tx, answer).err().and_then(end_value)
    }

    /// Sends the held answer unless a `Kill` ended the actor: its caller then
    /// reads the final reason, `Kill`, from the status plane. Returns the
    /// payload of a panic from dropping an answer nobody received.
    pub(super) fn deliver(&mut self, final_reason: &StopReason) -> Option<String> {
        if !matches!(final_reason, StopReason::Kill) {
            return self.release_now();
        }
        let HeldReply {
            mut sender, answer, ..
        } = *self.slot.take()?;
        if let Some(tx) = sender.take() {
            // Dropping the sender wakes the caller, who then reads `Kill`; a
            // panic from that caller's waker stays out of this task.
            contain_waker_panic("reply", None, move || drop(tx));
        }
        end_value(answer)
    }
}

/// A task that ends before the seal (an abort, the end of its runtime, its
/// own unwind) drops the held answer here: the sender unanswered, inside the
/// catch of its [`AnswerHold`], and the value inside a catch, so a panic of
/// its drop neither leaves the task's drop nor adds a second panic to an
/// unwind.
impl<R> Drop for HeldAnswer<R> {
    fn drop(&mut self) {
        let Some(held) = self.slot.take() else {
            return;
        };
        let HeldReply {
            sender,
            answer,
            owner,
            record,
        } = *held;
        drop(sender);
        log_unsealed_answer(end_value(answer), &owner, &record);
    }
}

/// Logs, with the `tracing` feature, a panic from dropping a held answer at a
/// task end before the seal.
fn log_unsealed_answer(panic: Option<String>, owner: &ActorId, record: &IncarnationRef) {
    #[cfg(feature = "tracing")]
    if let Some(panic) = &panic {
        caught_log!(warn, target: "tokio_actors::actor::runtime",
            actor_id = %owner,
            panic = %crate::telemetry::log_fields(record, None).panic_text(panic),
            "dropping a held answer at the end of the actor's task panicked; the panic is contained"
        );
    }
    #[cfg(not(feature = "tracing"))]
    let _ = (panic, owner, record);
}
