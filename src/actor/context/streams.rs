//! Attached streams: `add_stream` and its cancellation.

use futures_core::Stream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use super::{
    release, ActorContext, ForwarderKind, PanicSlot, ReleaseGuard, Released, RunPanic,
    StreamRegistration,
};
use crate::actor::handle::ActorHandle;
use crate::actor::{runtime, Actor};
use crate::types::{StreamEvent, StreamId};

impl<A: Actor> ActorContext<A> {
    // - Streams ------------------------------------------------------------

    /// Attaches an external stream to this actor's mailbox.
    ///
    /// Kind: a registration of a source of the actor's own; accepted while the actor is stopping, and it then produces nothing. See [`signals`](crate::signals).
    ///
    /// Like a recurring [`schedule`](Self::schedule), the forwarder task
    /// holds an internal self-reference (see [`self_handle`](Self::self_handle))
    /// that never extends the actor's lifetime: it is torn down when the
    /// actor stops.
    ///
    /// A one-item stream also gives a request that does not block the turn,
    /// as Erlang/OTP's `send_request` does: clone the peer's handle and attach
    /// `Box::pin(futures::stream::once(async move { h.send(msg).await }))`,
    /// the reply mapped into the actor's message through `From<StreamEvent<_>>`
    /// (the `futures` crate is the caller's). An actor can likewise stand in
    /// for Erlang/OTP's `supervisor_bridge`: its `on_started` spawns the task
    /// and attaches `futures::stream::once(task)` over the task's `JoinHandle`
    /// as a one-item stream, its `handle` turns the task's end into an `Err`,
    /// so the actor stops with `StopReason::Failure` (a `supervisor_bridge`
    /// stops with the subsystem's own exit reason), and its `on_stopped`
    /// aborts the task.
    pub fn add_stream<S>(&mut self, stream: S) -> StreamId
    where
        S: Stream + Send + Unpin + 'static,
        S::Item: Send + 'static,
        A::Message: From<StreamEvent<S::Item>>,
    {
        let id = self.ex_mut().id_gen.next_stream_id();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.self_handle.clone();
        let slot = PanicSlot::default();
        let fut = stream_forward::<A, S>(stream, handle, cancel_clone, slot.clone());
        self.spawn_forwarder(ForwarderKind::Stream(id), fut, slot);
        self.ex_mut()
            .streams
            .insert(id, StreamRegistration { token });
        id
    }

    /// Cancels a specific stream by its ID. Kind: a revoke, see
    /// [`signals`](crate::signals).
    ///
    /// `true` means the stream was still held (a stream that finished is held
    /// until the loop takes its end). Once the call returns, nothing more from
    /// the stream is put in the mailbox, except an item that a worker was
    /// already putting there at that instant (multi-thread runtimes); items
    /// already in the mailbox are handled as usual. An item that waits for
    /// room in a full mailbox is dropped by the stream's forwarder in its next
    /// poll instead of being delivered, and so is a `Finished` that waits;
    /// neither is dropped inside the call. `false` means there was no such
    /// stream.
    pub fn cancel_stream(&mut self, id: StreamId) -> bool {
        match self.ex_mut().streams.remove(&id) {
            Some(entry) => {
                entry.token.cancel();
                true
            }
            None => false,
        }
    }

    /// Cancels all active streams, each as [`cancel_stream`](Self::cancel_stream)
    /// describes.
    pub fn cancel_all_streams(&mut self) {
        if let Some(extras) = self.extras.as_deref_mut() {
            for entry in extras.streams.values() {
                entry.token.cancel();
            }
            extras.streams.clear();
        }
    }

    /// Returns the number of active streams.
    pub fn active_stream_count(&self) -> usize {
        self.ex().streams.len()
    }
}

pub(super) async fn stream_forward<A, S>(
    stream: S,
    handle: ActorHandle<A>,
    token: CancellationToken,
    slot: PanicSlot,
) -> Released
where
    A: Actor,
    S: Stream + Send + Unpin + 'static,
    S::Item: Send + 'static,
    A::Message: From<StreamEvent<S::Item>>,
{
    // The first local of the body: when the future is dropped (the end's
    // abort), the item and the send in flight, which live in inner scopes,
    // drop before it, so the stream's drop comes last. Keep it first.
    let mut stream = ReleaseGuard::new(stream, handle.owner(), slot);
    let closed = loop {
        tokio::select! {
            biased;
            // A registration cancelled during the run: the stream is dropped
            // when this returns, in this poll.
            _ = token.cancelled() => return Released::default(),
            // The actor stopped; once the mailbox is closed the stream is
            // never polled again.
            () = handle.mailbox_close() => break true,
            item = StreamExt::next(stream.get_mut()) => {
                match item {
                    Some(value) => {
                        let msg: A::Message = StreamEvent::Data(value).into();
                        // The item waits for a slot in the mailbox and is never
                        // moved into the wait: a cancel that comes first ends
                        // the stream, and the item and the stream drop each in
                        // their own catch, in this poll, as the run's.
                        let permit = tokio::select! {
                            biased;
                            _ = token.cancelled() => {
                                let mut run = release(msg);
                                runtime::keep_first_of(&mut run, release(stream.take()), &handle.owner());
                                return Released { run, end: None };
                            }
                            permit = handle.reserve_forward() => permit,
                        };
                        let Some(permit) = permit else {
                            return Released::both_at_the_close(msg, stream.take(), &handle.owner());
                        };
                        permit.send(msg);
                    }
                    None => break false,
                }
            }
        }
    };
    if closed {
        return Released::at_the_close(stream.take());
    }
    // The close read just before the stream's drop is the order point.
    if handle.mailbox_closed() {
        return Released::at_the_close(stream.take());
    }
    // The stream ended during the run: it is dropped here, before its
    // `Finished` is sent.
    let mut run = RunPanic {
        payload: release(stream.take()),
        owner: handle.id().clone(),
        #[cfg(feature = "tracing")]
        record: handle.stop_lane_ref().abort_record().clone(),
    };
    let msg: A::Message = StreamEvent::Finished.into();
    // `Finished` waits for a slot as an item does, and a cancel that comes
    // first drops it.
    let permit = tokio::select! {
        biased;
        _ = token.cancelled() => {
            let mut dropped = run.take();
            runtime::keep_first_of(&mut dropped, release(msg), &handle.owner());
            return Released { run: dropped, end: None };
        }
        permit = handle.reserve_forward() => permit,
    };
    match permit {
        Some(permit) => {
            permit.send(msg);
            Released {
                run: run.take(),
                end: None,
            }
        }
        None => Released {
            run: run.take(),
            end: release(msg),
        },
    }
}
