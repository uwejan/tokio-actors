//! Delivery of the system monitor's reports to its actor.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{MonitorGuard, MonitorInbox, Report};
use crate::actor::handle::ActorHandle;
use crate::actor::panic::{catch_sync, payload_into_string};
use crate::actor::Actor;
use crate::telemetry::event::TelemetryEvent;
use crate::telemetry::subscription::{Delivery, SubscriptionNotice};

/// The helper that delivers the monitor's reports: takes each report the
/// sweep offered out of the inbox, maps it into a message of the monitor and
/// offers it to the monitor's mailbox without waiting, and answers the sweep
/// whether it went in. A message the full mailbox refuses is counted, and the
/// count reaches the monitor as an [`Overflow`](SubscriptionNotice::Overflow)
/// notice once the inbox runs empty; the sweep offers the refused report's
/// state again at its next look while the state lasts, so a refusal is never
/// silent and a state the monitor was told is never told twice.
/// The helper ends when the monitor's status plane reads a final reason, when
/// it is told to end, or when the map panics (after one more call with a
/// notice that it did).
pub(crate) async fn forward<A, F>(
    _guard: MonitorGuard,
    mut rx: mpsc::Receiver<Report>,
    inbox: Arc<MonitorInbox>,
    mut map: F,
    handle: ActorHandle<A>,
    cancel: CancellationToken,
) where
    A: Actor,
    F: FnMut(Delivery) -> A::Message + Send + 'static,
{
    let mut status = handle.status_rx();
    loop {
        let report = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            _ = crate::system::wait_final_reason(&mut status) => return,
            report = rx.recv() => report,
        };
        let Some(Report { event, tag }) = report else {
            return;
        };
        if let Some(event) = event {
            let delivery = Delivery {
                event,
                missed_before: 0,
            };
            let Some(landed) = deliver(&mut map, delivery, &handle) else {
                return;
            };
            if !landed {
                inbox.missed.fetch_add(1, Ordering::AcqRel);
            }
            inbox.answer(tag, landed);
        }
        if rx.is_empty() {
            let missed = inbox.missed.swap(0, Ordering::AcqRel);
            if missed > 0 {
                let notice = Delivery {
                    event: Arc::new(TelemetryEvent::Notice(SubscriptionNotice::Overflow {
                        missed,
                    })),
                    missed_before: 0,
                };
                match deliver(&mut map, notice, &handle) {
                    None => return,
                    // The count waits for the next notice, which the sweep
                    // prompts at its next look.
                    Some(false) => {
                        inbox.missed.fetch_add(missed, Ordering::AcqRel);
                    }
                    Some(true) => {}
                }
            }
        }
    }
}

/// Maps `delivery` and offers the message to the monitor: `Some(true)` when
/// it went in, `Some(false)` when the mailbox refused it. A map that panics
/// is called once more with the notice that it did, and ends the helper
/// (`None`). A message the mailbox refuses is dropped inside a catch.
fn deliver<A, F>(map: &mut F, delivery: Delivery, handle: &ActorHandle<A>) -> Option<bool>
where
    A: Actor,
    F: FnMut(Delivery) -> A::Message + Send + 'static,
{
    match catch_sync(|| map(delivery)) {
        Ok(msg) => Some(offer(handle, msg)),
        Err(payload) => {
            let _ = payload_into_string(payload);
            #[cfg(feature = "tracing")]
            caught_log!(warn, target: "tokio_actors::telemetry::monitor",
                actor_id = %handle.id(),
                "the map of the system monitor panicked; the monitor ends"
            );
            let notice = Delivery {
                event: Arc::new(TelemetryEvent::Notice(SubscriptionNotice::MapPanicked)),
                missed_before: 0,
            };
            // A second panic ends the monitor without a notice.
            if let Ok(msg) = catch_sync(|| map(notice)) {
                offer(handle, msg);
            }
            None
        }
    }
}

/// Offers `msg` to the monitor's mailbox; `true` when it went in. The
/// message and the refusal are dropped inside a catch, since the message is
/// the user's.
fn offer<A: Actor>(handle: &ActorHandle<A>, msg: A::Message) -> bool {
    match catch_sync(|| handle.try_notify(msg).is_ok()) {
        Ok(landed) => landed,
        Err(payload) => {
            let _ = payload_into_string(payload);
            false
        }
    }
}
