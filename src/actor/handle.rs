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
//!   [`ActorStatus`](crate::types::ActorStatus) and, once the actor has
//!   ended, the [`StopReason`] it ended with.
//!
//! `send` and `get_status` share the same self-call hazard: awaiting either
//! of them from inside the actor's own callback, targeting
//! `ctx.self_handle()`, deadlocks the actor against itself. See the
//! "Deadlock warning" section on each method.

use std::sync::atomic::AtomicBool;
#[cfg(feature = "tracing")]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::AbortHandle;
use tokio::time::{self, Instant};

use crate::actor::runtime::saturating_deadline;
use crate::actor::supervision::{ABORT_SETTLE, DEFAULT_KILL_GRACE};
use crate::actor::{Actor, ActorEnvelope};
use crate::error::{AskError, SendError, TrySendError};
use crate::system::{
    read_status, terminal_outcome, wait_final_reason, wait_reason, wait_terminal, ActorSystem,
};
use crate::types::{
    ActorId, ActorStatus, ActorStatusInfo, Envelope, RaiseResult, StatusValue, StopLane,
    StopOutcome, StopReason, SystemMessage,
};

/// Cloneable handle that callers use to communicate with an actor.
#[derive(Debug)]
pub struct ActorHandle<A: Actor> {
    id: ActorId,
    tx: mpsc::Sender<ActorEnvelope<A>>,
    /// Sender for the actor's priority lane. `None` on every actor spawned
    /// without one ([`MailboxConfig::with_priority_lane`](crate::actor::runtime::MailboxConfig::with_priority_lane)
    /// or the equivalent [`ActorConfig`](crate::actor::runtime::ActorConfig)
    /// builder), in which case the four `*_priority` methods below degrade
    /// to their normal-lane siblings.
    priority_tx: Option<mpsc::Sender<ActorEnvelope<A>>>,
    system_tx: mpsc::Sender<SystemMessage>,
    stop_lane: StopLane,
    mailbox_capacity: usize,
    status_rx: watch::Receiver<StatusValue>,
    /// Name of the [`ActorSystem`](crate::system::ActorSystem) this actor is
    /// registered in. Every actor is always registered in exactly one system
    /// (bare spawns join `ActorSystem::default()`), and a system's name is
    /// permanently unique once claimed, so this is a stable identity: two
    /// handles with the same `ActorId` but different systems are genuinely
    /// different actors, and a supervised child's handle stays equal to
    /// itself across a restart (same id, same system) even though the
    /// underlying channels are replaced underneath it.
    system_name: Arc<str>,
    /// Set once a priority send has already had to wait for capacity, so the
    /// "lane full" warning fires once per full spell instead of once per
    /// blocked send. Read and written only under the `tracing` feature.
    #[cfg_attr(not(feature = "tracing"), allow(dead_code))]
    priority_full_warned: Arc<AtomicBool>,
}

impl<A: Actor> Clone for ActorHandle<A> {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            tx: self.tx.clone(),
            priority_tx: self.priority_tx.clone(),
            system_tx: self.system_tx.clone(),
            stop_lane: self.stop_lane.clone(),
            mailbox_capacity: self.mailbox_capacity,
            status_rx: self.status_rx.clone(),
            system_name: self.system_name.clone(),
            priority_full_warned: self.priority_full_warned.clone(),
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
        status_rx: watch::Receiver<StatusValue>,
        system_name: Arc<str>,
        priority_tx: Option<mpsc::Sender<ActorEnvelope<A>>>,
        priority_full_warned: Arc<AtomicBool>,
    ) -> Self {
        Self {
            id,
            tx,
            priority_tx,
            system_tx,
            stop_lane,
            mailbox_capacity,
            status_rx,
            system_name,
            priority_full_warned,
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
    pub(crate) fn status_rx(&self) -> watch::Receiver<StatusValue> {
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

    /// Returns the current number of messages queued in the priority lane,
    /// or `0` if this actor has no priority lane. Unaffected by traffic on
    /// the normal mailbox, and vice versa: see
    /// [`mailbox_len`](Self::mailbox_len) for that counter.
    pub fn priority_len(&self) -> usize {
        self.priority_tx
            .as_ref()
            .map_or(0, |tx| tx.max_capacity() - tx.capacity())
    }

    /// Returns true if the actor is still alive and processing messages.
    ///
    /// Once the actor has left its message loop for good (its `on_stopped` hook
    /// has returned, or a `Kill` ended the loop, which skips that hook), this
    /// reports `false` immediately, throughout the teardown of its children,
    /// not only once its task has fully exited.
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
    /// task has fully exited.
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
    /// The lane's length and capacity are observable here
    /// ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure by
    /// awaiting here where Erlang's single queue is unbounded.
    ///
    /// # Errors
    /// Returns `SendError::Closed` if the actor has stopped.
    pub async fn notify_priority(&self, msg: A::Message) -> Result<(), SendError> {
        let Some(tx) = self.priority_tx.as_ref() else {
            return self.notify(msg).await;
        };
        #[cfg(feature = "tracing")]
        {
            match tx.try_send(Envelope::Message {
                payload: msg,
                responder: None,
            }) {
                Ok(()) => {
                    self.priority_full_warned.store(false, Ordering::Relaxed);
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(SendError::Closed),
                Err(mpsc::error::TrySendError::Full(env)) => {
                    if !self.priority_full_warned.swap(true, Ordering::Relaxed) {
                        caught_log!(warn,
                            actor_id = %self.id,
                            "priority lane full: a priority send is waiting for capacity"
                        );
                    }
                    tx.send(env).await.map_err(|_| SendError::Closed)
                }
            }
        }
        #[cfg(not(feature = "tracing"))]
        {
            tx.send(Envelope::Message {
                payload: msg,
                responder: None,
            })
            .await
            .map_err(|_| SendError::Closed)
        }
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

    /// The send of a timer or stream forwarder, waiting for room. On a closed
    /// mailbox the message comes back instead of being dropped here, so the
    /// forwarder releases it itself and reports that drop as made after the
    /// close.
    pub(crate) async fn forward(&self, msg: A::Message) -> Result<(), A::Message> {
        self.tx
            .send(Envelope::Message {
                payload: msg,
                responder: None,
            })
            .await
            .map_err(|err| {
                let Envelope::Message { payload, .. } = err.0;
                payload
            })
    }

    /// Reserves a slot in the mailbox for a message one of the actor's timers
    /// sends, waiting while the mailbox is full; `None` once the mailbox is
    /// closed. Nothing awaits between the reserve and the permit's send or
    /// drop, so a timer can decide at that instant whether its message is
    /// sent.
    pub(crate) async fn reserve_forward(&self) -> Option<ForwardPermit<'_, A>> {
        self.tx.reserve().await.ok().map(ForwardPermit)
    }

    /// [`forward`](Self::forward) without waiting: a full or closed mailbox
    /// gives the message back.
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
                mpsc::error::TrySendError::Full(Envelope::Message { payload, .. }) => {
                    mpsc::error::TrySendError::Full(payload)
                }
                mpsc::error::TrySendError::Closed(Envelope::Message { payload, .. }) => {
                    mpsc::error::TrySendError::Closed(payload)
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
    /// The lane's length and capacity are observable here
    /// ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure
    /// (`Full`, below) where Erlang's single queue is unbounded.
    ///
    /// # Errors
    /// - `TrySendError::Full` if the priority lane is full.
    /// - `TrySendError::Closed` if the actor has stopped.
    pub fn try_notify_priority(&self, msg: A::Message) -> Result<(), TrySendError> {
        let Some(tx) = self.priority_tx.as_ref() else {
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

        match rx.await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(err)) => Err(AskError::Actor(err)),
            Err(_) => Err(self.response_dropped().await),
        }
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
    /// The lane's length and capacity are observable here
    /// ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure by
    /// awaiting here where Erlang's single queue is unbounded.
    ///
    /// # Deadlock warning
    /// Never `.await` this from inside the actor's own `handle` (or any other
    /// callback) when the target is `ctx.self_handle()`. The actor is a
    /// single task that is already busy running the callback, so it can
    /// never reach its priority lane to answer its own request; the call
    /// parks forever.
    ///
    /// # Errors
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
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor;
    ///   if it is supervised, its supervisor restarts it per policy, so an
    ///   immediate retry may still see `Closed` during the restart window -
    ///   re-look the actor up by name.
    pub async fn send_priority(&self, msg: A::Message) -> Result<A::Response, AskError> {
        let Some(tx) = self.priority_tx.as_ref() else {
            return self.send(msg).await;
        };
        let (resp_tx, resp_rx) = oneshot::channel();

        #[cfg(feature = "tracing")]
        {
            match tx.try_send(Envelope::Message {
                payload: msg,
                responder: Some(resp_tx),
            }) {
                Ok(()) => {
                    self.priority_full_warned.store(false, Ordering::Relaxed);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(AskError::Closed),
                Err(mpsc::error::TrySendError::Full(env)) => {
                    if !self.priority_full_warned.swap(true, Ordering::Relaxed) {
                        caught_log!(warn,
                            actor_id = %self.id,
                            "priority lane full: a priority send is waiting for capacity"
                        );
                    }
                    tx.send(env).await.map_err(|_| AskError::Closed)?;
                }
            }
        }
        #[cfg(not(feature = "tracing"))]
        {
            tx.send(Envelope::Message {
                payload: msg,
                responder: Some(resp_tx),
            })
            .await
            .map_err(|_| AskError::Closed)?;
        }

        match resp_rx.await {
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
    /// - `AskError::ResponseDropped { reason }` if the actor stopped after
    ///   accepting the message but before replying (unknown whether it was
    ///   processed); `reason` is why the actor stopped. The error is
    ///   returned once that reason is final: the actor makes it final when it
    ///   seals its stop lane, after its children's teardown and its
    ///   `on_stopped`, and before its queued messages, its value and its
    ///   context are dropped. That wait counts against this call's own
    ///   deadline.
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
    /// The lane's length and capacity are observable here
    /// ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure
    /// (a timeout, below) where Erlang's single queue is unbounded.
    ///
    /// # Errors
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
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor;
    ///   if it is supervised, its supervisor restarts it per policy, so an
    ///   immediate retry may still see `Closed` during the restart window -
    ///   re-look the actor up by name.
    pub async fn send_priority_timeout(
        &self,
        msg: A::Message,
        timeout: Duration,
    ) -> Result<A::Response, AskError> {
        let Some(tx) = self.priority_tx.as_ref() else {
            return self.send_timeout(msg, timeout).await;
        };
        let deadline = saturating_deadline(Instant::now(), timeout);
        let (resp_tx, resp_rx) = oneshot::channel();

        tx.send_timeout(
            Envelope::Message {
                payload: msg,
                responder: Some(resp_tx),
            },
            deadline.saturating_duration_since(Instant::now()),
        )
        .await
        .map_err(|err| match err {
            mpsc::error::SendTimeoutError::Timeout(_) => AskError::Timeout { enqueued: false },
            mpsc::error::SendTimeoutError::Closed(_) => AskError::Closed,
        })?;

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
    /// Delivered through the stop lane: a synchronous, infallible signal the
    /// run loop observes ahead of everything else - the system channel and the
    /// user mailbox alike - so it is never delayed by a full mailbox or a
    /// backlog of supervision traffic. Like every signal on the lane, it is
    /// only ever observed at a turn boundary: the signal itself never
    /// interrupts a callback already in progress (the Isolated Turn Principle);
    /// with [`StopReason::Kill`], the abort described below cuts a callback
    /// still awaiting once the actor's kill grace has passed.
    ///
    /// Escalation only ever increases: calling this with a lower-severity
    /// `reason` (for example `Graceful` after a `Kill` already landed) has no
    /// effect, and calling it again with the SAME severity - most commonly a
    /// retry after [`pre_stop`](crate::actor::Actor::pre_stop) vetoed the
    /// first request - always lands and is guaranteed a fresh
    /// [`pre_stop`](crate::actor::Actor::pre_stop) invocation, even though the
    /// requested reason did not change tier. Within one severity the pending
    /// request keeps the higher rank: a parent's request is kept over a later
    /// graceful one, and a pending failure over a later cancellation. Calling
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
    /// # Errors
    /// Returns `SendError::Closed` if the actor does not accept the
    /// request any more: its task has ended (an aborted one included), it has
    /// sealed its stop lane, or it has finished its message loop and the
    /// request is not [`StopReason::Kill`]. Through this handle a `Kill` is
    /// accepted until the actor seals its stop lane, just before its queued
    /// messages and its value are dropped; after that it answers
    /// `SendError::Closed`. [`ActorSystem::stop`](crate::system::ActorSystem::stop)
    /// and [`ActorSystem::kill`](crate::system::ActorSystem::kill) answer
    /// `NotFound` once the actor has left the registry.
    pub async fn stop(&self, reason: StopReason) -> Result<(), SendError> {
        let is_kill = matches!(reason, StopReason::Kill);
        if self.stop_lane.try_raise(reason) == RaiseResult::Refused {
            return Err(SendError::Closed);
        }
        if is_kill {
            self.arm_backstop(None);
        }
        Ok(())
    }

    /// Looks up this actor's task in its system's registry, as this
    /// incarnation: its abort handle (absent until the task's abort handle is
    /// attached), its kill grace and the runtime that drives it.
    fn kill_target(
        &self,
    ) -> Option<(
        std::sync::Arc<ActorSystem>,
        Option<AbortHandle>,
        Duration,
        Handle,
    )> {
        let system = ActorSystem::get_named(&self.system_name)?;
        let (abort, grace, runtime) = system.kill_target_for(&self.id, &self.status_rx)?;
        Some((system, abort, grace, runtime))
    }

    /// Schedules the abort of the task after a `Kill` was raised, on the
    /// runtime that drives it: after `window` when given, otherwise after the
    /// actor's own kill grace. A target whose abort handle is not attached
    /// yet is armed by the attach itself.
    fn arm_backstop(&self, window: Option<Duration>) {
        if let Some((system, Some(abort), grace, runtime)) = self.kill_target() {
            system.arm_kill_backstop(
                abort,
                self.stop_lane.abort_record(),
                window.unwrap_or(grace),
                &runtime,
            );
        }
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
    /// already `Kill` and the deadline passes first, this escalates to `Kill`
    /// and waits the actor's kill grace; if the actor still has not reached a
    /// terminal status by then, its task is aborted directly and this waits
    /// a short settle time before giving up. The abort is also scheduled by
    /// the system when the `Kill` is accepted while the actor is still
    /// registered, so dropping this future never cancels it. OTP parity: `sys:terminate/3`, which likewise bounds the
    /// CALLER's wait rather than the target's own shutdown.
    ///
    /// A handler that never yields back to the run loop defeats every bound
    /// here - `Kill` is only ever observed at a turn boundary, and an abort
    /// only takes effect at the task's next `.await` point - and is reported
    /// as [`StopOutcome::Unresponsive`].
    ///
    /// # Dropping the future
    /// Drive this future to completion, or spawn it. If it is dropped, the stop
    /// request already raised stays raised and is not withdrawn (a `pre_stop`
    /// may still veto a `Graceful` one), and the escalation steps it has not
    /// reached are not taken, so nothing further is raised. A `Kill` that this
    /// call already raised and that the actor accepted while it was still
    /// registered still ends in its abort, after the actor's kill grace, or
    /// after `within` for `stop_within(StopReason::Kill, within)`.
    ///
    /// # Errors
    /// Returns `SendError::Closed` if the actor does not accept the
    /// request any more, the same case [`stop`](Self::stop) reports.
    pub async fn stop_within(
        &self,
        reason: StopReason,
        within: Duration,
    ) -> Result<StopOutcome, SendError> {
        let mut rx = self.status_rx.clone();
        let is_kill = matches!(reason, StopReason::Kill);
        // The incarnation's own kill grace, read while its registry entry
        // still holds it: the entry is freed before the actor publishes
        // `Stopped`, and the Kill step's wait must hold after that. An actor
        // already out of the registry refuses every reason but `Kill` below.
        let kill_grace = self
            .kill_target()
            .map_or(DEFAULT_KILL_GRACE, |(_, _, grace, _)| grace);
        if self.stop_lane.try_raise(reason) == RaiseResult::Refused {
            return Err(SendError::Closed);
        }

        // What a `Stopped` the actor publishes reads as: `Killed` once this
        // call's `Kill` was taken by the lane.
        let mut requested = StopOutcome::Killed;
        if is_kill {
            self.arm_backstop(Some(within));
            if wait_terminal(&mut rx, within).await {
                return Ok(terminal_outcome(&rx, requested));
            }
        } else {
            if wait_terminal(&mut rx, within).await {
                return Ok(terminal_outcome(&rx, StopOutcome::Graceful));
            }
            #[cfg(test)]
            if let Some(system) = ActorSystem::get_named(&self.system_name) {
                system.run_ladder_hook(crate::system::LadderPoint::StopWithinKill, &self.id);
            }
            // A lane that refuses the `Kill` belongs to an actor already past
            // its last read of it: it ends with its own reason.
            if self.stop_lane.try_raise(StopReason::Kill) == RaiseResult::Refused {
                requested = StopOutcome::Graceful;
            } else {
                self.arm_backstop(None);
            }
            if wait_terminal(&mut rx, kill_grace).await {
                return Ok(terminal_outcome(&rx, requested));
            }
        }

        if let Some((_, Some(abort), _, _)) = self.kill_target() {
            self.stop_lane.abort_record().abort(&abort);
        }

        if wait_terminal(&mut rx, ABORT_SETTLE).await {
            return Ok(terminal_outcome(&rx, requested));
        }

        Ok(StopOutcome::Unresponsive)
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
    /// is busy running that very callback. A descendant that queries this
    /// actor while this actor is stopping has the same shape: the query is
    /// answered with the actor's end, and the actor's teardown waits for the
    /// descendant, so under [`Shutdown::Infinity`](crate::types::Shutdown::Infinity)
    /// neither returns. For a hung actor's status without these risks, use
    /// the instant, always-answering [`status`](Self::status).
    ///
    /// # Errors
    /// - [`AskError::Closed`] if the actor's system channel is closed: the
    ///   actor has ended, or is past its message loop.
    /// - [`AskError::ResponseDropped`] if the actor stopped after accepting
    ///   the query but before answering. It carries the reason the actor
    ///   ended with, and is returned once that reason is final, which waits
    ///   through the rest of the actor's teardown (the stop of its children
    ///   included) and has no bound of its own: bound the call with
    ///   `tokio::time::timeout` when the target's teardown may not return.
    pub async fn get_status(&self) -> Result<ActorStatusInfo, AskError> {
        let (tx, rx) = oneshot::channel();
        self.system_tx
            .send(SystemMessage::GetStatus(tx))
            .await
            .map_err(|_| AskError::Closed)?;
        match rx.await {
            Ok(info) => Ok(info),
            Err(_) => Err(self.response_dropped().await),
        }
    }

    /// Waits until the actor reaches its terminal [`ActorStatus::Stopped`]
    /// state and returns the reason it stopped with.
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
    /// the abort that ends it is a kill.
    ///
    /// Uses the same runtime status plane as [`status`](Self::status), so it
    /// resolves even for an actor hung inside a callback.
    ///
    /// This resolves when THIS actor has stopped. A supervising parent
    /// observes the same death separately, in its own run loop, so
    /// [`on_child_stopped`](crate::actor::Actor::on_child_stopped) may run
    /// slightly after this call returns.
    pub async fn wait_stopped(&self) -> StopReason {
        let mut rx = self.status_rx.clone();
        wait_reason(&mut rx).await
    }
}

/// A slot reserved in an actor's mailbox by
/// [`reserve_forward`](ActorHandle::reserve_forward): sending through it
/// cannot wait or fail, and dropping it frees the slot.
pub(crate) struct ForwardPermit<'a, A: Actor>(mpsc::Permit<'a, ActorEnvelope<A>>);

impl<A: Actor> ForwardPermit<'_, A> {
    /// Puts `msg` in the mailbox, as [`forward`](ActorHandle::forward) does.
    pub(crate) fn send(self, msg: A::Message) {
        self.0.send(Envelope::Message {
            payload: msg,
            responder: None,
        });
    }
}
