//! Handle-based communication API for actors.

use tokio::sync::{mpsc, oneshot};

use crate::actor::{Actor, ActorEnvelope};
use crate::error::{AskError, SendError, TrySendError};
use crate::types::{ActorId, Envelope, StopReason};

/// Cloneable handle that callers use to communicate with an actor.
#[derive(Debug)]
pub struct ActorHandle<A: Actor> {
    id: ActorId,
    tx: mpsc::Sender<ActorEnvelope<A>>,
    mailbox_capacity: usize,
}

impl<A: Actor> Clone for ActorHandle<A> {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            tx: self.tx.clone(),
            mailbox_capacity: self.mailbox_capacity,
        }
    }
}

impl<A: Actor> PartialEq for ActorHandle<A> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<A: Actor> Eq for ActorHandle<A> {}

impl<A: Actor> std::hash::Hash for ActorHandle<A> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<A: Actor> ActorHandle<A> {
    pub(crate) fn new(
        id: ActorId,
        tx: mpsc::Sender<ActorEnvelope<A>>,
        mailbox_capacity: usize,
    ) -> Self {
        Self {
            id,
            tx,
            mailbox_capacity,
        }
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
    /// # Errors
    /// - `AskError::Send(Closed)` if the actor is stopped.
    /// - `AskError::ResponseDropped` if the actor dropped the responder without replying.
    /// - `AskError::Actor(err)` if the actor returned an error.
    pub async fn send(&self, msg: A::Message) -> Result<A::Response, AskError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Envelope::Message {
                payload: msg,
                responder: Some(tx),
            })
            .await
            .map_err(|_| SendError::Closed)?;

        match rx.await.map_err(|_| AskError::ResponseDropped)? {
            Ok(resp) => Ok(resp),
            Err(err) => Err(AskError::Actor(err)),
        }
    }

    /// Signals the actor to stop.
    ///
    /// The actor will process any pending messages before stopping, unless the
    /// stop reason implies an immediate halt (though currently all stops are processed in order).
    pub async fn stop(&self, reason: StopReason) -> Result<(), SendError> {
        self.tx
            .send(Envelope::Stop(reason))
            .await
            .map_err(|_| SendError::Closed)
    }
}
