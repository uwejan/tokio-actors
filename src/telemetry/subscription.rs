//! Subscriptions: filters, queues and what a watcher receives.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use super::actor_ref::ActorRef;
use super::event::{EventKind, KindSet, TelemetryEvent};
use super::mode::Mode;
use super::pump::Pump;
use crate::types::ActorId;

/// The largest queue a subscription may ask for.
pub const MAX_SUBSCRIPTION_QUEUE: usize = 4096;

/// The queue length a subscription gets when it asks for none.
const DEFAULT_SUBSCRIPTION_QUEUE: usize = 64;

/// The id of one subscription.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionId(pub(crate) u64);

/// Whose events a subscription receives.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Scope {
    /// Every actor of the system, and the events that name no actor.
    System,
    /// The events about one actor id, across its incarnations.
    Actor(ActorId),
    /// The events about one actor id and every actor below it.
    Subtree(ActorId),
}

/// Which events a subscription receives, and how many it may queue.
#[derive(Clone, Debug)]
pub struct Filter {
    pub(crate) scope: Scope,
    pub(crate) kinds: KindSet,
    pub(crate) queue: usize,
}

impl Filter {
    /// A filter for `scope` with every cold kind and the default queue. A
    /// subscription never receives a hot kind, whatever its filter names:
    /// hot kinds go to handlers only.
    pub fn new(scope: Scope) -> Self {
        Self {
            scope,
            kinds: KindSet::cold(),
            queue: DEFAULT_SUBSCRIPTION_QUEUE,
        }
    }

    /// Limits the filter to `kinds`.
    pub fn kinds(mut self, kinds: KindSet) -> Self {
        self.kinds = kinds;
        self
    }

    /// Sets the queue length, clamped to `1..=MAX_SUBSCRIPTION_QUEUE`.
    pub fn queue(mut self, queue: usize) -> Self {
        self.queue = queue.clamp(1, MAX_SUBSCRIPTION_QUEUE);
        self
    }
}

/// One event handed to a watcher, with the count of events its queue lost
/// before this one.
#[derive(Clone, Debug)]
pub struct Delivery {
    /// The event.
    pub event: Arc<TelemetryEvent<'static>>,
    /// Events dropped from the queue since the previous delivery.
    pub missed_before: u64,
}

/// A notice a subscription receives about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionNotice {
    /// The queue overflowed and `missed` events were dropped.
    Overflow {
        /// The number of events dropped.
        missed: u64,
    },
    /// The watcher's map function panicked.
    MapPanicked,
}

/// One registered subscription.
#[derive(Clone, Debug)]
pub(crate) struct SubEntry {
    pub(crate) id: SubscriptionId,
    pub(crate) watcher: ActorRef,
    pub(crate) filter: Filter,
    pub(crate) queue: Arc<SubQueue>,
}

/// The queue of one subscription.
#[derive(Debug)]
pub(crate) struct SubQueue {
    pub(crate) tx: mpsc::Sender<QueueItem>,
    /// Events the full queue refused since the last one that went in or was
    /// reported.
    pub(crate) dropped: AtomicU64,
    /// Set while a wake marker sits in the queue or is being offered.
    wake_pending: AtomicBool,
}

/// One queued item.
#[derive(Debug)]
pub(crate) enum QueueItem {
    /// An event, with the count of events the queue lost before it.
    Event {
        event: Arc<TelemetryEvent<'static>>,
        missed_before: u64,
    },
    /// Tells the forwarder to look at the drop count: sent when an event was
    /// refused, so a count that arrives after the forwarder emptied the
    /// queue is not left unreported.
    Wake,
}

impl SubQueue {
    /// A queue of `capacity` items and the receiver that drains it.
    pub(crate) fn new(capacity: usize) -> (Arc<SubQueue>, mpsc::Receiver<QueueItem>) {
        let (tx, rx) = mpsc::channel(capacity);
        let queue = SubQueue {
            tx,
            dropped: AtomicU64::new(0),
            wake_pending: AtomicBool::new(false),
        };
        (Arc::new(queue), rx)
    }

    /// Takes the drop count: the events lost and not yet reported.
    ///
    /// The fence before the swap pairs with the one at the start of
    /// [`queue_marker_with`](Self::queue_marker_with), which every adder of
    /// the count runs right after its add. Of the two sequentially consistent
    /// fences one precedes the other in their single total order. If the
    /// adder's comes first, this swap reads the add. If this one comes first,
    /// what the forwarder did before it (the items it took, a marker flag it
    /// cleared) is seen by the adder's reads after its fence: the queue has
    /// room and the adder's marker goes in, or the flag reads clear and the
    /// adder sends one, or a later adder set the flag and its marker is on the
    /// way, after which the forwarder reaches this fence again and the first
    /// case holds. Acquire and release alone allow both sides to miss each
    /// other here, since each writes one location and reads another.
    pub(crate) fn take_dropped(&self) -> u64 {
        std::sync::atomic::fence(Ordering::SeqCst);
        self.dropped.swap(0, Ordering::AcqRel)
    }

    /// The forwarder took a wake marker out of the queue.
    pub(crate) fn wake_taken(&self) {
        self.wake_pending.store(false, Ordering::Release);
    }

    /// Offers `event`; never waits. A full queue drops it and counts it.
    fn offer(&self, event: &Arc<TelemetryEvent<'static>>) {
        let missed = self.dropped.swap(0, Ordering::AcqRel);
        let item = QueueItem::Event {
            event: event.clone(),
            missed_before: missed,
        };
        match self.tx.try_send(item) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(missed + 1, Ordering::AcqRel);
                self.queue_marker();
            }
            // The forwarder has ended; its entry is on its way out.
            Err(TrySendError::Closed(_)) => {
                if missed > 0 {
                    self.dropped.fetch_add(missed, Ordering::AcqRel);
                }
            }
        }
    }

    /// Queues a marker for the forwarder unless one is queued. A refused marker
    /// means the queue held items when it was refused, which the forwarder
    /// reads the count after; but the forwarder may have taken them since, and
    /// parked on an empty queue, so a refusal that leaves the queue empty is
    /// tried once more. A failed offer therefore always leaves a marker or a
    /// queue the forwarder has yet to read.
    fn queue_marker(&self) {
        self.queue_marker_with(|| {});
    }

    /// [`queue_marker`](Self::queue_marker), running `after_refusal` between a
    /// refused send and the flag's reset, which is where the forwarder can
    /// empty the queue.
    ///
    /// Every caller has just added to the drop count. The fence orders that
    /// add before the reads below; it pairs with the fence in
    /// [`take_dropped`](Self::take_dropped), so a count added here is either
    /// read by the forwarder's next swap or answered by a marker the forwarder
    /// will take.
    pub(crate) fn queue_marker_with(&self, after_refusal: impl FnOnce()) {
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.wake_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.tx.try_send(QueueItem::Wake).is_ok() {
            return;
        }
        after_refusal();
        self.wake_pending.store(false, Ordering::Release);
        if self.tx.capacity() == self.tx.max_capacity()
            && !self.wake_pending.swap(true, Ordering::AcqRel)
            && self.tx.try_send(QueueItem::Wake).is_err()
        {
            self.wake_pending.store(false, Ordering::Release);
        }
    }
}

/// Whether `entry` takes `event`: the scope matches the event's subject by
/// the ancestry the event carries, and the watcher's own events never do.
fn matches(entry: &SubEntry, event: &TelemetryEvent<'static>) -> bool {
    let subject = event.subject();
    if subject.is_some_and(|subject| *subject == entry.watcher) {
        return false;
    }
    match &entry.filter.scope {
        Scope::System => true,
        Scope::Actor(id) => subject.is_some_and(|subject| subject.id() == id),
        Scope::Subtree(root) => {
            subject.is_some_and(|subject| subject.id() == root || subject.is_descendant_of(root))
        }
    }
}

/// Offers `event` to every subscription in `subs` whose filter matches. Never
/// waits and runs no user code.
pub(crate) fn deliver(subs: &[SubEntry], kind: EventKind, event: &Arc<TelemetryEvent<'static>>) {
    for entry in subs {
        if entry.filter.kinds.contains(kind) && matches(entry, event) {
            entry.queue.offer(event);
        }
    }
}

/// Carries one event the pump dropped into the drop count of every
/// subscription whose filter would have taken it, so the watcher reads the
/// loss as `missed_before` of its next delivery. Takes the registry lock for a
/// moment, never waits for room and runs no user code.
pub(crate) fn carry_drop(mode: &Mode, kind: EventKind, event: &TelemetryEvent<'static>) {
    let subs = mode
        .sinks
        .tables
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .subs
        .clone();
    for entry in subs.iter() {
        if entry.filter.kinds.contains(kind) && matches(entry, event) {
            entry.queue.dropped.fetch_add(1, Ordering::AcqRel);
            entry.queue.queue_marker();
        }
    }
}

/// Adds `entry` to the registry and recomputes the gate. The entry's filter
/// keeps only its cold kinds: hot kinds are not delivered to subscriptions.
/// A registration that made the system resolve identities records every
/// registered actor's once the registry mutex is released, on the caller's
/// thread.
pub(crate) fn register(mode: &Mode, mut entry: SubEntry) {
    entry.filter.kinds = entry.filter.kinds.intersect(KindSet::cold());
    {
        let mut tables = mode
            .sinks
            .tables
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut subs: Vec<SubEntry> = tables.subs.iter().cloned().collect();
        subs.push(entry);
        tables.subs = Arc::from(subs);
        mode.recompute(&tables);
    }
    mode.engage_pass();
}

/// Removes subscription `id` from the registry and recomputes the gate; a
/// subscription that is not registered changes nothing.
pub(crate) fn unregister(mode: &Mode, id: SubscriptionId) {
    let mut tables = mode
        .sinks
        .tables
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !tables.subs.iter().any(|entry| entry.id == id) {
        return;
    }
    let subs: Vec<SubEntry> = tables
        .subs
        .iter()
        .filter(|entry| entry.id != id)
        .cloned()
        .collect();
    tables.subs = Arc::from(subs);
    mode.recompute(&tables);
}

/// Starts the pump when `kinds` includes a kind that reaches subscriptions
/// through it.
pub(crate) fn start_pump_for(mode: &Arc<Mode>, kinds: KindSet) {
    let pumped = KindSet::of(&[
        EventKind::KillStep,
        EventKind::HelperEnded,
        EventKind::ActorEnded,
        EventKind::SenderWaited,
        EventKind::ActorSpawned,
        EventKind::SpawnRefused,
        EventKind::ActorStarted,
        EventKind::RestartAttempt,
        EventKind::WaitCycle,
        EventKind::TurnStuck,
        EventKind::MailboxBacklog,
    ]);
    if kinds.bits() & pumped.bits() != 0 {
        Pump::ensure_started(mode);
    }
    start_sweep_for(mode, kinds);
}

/// Starts the sampling task of the system when `kinds` includes a kind that
/// task raises. Call it after the kinds are published, with no registry lock
/// held.
pub(crate) fn start_sweep_for(mode: &Arc<Mode>, kinds: KindSet) {
    let sampled = KindSet::of(&[EventKind::TurnStuck, EventKind::MailboxBacklog]);
    if kinds.bits() & sampled.bits() == 0 {
        return;
    }
    if let Some(system) = mode.system() {
        system.ensure_sweep();
    }
}

/// The next subscription id of `mode`'s system.
pub(crate) fn next_id(mode: &Mode) -> SubscriptionId {
    SubscriptionId(mode.sinks.next_sub.fetch_add(1, Ordering::Relaxed) + 1)
}

/// Removes its subscription from the registry when it drops, wherever the
/// forwarder that owns it ends.
#[derive(Debug)]
pub(crate) struct SubscriptionEntryGuard {
    mode: Arc<Mode>,
    id: SubscriptionId,
}

impl SubscriptionEntryGuard {
    pub(crate) fn new(mode: Arc<Mode>, id: SubscriptionId) -> Self {
        Self { mode, id }
    }
}

impl Drop for SubscriptionEntryGuard {
    fn drop(&mut self) {
        unregister(&self.mode, self.id);
    }
}

#[cfg(test)]
impl Mode {
    /// The number of subscriptions registered.
    pub(crate) fn subscription_count(&self) -> usize {
        self.sinks
            .tables
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .subs
            .len()
    }

    /// The reference each registered subscription names its watcher by.
    pub(crate) fn subscription_watchers(&self) -> Vec<ActorRef> {
        self.sinks
            .tables
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .subs
            .iter()
            .map(|entry| entry.watcher.clone())
            .collect()
    }

    /// The kinds some sink takes, as class bits, for tests of the union.
    pub(crate) fn class_bits(&self) -> u64 {
        ALL_KINDS
            .iter()
            .filter(|kind| self.armed(**kind))
            .fold(0, |bits, kind| bits | kind.bit())
    }
}

#[cfg(test)]
const ALL_KINDS: [EventKind; 18] = [
    EventKind::StopRequested,
    EventKind::ActorEnded,
    EventKind::ChildDied,
    EventKind::RestartBudget,
    EventKind::Unresponsive,
    EventKind::KillStep,
    EventKind::HelperEnded,
    EventKind::HandlerDetached,
    EventKind::Dropped,
    EventKind::TurnFinished,
    EventKind::RunFinished,
    EventKind::TimerTick,
    EventKind::SenderWaited,
    EventKind::ActorSpawned,
    EventKind::SpawnRefused,
    EventKind::ActorStarted,
    EventKind::RestartAttempt,
    EventKind::WaitCycle,
];
