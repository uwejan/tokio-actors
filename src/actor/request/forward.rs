//! The task that carries one request and brings back its reply.

use super::{dropped, RequestTask};
use crate::actor::context::Released;
use crate::actor::monitor::put_notice;
use crate::actor::panic::{catch_sync, payload_into_string};
use crate::actor::Actor;
use crate::error::ActorError;
use crate::types::{RequestId, RequestReply, StopReason};

/// The request's task: sends the request to the callee, waits for its
/// answer, then puts the reply in the asker's mailbox, once.
///
/// The reply enters through an ordinary mailbox slot, after every message the
/// callee sent the asker before it answered, because the answer is read after
/// those messages landed. A full mailbox makes the task wait for room; the wait
/// ends with a cancel, the asker's mailbox, or the stage.
pub(crate) async fn request_forward<A, B, F>(task: RequestTask<A, B, F>) -> Released
where
    A: Actor,
    B: Actor,
    F: Fn(RequestReply<B::Response>) -> A::Message + Send + Sync + 'static,
{
    let RequestTask {
        asker,
        callee,
        msg,
        within,
        shared,
        token,
        map,
    } = task;
    let id = RequestId::from_monitor(shared.id());
    // The send awaits inside the task, never inside the asker's turn. The
    // task is not the asker's task, so a request to oneself is no self call.
    let result = tokio::select! {
        biased;
        _ = token.cancelled() => return Released::default(),
        () = asker.mailbox_close() => return Released::default(),
        result = async {
            match within {
                Some(within) => callee.send_timeout(msg, within).await,
                None => callee.send(msg).await,
            }
        } => result,
    };
    // The reply waits for a slot in the asker's mailbox. A cancel or the
    // stage ends the wait; nothing awaits between the slot and the send.
    let permit = tokio::select! {
        biased;
        _ = token.cancelled() => return dropped(result),
        permit = asker.reserve_signal(false) => permit,
    };
    let Some(permit) = permit else {
        return dropped(result);
    };
    // The map function runs before the cell is taken, so the time between the
    // take and the send is a few instructions: the stage, which waits for a
    // taken reply to land, waits for no user code. A cancel that wins
    // meanwhile drops the message the map made.
    if !shared.is_pending() {
        drop(permit);
        return dropped(result);
    }
    let made = catch_sync(|| (map.map())(RequestReply { id, result })).map_err(payload_into_string);
    match made {
        Ok(message) => {
            if shared.take() {
                put_notice(permit, &shared, message);
                Released::default()
            } else {
                drop(permit);
                dropped(message)
            }
        }
        Err(panic) => {
            drop(permit);
            if !shared.take() {
                // Cancelled while the map ran: nothing was owed, and the
                // asker is not failed for a request it has already dropped.
                return Released::default();
            }
            // Kept before the raise: a panic that lands after the asker's
            // message loop read its stop lane for the last time raises where
            // no reader is left, and the stop reads the panic from the
            // incarnation's record.
            asker
                .stop_lane_ref()
                .abort_record()
                .keep_map_panic(panic.clone());
            shared.spend();
            #[cfg(feature = "tracing")]
            caught_log!(warn, target: "tokio_actors::actor::request",
                actor_id = %asker.id(),
                panic = %asker.log_fields().panic_text(&panic),
                "a request's map function panicked; the actor fails with that panic"
            );
            let _ = crate::system::raise_noted(
                None,
                asker.stop_lane_ref(),
                StopReason::Failure(ActorError::Panic(panic.clone())),
                crate::telemetry::StopOrigin::MapPanic,
            );
            Released {
                run: Some(panic),
                end: None,
            }
        }
    }
}
