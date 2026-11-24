//! Casts and calls: `notify`, `send` and their priority, try and timed forms.

use std::pin::{pin, Pin};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time;
use tokio::time::Instant;

use super::{
    lane_room, poll_cast_unit, queue_closed_after_push, reserve_priority, settled, ActorHandle,
    Edged, ForwardPermit,
};
use crate::actor::envelope::Envelope;
#[cfg(test)]
use crate::actor::handle::late_slot_tests;
use crate::actor::panic::{try_reserve_held, WakeHold};
use crate::actor::runtime::saturating_deadline;
use crate::actor::{Actor, ActorEnvelope};
use crate::error::{ActorError, AskError, SendError, TrySendError};
use crate::system::wait_final_reason;
use crate::telemetry::wait_graph::wait_wanted;

impl<A: Actor> ActorHandle<A> {
    /// Sends a message to the actor without waiting for a response (fire-and-forget).
    ///
    /// This method awaits if the mailbox is full.
    ///
    /// Once the actor has left its message loop for good (its `on_stopped` hook
    /// has returned, or a `Kill` ended the loop, which skips that hook), sends
    /// are refused throughout the teardown of its children, not only once its
    /// task has fully exited. A message accepted while the actor runs a
    /// supervisor's final deliveries or `on_stopped` is dropped unhandled.
    ///
    /// Two actors that each `notify` the other from `handle` while both
    /// mailboxes are full wait on each other for good, because a handler runs
    /// to its end before its actor receives again. A caller outside the cycle
    /// leaves it with [`try_notify`](Self::try_notify), which answers `Full`,
    /// or with [`send_timeout`](Self::send_timeout), which answers
    /// `Timeout { enqueued: false }`. A message whose `notify` has returned is
    /// received before any message that another actor notifies after that
    /// return; a `notify` still waiting for room promises no order against
    /// other waiting senders.
    ///
    /// A call made on the actor's own task (from a callback, or from a future
    /// the callback awaits in place) to its own full mailbox answers
    /// [`SendError::Full`] at once, since that task is the one that makes
    /// room; with room it sends as a call from any other task does. A task
    /// or thread the callback starts and waits for is not the actor's own
    /// task: its send to the full mailbox waits for room as any other does.
    ///
    /// # Errors
    /// - `SendError::Closed` if the actor has stopped.
    /// - `SendError::Full` if the call is made on the actor's own task and its
    ///   mailbox is full.
    pub async fn notify(&self, msg: A::Message) -> Result<(), SendError> {
        let record = self.inner.stop_lane.abort_record();
        let mut reserve = pin!(self.inner.tx.reserve());
        let wait = std::future::poll_fn(|cx| poll_cast_unit(reserve.as_mut(), cx, record));
        let permit = if wait_wanted(record) {
            Edged::new(pin!(wait), record).await
        } else {
            wait.await
        }?;
        permit.send(Envelope::Message {
            payload: msg,
            responder: None,
        });
        Ok(())
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
    /// The lane's length and capacity are observable here ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure by
    /// awaiting here where Erlang's single queue is unbounded. Any holder of
    /// this actor's handle can send on the lane unless the actor leaves it to
    /// its priority aliases
    /// ([`with_priority_by_alias`](crate::actor::runtime::ActorConfig::with_priority_by_alias)),
    /// where Erlang/OTP lets a sender use priority only through an alias the
    /// receiver created for it, so the receiver chooses who may.
    ///
    /// A call made on the actor's own task to its own full lane answers
    /// [`SendError::Full`] at once, as [`notify`](Self::notify) does for a
    /// full mailbox.
    ///
    /// # Errors
    /// - `SendError::Closed` if the actor has stopped.
    /// - `SendError::Full` if the call is made on the actor's own task and the
    ///   queue it uses is full.
    pub async fn notify_priority(&self, msg: A::Message) -> Result<(), SendError> {
        let Some(tx) = self.open_lane() else {
            return self.notify(msg).await;
        };
        let permit = self.reserve_lane_cast(tx).await?;
        permit.into_inner().send(Envelope::Message {
            payload: msg,
            responder: None,
        });
        Ok(())
    }

    /// Attempts to send a message without blocking.
    ///
    /// `Ok` means the mailbox accepted the message, not that it will be
    /// handled: one accepted while the actor runs its final deliveries or
    /// `on_stopped`, or as its end closes the queue after the check, is
    /// dropped unhandled. With an error the message is dropped; keep a copy
    /// to retry.
    ///
    /// # Errors
    /// - `TrySendError::Full` if the mailbox is full.
    /// - `TrySendError::Closed` if the actor has stopped.
    pub fn try_notify(&self, msg: A::Message) -> Result<(), TrySendError> {
        self.inner
            .tx
            .try_send(Envelope::Message {
                payload: msg,
                responder: None,
            })
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => TrySendError::Full,
                mpsc::error::TrySendError::Closed(_) => TrySendError::Closed,
            })
    }

    /// A slot of the priority lane for a cast: as the lane's wait, except that
    /// a call made on the actor's own task to its own full lane is refused
    /// with `Full` at the point where it would wait, since only that task
    /// makes room.
    async fn reserve_lane_cast<'a>(
        &'a self,
        lane: &'a mpsc::Sender<ActorEnvelope<A>>,
    ) -> Result<WakeHold<mpsc::Permit<'a, ActorEnvelope<A>>>, SendError> {
        let record = self.inner.stop_lane.abort_record();
        match try_reserve_held("priority lane", Some(&self.inner.id), lane) {
            Ok(permit) => {
                lane_room(self.lane_warned());
                return Ok(permit);
            }
            Err(mpsc::error::TrySendError::Closed(())) => return Err(SendError::Closed),
            Err(mpsc::error::TrySendError::Full(())) if record.is_self_call() => {
                return Err(SendError::Full);
            }
            Err(mpsc::error::TrySendError::Full(())) => {}
        }
        let wait = reserve_priority(lane, &self.inner.id, self.lane_warned(), record);
        if wait_wanted(record) {
            Edged::new(pin!(wait), record).await
        } else {
            wait.await
        }
        .ok_or(SendError::Closed)
    }

    /// A slot for a cast through an alias: on the priority lane when
    /// `priority` is set and the actor has one, otherwise on the mailbox. A
    /// call made on the actor's own task that would wait for its own full
    /// queue is refused with `Full`; the slot of a forwarder is taken with
    /// [`reserve_signal`](Self::reserve_signal), which always waits.
    pub(crate) async fn reserve_cast(
        &self,
        priority: bool,
    ) -> Result<ForwardPermit<'_, A>, SendError> {
        match (&self.inner.priority_tx, priority) {
            (Some(lane), true) => self
                .reserve_lane_cast(lane)
                .await
                .map(|permit| ForwardPermit::Priority(permit, lane)),
            _ => {
                let record = self.inner.stop_lane.abort_record();
                let mut reserve = pin!(self.inner.tx.reserve());
                std::future::poll_fn(|cx| poll_cast_unit(reserve.as_mut(), cx, record))
                    .await
                    .map(ForwardPermit::Normal)
            }
        }
    }

    /// As [`reserve_signal`](Self::reserve_signal) without waiting: a full or
    /// closed queue is refused. Used by aliases, which choose the lane
    /// themselves and so ignore the gate that leaves it to them.
    pub(crate) fn try_reserve_signal(
        &self,
        priority: bool,
    ) -> Result<ForwardPermit<'_, A>, TrySendError> {
        match (&self.inner.priority_tx, priority) {
            (Some(lane), true) => try_reserve_held("priority lane", Some(&self.inner.id), lane)
                .map(|permit| ForwardPermit::Priority(permit, lane))
                .map_err(|err| match err {
                    mpsc::error::TrySendError::Full(()) => TrySendError::Full,
                    mpsc::error::TrySendError::Closed(()) => TrySendError::Closed,
                }),
            _ => self.inner.tx.try_reserve().map(ForwardPermit::Normal),
        }
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
    /// The lane's length and capacity are observable here ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure
    /// (`Full`, below) where Erlang's single queue is unbounded. Any holder of
    /// this actor's handle can send on the lane unless the actor leaves it to
    /// its priority aliases
    /// ([`with_priority_by_alias`](crate::actor::runtime::ActorConfig::with_priority_by_alias)),
    /// where Erlang/OTP lets a sender use priority only through an alias the
    /// receiver created for it, so the receiver chooses who may.
    ///
    /// `Ok` means the lane accepted the message, not that it will be
    /// handled: one accepted while the actor runs its final deliveries or
    /// `on_stopped` is dropped unhandled. With an error the message is
    /// dropped; keep a copy to retry.
    ///
    /// # Errors
    /// - `TrySendError::Full` if the priority lane is full.
    /// - `TrySendError::Closed` if the actor has stopped.
    pub fn try_notify_priority(&self, msg: A::Message) -> Result<(), TrySendError> {
        let Some(tx) = self.open_lane() else {
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
    /// Kind: a call from outside, through the mailbox. See [`signals`](crate::signals).
    ///
    /// To answer later, as a deferred reply does in Erlang/OTP, put a
    /// `tokio::sync::oneshot::Sender` in the message and answer it from a later
    /// turn.
    ///
    /// # Calling oneself
    /// A call from the actor's own task to its own handle (from `handle`, a
    /// start or stop hook, `on_child_stopped` or a stop-time delivery) returns
    /// `AskError::CallingSelf` at once and sends nothing: the actor is a
    /// single task that is busy running the callback, so it could never reach
    /// its mailbox to answer its own request, as in Erlang/OTP, where
    /// `gen_server:call` to oneself fails with `calling_self`. The check
    /// refuses only the actor's own task. A task spawned from a callback, a
    /// `spawn_blocking` thread, another actor and another or an ended
    /// incarnation are not refused, so a callback that awaits such a task
    /// which then calls this actor still parks forever, and so do two actors
    /// that `send` each other from their callbacks: [`send_timeout`](Self::send_timeout)
    /// is the way out of that cycle. `notify` and the other casts wait for
    /// no answer, so they are not refused as a call to oneself; one made on
    /// the actor's own task that would have to wait for room in its own full
    /// queue answers [`SendError::Full`] at once
    /// ([`AliasError::Full`](crate::AliasError::Full) through an alias).
    ///
    /// # Errors
    /// - `AskError::CallingSelf` if the call is made from the actor's own
    ///   task (nothing was sent).
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
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor.
    ///   The caller reads it the way an Erlang/OTP `gen_server:call` caller
    ///   reads a crash, after the process is dead: the error is returned when
    ///   the actor seals its stop lane, after its `on_stopped` and its
    ///   children's teardown, with its name already free and before its queued
    ///   messages, its value and its context are dropped. A lookup by name
    ///   right after it finds no registration, or the next incarnation if a
    ///   supervisor restarts the actor, never the one that crashed: retry with
    ///   a fresh [`ActorSystem::get`](crate::system::ActorSystem::get); `None`
    ///   is final for an actor that nothing restarts, and for a supervised one
    ///   it means either that the next incarnation is not registered yet or
    ///   that its supervisor will not restart it, so bound the retries. The
    ///   wait for the seal has no bound of its own, like the one for
    ///   `ResponseDropped`: when the teardown of the target may not return,
    ///   bound the call with `tokio::time::timeout`. A child of the target
    ///   whose handler awaits this call cannot take its parent's stop until
    ///   the call returns: the target's shutdown window for it runs out and it
    ///   is killed instead of stopped in order (under `Shutdown::Infinity` the
    ///   window never runs out). A `Kill`, an abort or the end of the actor's
    ///   runtime that ends the actor first makes the call return
    ///   `ResponseDropped` with the final reason instead.
    pub async fn send(&self, msg: A::Message) -> Result<A::Response, AskError> {
        self.refuse_self_call()?;
        let (tx, mut rx) = oneshot::channel();
        let record = self.inner.stop_lane.abort_record();
        let edged = wait_wanted(record);
        let permit = if edged {
            Edged::new(pin!(self.inner.tx.reserve()), record).await
        } else {
            self.inner.tx.reserve().await
        }
        .map_err(|_| AskError::Closed)?;
        #[cfg(test)]
        late_slot_tests::between_reserve_and_push();
        permit.send(Envelope::Message {
            payload: msg,
            responder: Some(tx),
        });
        if queue_closed_after_push(|| self.inner.tx.is_closed()) {
            return self.answer_after_close(rx).await;
        }

        // Decided again here: a send first polled outside a hook, or with
        // detection off, can begin its reply wait inside a hook with it on.
        let reply = if wait_wanted(record) {
            Edged::new(Pin::new(&mut rx), record).await
        } else {
            rx.await
        };
        match reply {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(err)) => Err(AskError::Actor(err)),
            Err(_) => Err(self.response_dropped().await),
        }
    }

    /// The answer to a request that found its queue closed after its push:
    /// the actor ended while the request was on its way. A reply the actor
    /// sent before it closed the queue is read here; otherwise the request
    /// was dropped unhandled or is left in the queue unseen, and the caller
    /// reads the actor's final reason from the status plane instead of
    /// waiting on a reply nobody will send. The actor sends every reply
    /// before it closes the queue, except the answer to a request whose
    /// handler failed. That one is sent at the lane's seal, before the final
    /// reason is written, so it is read once more after that reason: no
    /// answer is missed.
    pub(super) async fn answer_after_close(
        &self,
        mut rx: oneshot::Receiver<Result<A::Response, ActorError>>,
    ) -> Result<A::Response, AskError> {
        if let Some(answer) = settled(rx.try_recv()) {
            return answer;
        }
        let dropped = self.response_dropped().await;
        settled(rx.try_recv()).unwrap_or(Err(dropped))
    }

    /// The error for a request whose reply channel closed: the actor ended
    /// before it replied. Reads the reason the actor ended with from the
    /// runtime status plane, waiting until the reason is final (written at
    /// the actor's lane seal) or the plane closes.
    pub(super) async fn response_dropped(&self) -> AskError {
        let mut rx = self.inner.status_rx.clone();
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
    /// The lane's length and capacity are observable here ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure by
    /// awaiting here where Erlang's single queue is unbounded. Any holder of
    /// this actor's handle can send on the lane unless the actor leaves it to
    /// its priority aliases
    /// ([`with_priority_by_alias`](crate::actor::runtime::ActorConfig::with_priority_by_alias)),
    /// where Erlang/OTP lets a sender use priority only through an alias the
    /// receiver created for it, so the receiver chooses who may.
    ///
    /// # Calling oneself
    /// As for [`send`](Self::send): a call from the actor's own task returns
    /// `AskError::CallingSelf` at once, and a task spawned from a callback
    /// that calls this actor while the callback awaits it still parks forever.
    ///
    /// # Errors
    /// - `AskError::CallingSelf` if the call is made from the actor's own
    ///   task (nothing was sent).
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
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor.
    ///   The caller reads it the way an Erlang/OTP `gen_server:call` caller
    ///   reads a crash, after the process is dead: the error is returned when
    ///   the actor seals its stop lane, after its `on_stopped` and its
    ///   children's teardown, with its name already free and before its queued
    ///   messages, its value and its context are dropped. A lookup by name
    ///   right after it finds no registration, or the next incarnation if a
    ///   supervisor restarts the actor, never the one that crashed: retry with
    ///   a fresh [`ActorSystem::get`](crate::system::ActorSystem::get); `None`
    ///   is final for an actor that nothing restarts, and for a supervised one
    ///   it means either that the next incarnation is not registered yet or
    ///   that its supervisor will not restart it, so bound the retries. The
    ///   wait for the seal has no bound of its own, like the one for
    ///   `ResponseDropped`: when the teardown of the target may not return,
    ///   bound the call with `tokio::time::timeout`. A child of the target
    ///   whose handler awaits this call cannot take its parent's stop until
    ///   the call returns: the target's shutdown window for it runs out and it
    ///   is killed instead of stopped in order (under `Shutdown::Infinity` the
    ///   window never runs out). A `Kill`, an abort or the end of the actor's
    ///   runtime that ends the actor first makes the call return
    ///   `ResponseDropped` with the final reason instead.
    pub async fn send_priority(&self, msg: A::Message) -> Result<A::Response, AskError> {
        let Some(tx) = self.open_lane() else {
            return self.send(msg).await;
        };
        self.refuse_self_call()?;
        let (resp_tx, mut resp_rx) = oneshot::channel();
        let record = self.inner.stop_lane.abort_record();
        let edged = wait_wanted(record);

        let permit = if edged {
            Edged::new(
                pin!(reserve_priority(
                    tx,
                    &self.inner.id,
                    self.lane_warned(),
                    record
                )),
                record,
            )
            .await
        } else {
            reserve_priority(tx, &self.inner.id, self.lane_warned(), record).await
        }
        .ok_or(AskError::Closed)?
        .into_inner();
        #[cfg(test)]
        late_slot_tests::between_reserve_and_push();
        permit.send(Envelope::Message {
            payload: msg,
            responder: Some(resp_tx),
        });
        if queue_closed_after_push(|| tx.is_closed()) {
            return self.answer_after_close(resp_rx).await;
        }

        // Decided again here: a send first polled outside a hook, or with
        // detection off, can begin its reply wait inside a hook with it on.
        let reply = if wait_wanted(record) {
            Edged::new(Pin::new(&mut resp_rx), record).await
        } else {
            resp_rx.await
        };
        match reply {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(err)) => Err(AskError::Actor(err)),
            Err(_) => Err(self.response_dropped().await),
        }
    }

    /// Sends a message and waits for a response, bounded by a single deadline
    /// that spans both enqueueing the message and awaiting the reply. A timeout
    /// beyond 30 years is treated as 30 years.
    ///
    /// Kind: a call from outside, through the mailbox. See [`signals`](crate::signals).
    ///
    /// Mirrors `gen_server:call/3`: one clock for the whole request. The
    /// deadline is armed BEFORE the mailbox send, so a request parked on a
    /// full mailbox still times out instead of waiting indefinitely.
    ///
    /// # Calling oneself
    /// A call from the actor's own task returns `AskError::CallingSelf` at
    /// once, as [`send`](Self::send) does. A cycle of two actors that call each
    /// other from their callbacks is not detected: the deadline of this call is
    /// the way out of it.
    ///
    /// # Errors
    /// - `AskError::CallingSelf` if the call is made from the actor's own
    ///   task (nothing was sent).
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
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor.
    ///   The caller reads it the way an Erlang/OTP `gen_server:call` caller
    ///   reads a crash, after the process is dead: the error is returned when
    ///   the actor seals its stop lane, after its `on_stopped` and its
    ///   children's teardown, with its name already free and before its queued
    ///   messages, its value and its context are dropped. A lookup by name
    ///   right after it finds no registration, or the next incarnation if a
    ///   supervisor restarts the actor, never the one that crashed: retry with
    ///   a fresh [`ActorSystem::get`](crate::system::ActorSystem::get); `None`
    ///   is final for an actor that nothing restarts, and for a supervised one
    ///   it means either that the next incarnation is not registered yet or
    ///   that its supervisor will not restart it, so bound the retries. The
    ///   wait for the seal counts against this call's own deadline: a deadline
    ///   that passes first reads `Timeout { enqueued: true }`. A child of the
    ///   target whose handler awaits this call takes its parent's stop only
    ///   once the call returns, at the latest at its deadline. A `Kill`, an
    ///   abort or the end of the actor's runtime that ends the actor first
    ///   makes the call return `ResponseDropped` with the final reason
    ///   instead.
    pub async fn send_timeout(
        &self,
        msg: A::Message,
        timeout: Duration,
    ) -> Result<A::Response, AskError> {
        self.refuse_self_call()?;
        let deadline = saturating_deadline(Instant::now(), timeout);
        let (tx, rx) = oneshot::channel();

        let permit = match time::timeout_at(deadline, self.inner.tx.reserve()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(AskError::Closed),
            Err(_) => return Err(AskError::Timeout { enqueued: false }),
        };
        #[cfg(test)]
        late_slot_tests::between_reserve_and_push();
        permit.send(Envelope::Message {
            payload: msg,
            responder: Some(tx),
        });
        if queue_closed_after_push(|| self.inner.tx.is_closed()) {
            return time::timeout_at(deadline, self.answer_after_close(rx))
                .await
                .unwrap_or(Err(AskError::Timeout { enqueued: true }));
        }

        let reply = time::timeout_at(deadline, async {
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
    /// The lane's length and capacity are observable here ([`priority_len`](Self::priority_len),
    /// [`priority_capacity`](Self::priority_capacity)) where Erlang exposes
    /// only the total queue length, and a full lane applies backpressure (a
    /// timeout, below) where Erlang's single queue is unbounded. Any holder of
    /// this actor's handle can send on the lane unless the actor leaves it to
    /// its priority aliases
    /// ([`with_priority_by_alias`](crate::actor::runtime::ActorConfig::with_priority_by_alias)),
    /// where Erlang/OTP lets a sender use priority only through an alias the
    /// receiver created for it, so the receiver chooses who may.
    ///
    /// # Calling oneself
    /// A call from the actor's own task returns `AskError::CallingSelf` at
    /// once, as [`send`](Self::send) does.
    ///
    /// # Errors
    /// - `AskError::CallingSelf` if the call is made from the actor's own
    ///   task (nothing was sent).
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
    ///   (`ActorError::Panic`). A handler error or panic also stops the actor.
    ///   The caller reads it the way an Erlang/OTP `gen_server:call` caller
    ///   reads a crash, after the process is dead: the error is returned when
    ///   the actor seals its stop lane, after its `on_stopped` and its
    ///   children's teardown, with its name already free and before its queued
    ///   messages, its value and its context are dropped. A lookup by name
    ///   right after it finds no registration, or the next incarnation if a
    ///   supervisor restarts the actor, never the one that crashed: retry with
    ///   a fresh [`ActorSystem::get`](crate::system::ActorSystem::get); `None`
    ///   is final for an actor that nothing restarts, and for a supervised one
    ///   it means either that the next incarnation is not registered yet or
    ///   that its supervisor will not restart it, so bound the retries. The
    ///   wait for the seal counts against this call's own deadline: a deadline
    ///   that passes first reads `Timeout { enqueued: true }`. A child of the
    ///   target whose handler awaits this call takes its parent's stop only
    ///   once the call returns, at the latest at its deadline. A `Kill`, an
    ///   abort or the end of the actor's runtime that ends the actor first
    ///   makes the call return `ResponseDropped` with the final reason
    ///   instead.
    pub async fn send_priority_timeout(
        &self,
        msg: A::Message,
        timeout: Duration,
    ) -> Result<A::Response, AskError> {
        let Some(tx) = self.open_lane() else {
            return self.send_timeout(msg, timeout).await;
        };
        self.refuse_self_call()?;
        let deadline = saturating_deadline(Instant::now(), timeout);
        let (resp_tx, resp_rx) = oneshot::channel();

        let permit = match time::timeout_at(
            deadline,
            reserve_priority(
                tx,
                &self.inner.id,
                None,
                self.inner.stop_lane.abort_record(),
            ),
        )
        .await
        {
            Ok(Some(permit)) => permit.into_inner(),
            Ok(None) => return Err(AskError::Closed),
            Err(_) => return Err(AskError::Timeout { enqueued: false }),
        };
        #[cfg(test)]
        late_slot_tests::between_reserve_and_push();
        permit.send(Envelope::Message {
            payload: msg,
            responder: Some(resp_tx),
        });
        if queue_closed_after_push(|| tx.is_closed()) {
            return time::timeout_at(deadline, self.answer_after_close(resp_rx))
                .await
                .unwrap_or(Err(AskError::Timeout { enqueued: true }));
        }

        let reply = time::timeout_at(deadline, async {
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
}
