//! Handle-based communication API for actors.
//!
//! An [`ActorHandle`](crate::actor::handle::ActorHandle) exposes two
//! independent observability planes, mirroring Erlang/OTP's split between
//! `sys:get_status/1` and `process_info/2`:
//! - The queue plane ([`get_status`](crate::actor::handle::ActorHandle::get_status))
//!   travels through the actor's system channel: it returns the richer
//!   [`ActorStatusInfo`](crate::types::ActorStatusInfo) snapshot, but hangs if
//!   the actor is stuck and never services that channel.
//! - The runtime plane ([`status`](crate::actor::handle::ActorHandle::status)
//!   and [`wait_stopped`](crate::actor::handle::ActorHandle::wait_stopped))
//!   is a `watch` cell written directly by the runtime: it answers instantly,
//!   even for a hung actor, at the cost of carrying only the bare
//!   [`ActorStatus`](crate::types::ActorStatus).
//!
//! `send` and `get_status` share the same self-call hazard: awaiting either
//! of them from inside the actor's own callback, targeting
//! `ctx.self_handle()`, deadlocks the actor against itself. See the
//! "Deadlock warning" section on each method.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Instant};

use crate::actor::runtime::saturating_deadline;
use crate::actor::{Actor, ActorEnvelope};
use crate::error::{AskError, SendError, TrySendError};
use crate::types::{
    ActorId, ActorStatus, ActorStatusInfo, Envelope, StopLane, StopReason, SystemMessage,
};

/// Cloneable handle that callers use to communicate with an actor.
#[derive(Debug)]
pub struct ActorHandle<A: Actor> {
    id: ActorId,
    tx: mpsc::Sender<ActorEnvelope<A>>,
    system_tx: mpsc::Sender<SystemMessage>,
    stop_lane: StopLane,
    mailbox_capacity: usize,
    status_rx: watch::Receiver<ActorStatus>,
    /// Name of the [`ActorSystem`](crate::system::ActorSystem) this actor is
    /// registered in. Every actor is always registered in exactly one system
    /// (bare spawns join `ActorSystem::default()`), and a system's name is
    /// permanently unique once claimed, so this is a stable identity: two
    /// handles with the same `ActorId` but different systems are genuinely
    /// different actors, and a supervised child's handle stays equal to
    /// itself across a restart (same id, same system) even though the
    /// underlying channels are replaced underneath it.
    system_name: Arc<str>,
}

impl<A: Actor> Clone for ActorHandle<A> {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            tx: self.tx.clone(),
            system_tx: self.system_tx.clone(),
            stop_lane: self.stop_lane.clone(),
            mailbox_capacity: self.mailbox_capacity,
            status_rx: self.status_rx.clone(),
            system_name: self.system_name.clone(),
        }
    }
}

impl<A: Actor> PartialEq for ActorHandle<A> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.system_name == other.system_name
    }
}

impl<A: Actor> Eq for ActorHandle<A> {}

impl<A: Actor> std::hash::Hash for ActorHandle<A> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.system_name.hash(state);
    }
}

impl<A: Actor> ActorHandle<A> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: ActorId,
        tx: mpsc::Sender<ActorEnvelope<A>>,
        system_tx: mpsc::Sender<SystemMessage>,
        stop_lane: StopLane,
        mailbox_capacity: usize,
        status_rx: watch::Receiver<ActorStatus>,
        system_name: Arc<str>,
    ) -> Self {
        Self {
            id,
            tx,
            system_tx,
            stop_lane,
            mailbox_capacity,
            status_rx,
            system_name,
        }
    }

    /// Returns a clone of this actor's stop lane sender (for wiring the
    /// supervision escalation ladder and the system registry's shutdown
    /// sweep without a mailbox round-trip).
    pub(crate) fn stop_lane(&self) -> StopLane {
        self.stop_lane.clone()
    }

    /// Returns a clone of the runtime status receiver (used by the system
    /// registry's shutdown cascade - see the module docs on the two
    /// observability planes).
    pub(crate) fn status_rx(&self) -> watch::Receiver<ActorStatus> {
        self.status_rx.clone()
    }

    /// Returns the unique identifier of the actor.
    pub fn id(&self) -> &ActorId {
        &self.id
    }

    /// Returns the total capacity of the actor's mailbox.
    pub fn mailbox_capacity(&self) -> usize {
        self.mailbox_capacity
    }

    /// Returns the current number of messages in the mailbox.
    pub fn mailbox_len(&self) -> usize {
        self.mailbox_capacity - self.tx.capacity()
    }

    /// Returns the number of available slots in the mailbox.
    pub fn mailbox_available(&self) -> usize {
        self.tx.capacity()
    }

    /// Returns true if the actor is still alive and processing messages.
    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed()
    }

    /// Returns the actor's current lifecycle status.
    ///
    /// This is the runtime observability plane (see the module docs):
    /// it reads a `watch` cell maintained directly by the runtime, so it
    /// answers instantly, even while the actor is hung inside a callback and
    /// not servicing its mailbox or system channel. Contrast with
    /// [`get_status`](Self::get_status), the queue plane, which travels
    /// through the system channel and can hang on a stuck actor.
    ///
    /// The underlying channel is lossy: a short-lived intermediate status
    /// (for example [`Stopping`](ActorStatus::Stopping)) can be missed if it
    /// changes again before this is read. [`Stopped`](ActorStatus::Stopped)
    /// is terminal, so it is never missed this way; see
    /// [`wait_stopped`](Self::wait_stopped).
    pub fn status(&self) -> ActorStatus {
        *self.status_rx.borrow()
    }

    /// Sends a message to the actor without waiting for a response (fire-and-forget).
    ///
    /// This method awaits if the mailbox is full.
    ///
    /// # Errors
    /// Returns `SendError::Closed` if the actor has stopped.
    pub async fn notify(&self, msg: A::Message) -> Result<(), SendError> {
        self.tx
            .send(Envelope::Message {
                payload: msg,
                responder: None,
            })
            .await
            .map_err(|_| SendError::Closed)
    }

    /// Attempts to send a message without blocking.
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

    /// Sends a message and waits for a response (request-response).
    ///
    /// # Deadlock warning
    /// Never `.await` this from inside the actor's own `handle` (or any other
    /// callback) when the target is `ctx.self_handle()`. The actor is a
    /// single task that is already busy running the callback, so it can
    /// never reach its mailbox to answer its own request; the call parks
    /// forever.
    ///
    /// # Errors
    /// - `AskError::Closed` if the actor was already stopped (the message was
    ///   NOT processed).
    /// - `AskError::ResponseDropped` if the actor stopped after accepting the
    ///   message but before replying (unknown whether it was processed).
    /// - `AskError::Actor(err)` if the handler returned an error, or panicked
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor;
    ///   if it is supervised, its supervisor restarts it per policy, so an
    ///   immediate retry may still see `Closed` during the restart window -
    ///   re-look the actor up by name.
    pub async fn send(&self, msg: A::Message) -> Result<A::Response, AskError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Envelope::Message {
                payload: msg,
                responder: Some(tx),
            })
            .await
            .map_err(|_| AskError::Closed)?;

        match rx.await.map_err(|_| AskError::ResponseDropped)? {
            Ok(resp) => Ok(resp),
            Err(err) => Err(AskError::Actor(err)),
        }
    }

    /// Sends a message and waits for a response, bounded by a single deadline
    /// that spans both enqueueing the message and awaiting the reply.
    ///
    /// Mirrors `gen_server:call/3`: one clock for the whole request. The
    /// deadline is armed BEFORE the mailbox send, so a request parked on a
    /// full mailbox still times out instead of waiting indefinitely.
    ///
    /// # Errors
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
    /// - `AskError::ResponseDropped` if the actor stopped after accepting the
    ///   message but before replying (unknown whether it was processed).
    /// - `AskError::Actor(err)` if the handler returned an error, or panicked
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor;
    ///   if it is supervised, its supervisor restarts it per policy, so an
    ///   immediate retry may still see `Closed` during the restart window -
    ///   re-look the actor up by name.
    pub async fn send_timeout(
        &self,
        msg: A::Message,
        timeout: Duration,
    ) -> Result<A::Response, AskError> {
        let deadline = saturating_deadline(Instant::now(), timeout);
        let (tx, rx) = oneshot::channel();

        self.tx
            .send_timeout(
                Envelope::Message {
                    payload: msg,
                    responder: Some(tx),
                },
                deadline.saturating_duration_since(Instant::now()),
            )
            .await
            .map_err(|err| match err {
                mpsc::error::SendTimeoutError::Timeout(_) => AskError::Timeout { enqueued: false },
                mpsc::error::SendTimeoutError::Closed(_) => AskError::Closed,
            })?;

        let reply = time::timeout(deadline.saturating_duration_since(Instant::now()), rx)
            .await
            .map_err(|_| AskError::Timeout { enqueued: true })?
            .map_err(|_| AskError::ResponseDropped)?;

        match reply {
            Ok(resp) => Ok(resp),
            Err(err) => Err(AskError::Actor(err)),
        }
    }

    /// Signals the actor to stop.
    ///
    /// Delivered through the stop lane: a synchronous, infallible signal the
    /// run loop observes ahead of everything else - the system channel and
    /// the user mailbox alike - so it is never delayed by a full mailbox or a
    /// backlog of supervision traffic. Like every signal on the lane, it is
    /// only ever observed at a turn boundary: an in-flight `handle` invocation
    /// always runs to completion first (the Isolated Turn Principle), so this
    /// never races or cancels one.
    ///
    /// Escalation only ever increases: calling this with a lower-severity
    /// `reason` (for example `Graceful` after a `Kill` already landed) has no
    /// effect, and calling it again with the SAME severity - most commonly a
    /// retry after [`pre_stop`](crate::actor::Actor::pre_stop) vetoed the
    /// first request - always lands and is guaranteed a fresh
    /// [`pre_stop`](crate::actor::Actor::pre_stop) invocation, even though the
    /// requested reason did not change tier. Calling this several times in a
    /// row before the actor gets back to its `select!` coalesces into a
    /// single observation of the highest severity requested: the actor sees
    /// at most one turn-boundary check per turn, never one per call.
    ///
    /// # Errors
    /// Returns `SendError::Closed` if the actor has already finished its
    /// message loop - the same case, and the same error, a full mailbox send
    /// would have failed with before the stop lane existed.
    pub async fn stop(&self, reason: StopReason) -> Result<(), SendError> {
        if self.stop_lane.is_closed() {
            return Err(SendError::Closed);
        }
        self.stop_lane.raise(reason);
        Ok(())
    }

    /// Requests a status snapshot from the actor.
    ///
    /// This bypasses the mailbox and uses the system channel, so it
    /// responds even when the mailbox is full.
    ///
    /// # Deadlock warning
    /// The same self-call hazard as [`send`](Self::send) applies: calling
    /// this from inside the actor's own callback on `ctx.self_handle()` parks
    /// forever, because the actor cannot service its system channel while it
    /// is busy running that very callback. For a hung actor's status without
    /// this risk, use the instant, always-answering [`status`](Self::status).
    pub async fn get_status(&self) -> Result<ActorStatusInfo, AskError> {
        let (tx, rx) = oneshot::channel();
        self.system_tx
            .send(SystemMessage::GetStatus(tx))
            .await
            .map_err(|_| AskError::Closed)?;
        rx.await.map_err(|_| AskError::ResponseDropped)
    }

    /// Waits until the actor reaches its terminal [`ActorStatus::Stopped`]
    /// state.
    ///
    /// Resolves for every way an actor can die: a graceful stop, a kill, and
    /// even a task abort. An abort drops the status sender without a final
    /// write (nobody is left to report `Stopped`), so an actor-side sender
    /// drop is also treated as terminal here: the wait still returns instead
    /// of hanging forever.
    ///
    /// Uses the same runtime status plane as [`status`](Self::status), so it
    /// resolves even for an actor hung inside a callback.
    pub async fn wait_stopped(&self) {
        let mut rx = self.status_rx.clone();
        while *rx.borrow() != ActorStatus::Stopped {
            if rx.changed().await.is_err() {
                // The status sender was dropped (the task was aborted)
                // without ever writing Stopped. There is nobody left to
                // report a final status, so this is terminal too.
                return;
            }
        }
    }
}
