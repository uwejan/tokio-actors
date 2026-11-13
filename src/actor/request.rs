//! Requests.
//!
//! An actor that calls [`request`](crate::actor::context::ActorContext::request)
//! gets the callee's answer as one of its own messages. This module holds the
//! pieces behind it, which are the pieces of a monitor:
//!
//! - the state cell every request has (the monitor's cell, from the same id
//!   counter), which makes one reply and one cancel agree on who wins;
//! - the entry the asker keeps for each request, and the task that sends the
//!   request, waits for the callee's answer and puts the reply in the asker's
//!   mailbox;
//! - the count that caps the requests in flight.
//!
//! A reply is the answer to the asker's own question, not a notice the asker
//! is owed: one pending when the asker stops is dropped, so the stop-time
//! stage never delivers a request's reply.

use std::collections::{BTreeMap, HashSet};
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::actor::context::Released;
use crate::actor::handle::ActorHandle;
use crate::actor::monitor::{put_notice, MonitorShared};
use crate::actor::panic::{catch_sync, payload_into_string};
use crate::actor::runtime::{end_value, log_late_drop};
use crate::actor::Actor;
use crate::error::ActorError;
use crate::types::{MonitorId, RequestId, RequestReply, SignalTag, StopReason};

/// One request in its asker's table.
pub(crate) struct RequestEntry {
    shared: Arc<MonitorShared>,
    token: CancellationToken,
}

impl Drop for RequestEntry {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

/// The requests an actor has in flight, and the cap on how many. Empty until
/// the first request: an actor that never asks holds one null pointer.
pub(crate) struct RequestTable<A: Actor> {
    state: Option<Box<RequestState>>,
    _actor: PhantomData<fn() -> A>,
}

/// The requests, allocated with the first one.
#[derive(Default)]
struct RequestState {
    entries: BTreeMap<MonitorId, RequestEntry>,
    /// The cap `set_request_cap` stored; unset, the cap is the asker's
    /// mailbox capacity.
    cap: Option<usize>,
}

impl<A: Actor> Default for RequestTable<A> {
    fn default() -> Self {
        Self {
            state: None,
            _actor: PhantomData,
        }
    }
}

impl<A: Actor> RequestTable<A> {
    /// The requests held and not yet answered or cancelled.
    pub(crate) fn len(&self) -> usize {
        self.state.as_ref().map_or(0, |state| state.entries.len())
    }

    pub(crate) fn contains(&self, id: MonitorId) -> bool {
        self.state
            .as_ref()
            .is_some_and(|state| state.entries.contains_key(&id))
    }

    /// The cap `set_request_cap` stored, if it was called.
    pub(crate) fn cap(&self) -> Option<usize> {
        self.state.as_ref().and_then(|state| state.cap)
    }

    /// Stores the cap: 0 is read as 1.
    pub(crate) fn set_cap(&mut self, cap: usize) {
        self.state.get_or_insert_with(Default::default).cap = Some(cap.max(1));
    }

    /// Whether another request fits under `default_cap`, the cap used while
    /// none was stored.
    pub(crate) fn has_room(&self, default_cap: usize) -> bool {
        self.len() < self.cap().unwrap_or(default_cap)
    }

    pub(crate) fn insert(
        &mut self,
        id: MonitorId,
        shared: Arc<MonitorShared>,
        token: CancellationToken,
    ) {
        self.state
            .get_or_insert_with(Default::default)
            .entries
            .insert(id, RequestEntry { shared, token });
    }

    /// Removes a request; dropping the entry ends its task.
    pub(crate) fn remove(&mut self, id: MonitorId) {
        if let Some(state) = self.state.as_mut() {
            state.entries.remove(&id);
        }
    }

    /// Whether the stage delivers a queued reply of this request. Never: a
    /// reply is dropped at its asker's stop, so the stage keeps it with the
    /// messages it stashes.
    pub(crate) fn wants(&self, _tag: &SignalTag) -> bool {
        false
    }

    /// Whether the tag names a request this table holds.
    pub(crate) fn owns(&self, tag: &SignalTag) -> bool {
        self.contains(tag.shared.id())
    }

    /// The reply of the tag's request was dispatched: the request leaves the
    /// count.
    pub(crate) fn finish(&mut self, tag: &SignalTag) {
        if self.owns(tag) {
            self.remove(tag.shared.id());
        }
    }

    /// Whether a task took its reply and the stage has not found it in a
    /// queue yet.
    pub(crate) fn in_flight(&self, seen: &HashSet<MonitorId>) -> bool {
        self.state.as_ref().is_some_and(|state| {
            state
                .entries
                .iter()
                .any(|(id, entry)| entry.shared.is_sent() && !seen.contains(id))
        })
    }

    /// The stage closes every request nobody has taken, so no task queues a
    /// reply from here on.
    pub(crate) fn close_all(&mut self) {
        if let Some(state) = self.state.as_ref() {
            for entry in state.entries.values() {
                entry.shared.claim_for_stage();
            }
        }
    }

    /// Removes every request.
    pub(crate) fn clear(&mut self) {
        self.state = None;
    }

    /// Cancels a request: the task ends, and a reply already queued is
    /// marked so that it is dropped, not delivered, when it is dispatched.
    /// `None` when the table holds no such request. `Some(true)`: no reply
    /// had been queued and none will be. `Some(false)`: one had been
    /// queued, and it is dropped.
    pub(crate) fn cancel(&mut self, id: MonitorId) -> Option<bool> {
        let entry = self.state.as_mut()?.entries.remove(&id)?;
        Some(entry.shared.flush())
    }
}

/// Gives the user's map function back inside a catch when the task ends, by
/// whichever path: the function is the user's, so its drop can panic.
pub(crate) struct HeldMap<F> {
    map: Option<F>,
    owner: crate::actor::runtime::Owner,
}

impl<F> HeldMap<F> {
    pub(crate) fn new(map: F, owner: crate::actor::runtime::Owner) -> Self {
        Self {
            map: Some(map),
            owner,
        }
    }

    fn map(&self) -> &F {
        self.map.as_ref().expect("held until the guard drops")
    }
}

impl<F> Drop for HeldMap<F> {
    fn drop(&mut self) {
        if let Some(map) = self.map.take() {
            if let Some(panic) = end_value(map) {
                log_late_drop(&panic, &self.owner);
            }
        }
    }
}

/// What a request's task needs.
pub(crate) struct RequestTask<A: Actor, B: Actor, F> {
    /// The asker's own handle: its mailbox takes the reply.
    pub(crate) asker: ActorHandle<A>,
    /// The callee, held in the sender role.
    pub(crate) callee: ActorHandle<B>,
    pub(crate) msg: B::Message,
    /// The deadline of `request_within`, if any.
    pub(crate) within: Option<Duration>,
    pub(crate) shared: Arc<MonitorShared>,
    pub(crate) token: CancellationToken,
    pub(crate) map: HeldMap<F>,
}

/// The reply of a request whose result is dropped and not delivered: the
/// result holds the callee's response, which is the user's value.
fn dropped<T>(result: T) -> Released {
    Released {
        run: end_value(result),
        end: None,
    }
}

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
            caught_log!(warn,
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
