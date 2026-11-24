//! Telemetry subscriptions of an actor: `subscribe` and `unsubscribe`.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    map_and_send, release, ActorContext, ForwarderKind, PanicSlot, ReleaseGuard, Released,
};
use crate::actor::handle::ActorHandle;
use crate::actor::Actor;
use crate::telemetry::{Delivery, Filter, SubscriptionId, SubscriptionNotice, TelemetryEvent};

impl<A: Actor> ActorContext<A> {
    // - Event subscriptions ------------------------------------------------

    /// Subscribes this actor to the telemetry events of its system that
    /// `filter` selects, each handed to `map` as a [`Delivery`] and turned
    /// into a message of this actor's own type.
    ///
    /// Kind: a registration of a source of the actor's own; accepted while the actor is stopping, and it then produces nothing. See [`signals`](crate::signals).
    ///
    /// The events reach `map` through a queue the crate keeps for this
    /// subscription, so the code that raises an event never waits for this
    /// actor and never runs `map`: when the queue (`Filter::queue`, 64 by
    /// default and at most 4096) is full the event is dropped and counted,
    /// and the next [`Delivery`] carries the count in `missed_before`. When
    /// the queue runs empty with drops not yet reported, `map` receives a
    /// [`SubscriptionNotice::Overflow`] notice with the count. `map` runs on
    /// a task of this actor's forwarder plane, in the order of the queue, and
    /// each message it returns waits for room in this actor's mailbox like a
    /// stream item.
    ///
    /// The events this actor itself is the subject of are never delivered to
    /// it. The subscription belongs to this incarnation of the actor: it ends
    /// when the actor stops or restarts, or at
    /// [`unsubscribe`](Self::unsubscribe).
    ///
    /// A `map` that panics ends the subscription: it is called once more with
    /// a [`SubscriptionNotice::MapPanicked`] notice, the panic is logged
    /// (with the `tracing` feature) naming the actor, and the actor goes on.
    /// A subscription from a context that is not part of a system's
    /// registry receives nothing.
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn subscribe<F>(&mut self, filter: Filter, map: F) -> SubscriptionId
    where
        F: FnMut(Delivery) -> A::Message + Send + 'static,
    {
        let Some(mode) = self.stop_lane().abort_record().mode().cloned() else {
            return SubscriptionId(0);
        };
        let watcher = crate::telemetry::supervisor_ref(
            &mode,
            self.stop_lane().abort_record(),
            &self.actor_id,
        );
        let id = crate::telemetry::subscription::next_id(&mode);
        let (queue, rx) = crate::telemetry::subscription::SubQueue::new(filter.queue);
        let kinds = filter.kinds;
        let token = CancellationToken::new();
        let guard = crate::telemetry::subscription::SubscriptionEntryGuard::new(mode.clone(), id);
        crate::telemetry::subscription::register(
            &mode,
            crate::telemetry::subscription::SubEntry {
                id,
                watcher,
                filter,
                queue: queue.clone(),
            },
        );
        crate::telemetry::subscription::start_pump_for(&mode, kinds);
        let slot = PanicSlot::default();
        let fut = subscription_forward::<A, F>(
            guard,
            rx,
            queue,
            map,
            self.self_handle.clone(),
            token.clone(),
            slot.clone(),
        );
        self.spawn_forwarder(ForwarderKind::Subscription(id), fut, slot);
        self.ex_mut().subscriptions.insert(id, token);
        id
    }

    /// Ends the subscription `id`: once the call returns nothing more is put
    /// in the mailbox for it, except a message that a worker was already
    /// putting there at that instant (multi-thread runtimes). Returns `false`
    /// for an id this actor does not hold.
    ///
    /// Kind: a revoke, answers `bool`. See [`signals`](crate::signals).
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn unsubscribe(&mut self, id: SubscriptionId) -> bool {
        let Some(token) = self.ex_mut().subscriptions.remove(&id) else {
            return false;
        };
        token.cancel();
        crate::telemetry::with_mode(
            self.stop_lane().abort_record(),
            self.system_in_hand(),
            |mode| {
                crate::telemetry::subscription::unregister(mode, id);
            },
        );
        true
    }
}

/// The forwarder of one event subscription: takes the events the crate queued
/// for it, maps each into a message of the actor and puts it in the
/// mailbox, and reports the events the queue had to drop.
///
/// `entry` is a parameter, so it drops with the future on every path (the
/// future finishing, a cancel, the end's abort, and a future that was never
/// polled) and removes the subscription from the registry then.
async fn subscription_forward<A, F>(
    entry: crate::telemetry::subscription::SubscriptionEntryGuard,
    mut rx: mpsc::Receiver<crate::telemetry::subscription::QueueItem>,
    queue: Arc<crate::telemetry::subscription::SubQueue>,
    map: F,
    handle: ActorHandle<A>,
    token: CancellationToken,
    slot: PanicSlot,
) -> Released
where
    A: Actor,
    F: FnMut(Delivery) -> A::Message + Send + 'static,
{
    use crate::telemetry::subscription::QueueItem;

    let _entry = entry;
    // The first local of the body: when the future is dropped (the end's
    // abort), the message and the send in flight, which live in inner
    // scopes, drop before it, so the map's drop comes last. Keep it first.
    let mut map = ReleaseGuard::new(map, handle.owner(), slot);
    let closed = loop {
        let item = tokio::select! {
            biased;
            _ = token.cancelled() => return Released::default(),
            // The actor stopped; once the mailbox is closed the map is never
            // called again.
            () = handle.mailbox_close() => break true,
            item = rx.recv() => item,
        };
        let Some(item) = item else { break false };
        match item {
            QueueItem::Event {
                event,
                missed_before,
            } => {
                let delivery = Delivery {
                    event,
                    missed_before,
                };
                if let Err(ended) = map_and_send(&mut map, delivery, &handle, &token).await {
                    return ended;
                }
            }
            QueueItem::Wake => queue.wake_taken(),
        }
        // The queue ran empty: report what it had to drop.
        if rx.is_empty() {
            let missed = queue.take_dropped();
            if missed > 0 {
                let notice = Delivery {
                    event: Arc::new(TelemetryEvent::Notice(SubscriptionNotice::Overflow {
                        missed,
                    })),
                    missed_before: 0,
                };
                if let Err(ended) = map_and_send(&mut map, notice, &handle, &token).await {
                    return ended;
                }
            }
        }
    };
    if closed || handle.mailbox_closed() {
        return Released::at_the_close(map.take());
    }
    Released {
        run: release(map.take()),
        end: None,
    }
}
