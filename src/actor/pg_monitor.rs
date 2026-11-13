//! Group and scope monitors.
//!
//! An actor that calls
//! [`monitor_group`](crate::actor::context::ActorContext::monitor_group) or
//! [`monitor_scope`](crate::actor::context::ActorContext::monitor_scope) is
//! told, with a [`GroupChange`], each time the group or the scope changes. This
//! module holds the pieces behind it:
//!
//! - the subscription list inside a scope's table, where a join or a leave
//!   appends a change to the queue of each subscription it concerns, under the
//!   scope's lock, and records which entry tasks to wake after the lock is
//!   released;
//! - the entry task of each monitor, which waits for a change, reserves a slot
//!   in the watcher's mailbox, makes the watcher's message from the change and
//!   puts it in the mailbox;
//! - the watcher's table of the monitors it holds.
//!
//! A subscription queue holds as many changes as the watcher's mailbox. A
//! change that finds it full is not kept: the subscription is marked, and when
//! the queue has been delivered the entry task builds one
//! [`Resync`](GroupChange::Resync) from the table as it is then. A spell of
//! overflow therefore costs one copy of the watched members, however many
//! changes it dropped, and the copy is made outside the scope's lock.

use std::collections::{BTreeMap, VecDeque};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::actor::context::{ActorContext, Released};
use crate::actor::handle::ActorHandle;
use crate::actor::monitor::MonitorShared;
use crate::actor::panic::{catch_sync, payload_into_string};
use crate::actor::pg::{peers_of, PgScope, PgScopeInner, ScopeTable, Slot};
use crate::actor::runtime::{end_value, log_late_drop};
use crate::actor::subtree::{AbortRecord, Coupling};
use crate::actor::Actor;
use crate::error::{ActorError, MonitorError};
use crate::types::{ActorStatus, GroupChange, MonitorId, PeerRef, SignalTag, StopReason};

/// Each group a monitor covers with its members, shared with the table: what a
/// `Resync` and the answer of a new monitor are made from after the scope's
/// lock is released.
pub(crate) type Taken = Vec<(Arc<str>, Arc<Vec<Slot>>)>;

/// The function that turns a change into one of the watcher's messages.
pub(crate) type MapFn<M> = dyn Fn(GroupChange) -> M + Send + Sync + 'static;

/// The entry tasks to wake once a change has been recorded and the scope's
/// lock released.
#[derive(Default)]
pub(crate) struct Wakes(Vec<Arc<Notify>>);

impl Wakes {
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }

    fn push(&mut self, notify: &Arc<Notify>) {
        self.0.push(notify.clone());
    }

    pub(crate) fn into_inner(self) -> Vec<Arc<Notify>> {
        self.0
    }
}

/// What a subscription watches.
#[derive(Clone)]
pub(crate) enum SubKind {
    /// One group.
    Group(Arc<str>),
    /// Every group of the scope.
    Scope,
}

enum PendingKind {
    Join,
    Leave,
}

/// A change recorded in a subscription's queue, not yet made into a message.
pub(crate) struct Pending {
    kind: PendingKind,
    group: Arc<str>,
    peer: PeerRef,
}

struct Subscription {
    kind: SubKind,
    queue: VecDeque<Pending>,
    capacity: usize,
    /// Set when a change found the queue full, or arrived while the flag was
    /// already set; cleared by the entry task as it takes the snapshot the
    /// `Resync` is made from.
    overflowed: bool,
    wake: Arc<Notify>,
}

/// The subscriptions of one scope.
#[derive(Default)]
pub(crate) struct SubscriptionList {
    subs: BTreeMap<MonitorId, Subscription>,
    by_group: std::collections::HashMap<Arc<str>, Vec<MonitorId>>,
    scope: Vec<MonitorId>,
}

impl SubscriptionList {
    pub(crate) fn publish_join(&mut self, group: &Arc<str>, peer: &PeerRef, wakes: &mut Wakes) {
        self.publish(PendingKind::Join, group, peer, wakes);
    }

    pub(crate) fn publish_leave(&mut self, group: &Arc<str>, peer: &PeerRef, wakes: &mut Wakes) {
        self.publish(PendingKind::Leave, group, peer, wakes);
    }

    fn publish(&mut self, kind: PendingKind, group: &Arc<str>, peer: &PeerRef, wakes: &mut Wakes) {
        if self.subs.is_empty() {
            return;
        }
        let watching = self.by_group.get(group).map(Vec::as_slice).unwrap_or(&[]);
        for id in watching.iter().chain(self.scope.iter()) {
            let Some(sub) = self.subs.get_mut(id) else {
                continue;
            };
            if sub.overflowed {
                // The `Resync` the entry task owes already covers this
                // change, and one made now would be delivered before it.
                continue;
            }
            if sub.queue.len() >= sub.capacity {
                sub.overflowed = true;
            } else {
                sub.queue.push_back(Pending {
                    kind: match kind {
                        PendingKind::Join => PendingKind::Join,
                        PendingKind::Leave => PendingKind::Leave,
                    },
                    group: group.clone(),
                    peer: peer.clone(),
                });
            }
            wakes.push(&sub.wake);
        }
    }

    fn subscribe(&mut self, id: MonitorId, kind: SubKind, capacity: usize) -> Arc<Notify> {
        let wake = Arc::new(Notify::new());
        match &kind {
            SubKind::Group(group) => self.by_group.entry(group.clone()).or_default().push(id),
            SubKind::Scope => self.scope.push(id),
        }
        self.subs.insert(
            id,
            Subscription {
                kind,
                queue: VecDeque::new(),
                capacity,
                overflowed: false,
                wake: wake.clone(),
            },
        );
        wake
    }

    /// Ends a subscription; the changes it still queued are dropped.
    pub(crate) fn unsubscribe(&mut self, id: MonitorId) {
        let Some(sub) = self.subs.remove(&id) else {
            return;
        };
        match sub.kind {
            SubKind::Group(group) => {
                if let Some(ids) = self.by_group.get_mut(&group) {
                    ids.retain(|other| *other != id);
                    if ids.is_empty() {
                        self.by_group.remove(&group);
                    }
                }
            }
            SubKind::Scope => self.scope.retain(|other| *other != id),
        }
    }

    /// Whether the subscription has a change or a `Resync` to deliver; `None`
    /// when it has ended.
    pub(crate) fn has_work(&self, id: MonitorId) -> Option<bool> {
        let sub = self.subs.get(&id)?;
        Some(!sub.queue.is_empty() || sub.overflowed)
    }

    /// How many subscriptions the list holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.subs.len()
    }

    /// The changes queued for a subscription, and whether it is marked.
    #[cfg(test)]
    pub(crate) fn state_for_test(&self, id: MonitorId) -> Option<(usize, bool)> {
        let sub = self.subs.get(&id)?;
        Some((sub.queue.len(), sub.overflowed))
    }
}

/// What taking the next delivery from a subscription found.
pub(crate) enum Take {
    /// The subscription has ended.
    Gone,
    /// Nothing is queued and no `Resync` is owed.
    Idle,
    /// The next change in the queue.
    Change(Pending),
    /// A `Resync` is owed: the members of the groups it covers, as they are
    /// now, shared with the table (the copy into the message is made after
    /// the lock is released).
    Resync(Taken),
}

impl ScopeTable {
    /// Takes the next delivery of the subscription: the oldest queued change,
    /// or when the queue is empty and the subscription is marked, the marker
    /// cleared and the members to build the `Resync` from. The marker is
    /// cleared and the members read in this one critical section, so a change
    /// recorded after it follows the `Resync` and one recorded before it is
    /// in the members.
    pub(crate) fn take(&mut self, id: MonitorId) -> Take {
        let Some(sub) = self.subs.subs.get_mut(&id) else {
            return Take::Gone;
        };
        if let Some(change) = sub.queue.pop_front() {
            return Take::Change(change);
        }
        if !sub.overflowed {
            return Take::Idle;
        }
        sub.overflowed = false;
        match sub.kind.clone() {
            SubKind::Group(group) => {
                let slots = self
                    .groups
                    .get(&*group)
                    .map(|members| members.snapshot())
                    .unwrap_or_default();
                Take::Resync(vec![(group, slots)])
            }
            SubKind::Scope => Take::Resync(
                self.groups
                    .iter()
                    .map(|(name, members)| (name.clone(), members.snapshot()))
                    .collect(),
            ),
        }
    }
}

impl PgScopeInner {
    /// Subscribes to `kind` and reads what it watches, in one critical
    /// section: the changes the subscription queues are exactly those after
    /// the members returned.
    pub(crate) fn subscribe(
        &self,
        id: MonitorId,
        kind: SubKind,
        capacity: usize,
    ) -> (Arc<Notify>, Taken) {
        let mut table = self.lock();
        let initial = match &kind {
            SubKind::Group(group) => {
                let slots = table
                    .groups
                    .get(&**group)
                    .map(|members| members.snapshot())
                    .unwrap_or_default();
                vec![(group.clone(), slots)]
            }
            SubKind::Scope => table
                .groups
                .iter()
                .map(|(name, members)| (name.clone(), members.snapshot()))
                .collect(),
        };
        let wake = table.subs.subscribe(id, kind, capacity);
        (wake, initial)
    }
}

/// The word of bits that [`Counts`] keeps its landed count in: the top bit is
/// set once the monitor is closed.
const CLOSED: u64 = 1 << 63;

/// The counts a monitor keeps of the changes it put in the watcher's mailbox,
/// so `demonitor` can say whether one is still queued.
///
/// One word holds the closed bit and the number of changes that landed, so a
/// landing and a close are decided by one atomic step: a change either lands
/// before the close, and is counted by it, or finds the monitor closed and is
/// never put in the mailbox. Nothing is counted and taken back.
#[derive(Default)]
pub(crate) struct Counts {
    word: AtomicU64,
    dispatched: AtomicU64,
}

impl Counts {
    /// Counts one change as landing; false when the monitor is closed, and the
    /// change must not be put in the mailbox.
    fn land(&self) -> bool {
        let mut seen = self.word.load(Ordering::SeqCst);
        loop {
            if seen & CLOSED != 0 {
                return false;
            }
            match self.word.compare_exchange_weak(
                seen,
                seen + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(now) => seen = now,
            }
        }
    }

    /// Takes back a landing whose change the closed mailbox refused.
    fn unland(&self) {
        self.word.fetch_sub(1, Ordering::SeqCst);
    }

    /// Closes the monitor, for good, and answers how many changes had landed
    /// by then.
    fn close(&self) -> u64 {
        self.word.fetch_or(CLOSED, Ordering::SeqCst) & !CLOSED
    }
}

/// The watcher's drop of the closure that makes its messages. The closure is
/// the user's, so its drop can panic: it is dropped inside a catch, and the
/// panic is logged and goes no further.
struct MapHold<M> {
    map: Option<Arc<MapFn<M>>>,
    owner: crate::actor::runtime::Owner,
}

impl<M> MapHold<M> {
    fn map(&self) -> &MapFn<M> {
        self.map.as_deref().expect("held until the guard drops")
    }
}

impl<M> Drop for MapHold<M> {
    fn drop(&mut self) {
        if let Some(map) = self.map.take() {
            if let Some(panic) = end_value(map) {
                log_late_drop(&panic, &self.owner);
            }
        }
    }
}

/// What a monitor's entry task needs.
pub(crate) struct GroupTask<M> {
    scope: Arc<PgScopeInner>,
    id: MonitorId,
    shared: Arc<MonitorShared>,
    counts: Arc<Counts>,
    wake: Arc<Notify>,
    map: MapHold<M>,
}

/// One group or scope monitor in its watcher's table.
pub(crate) struct GroupEntry {
    shared: Arc<MonitorShared>,
    token: CancellationToken,
    scope: Arc<PgScopeInner>,
    counts: Arc<Counts>,
    /// The watcher's record, which shows how many monitors it holds.
    record: AbortRecord,
    /// The monitors the watcher holds, shared by its entries.
    live: Arc<AtomicU32>,
}

impl GroupEntry {
    /// An entry for a monitor the watcher holds until the entry is dropped.
    fn new(
        shared: Arc<MonitorShared>,
        token: CancellationToken,
        scope: Arc<PgScopeInner>,
        counts: Arc<Counts>,
        record: AbortRecord,
        live: Arc<AtomicU32>,
    ) -> Self {
        let held = live.fetch_add(1, Ordering::Relaxed) + 1;
        record.set_coupling(Coupling::GroupMonitor, held);
        Self {
            shared,
            token,
            scope,
            counts,
            record,
            live,
        }
    }
}

impl Drop for GroupEntry {
    fn drop(&mut self) {
        // Closed before the task is cancelled, so a change that has not
        // landed yet never does.
        self.counts.close();
        self.token.cancel();
        let id = self.shared.id();
        self.scope.lock().subs.unsubscribe(id);
        let held = self.live.fetch_sub(1, Ordering::Relaxed) - 1;
        self.record.set_coupling(Coupling::GroupMonitor, held);
    }
}

/// The group and scope monitors an actor holds. Empty until the first one:
/// an actor without any holds one null pointer.
pub(crate) struct GroupTable<A: Actor> {
    state: Option<Box<GroupState>>,
    _actor: PhantomData<fn() -> A>,
}

/// The monitors, allocated with the first one.
#[derive(Default)]
struct GroupState {
    entries: BTreeMap<MonitorId, GroupEntry>,
    /// How many entries are held, for the entries to count and show.
    live: Arc<AtomicU32>,
}

impl<A: Actor> Default for GroupTable<A> {
    fn default() -> Self {
        Self {
            state: None,
            _actor: PhantomData,
        }
    }
}

impl<A: Actor> GroupTable<A> {
    /// The monitors held and not yet revoked.
    pub(crate) fn len(&self) -> usize {
        self.state.as_ref().map_or(0, |state| state.entries.len())
    }

    pub(crate) fn contains(&self, id: MonitorId) -> bool {
        self.state
            .as_ref()
            .is_some_and(|state| state.entries.contains_key(&id))
    }

    /// The count the entries of this table share.
    fn live(&mut self) -> Arc<AtomicU32> {
        self.state.get_or_insert_with(Default::default).live.clone()
    }

    fn insert(&mut self, id: MonitorId, entry: GroupEntry) {
        self.state
            .get_or_insert_with(Default::default)
            .entries
            .insert(id, entry);
    }

    /// Removes a monitor; dropping the entry ends its subscription.
    pub(crate) fn remove(&mut self, id: MonitorId) {
        if let Some(state) = self.state.as_mut() {
            state.entries.remove(&id);
        }
    }

    /// Whether the monitor the tag names is held and its changes are not
    /// flushed.
    #[allow(dead_code)]
    pub(crate) fn wants(&self, tag: &SignalTag) -> bool {
        !tag.shared.is_flushed() && self.contains(tag.shared.id())
    }

    /// A change of the tag's monitor was dispatched.
    pub(crate) fn finish(&mut self, tag: &SignalTag) {
        if let Some(entry) = self
            .state
            .as_ref()
            .and_then(|state| state.entries.get(&tag.shared.id()))
        {
            entry.counts.dispatched.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Ends a monitor. `None` when `id` is not a group or scope monitor of
    /// this actor. Otherwise true means no change of it is queued and none
    /// will be delivered; false means one is queued: with `flush` it is
    /// dropped when the watcher reaches it, without it, it is delivered. The
    /// answer is read from one word that a landing change and this call
    /// decide between them, so a change either landed before the call and
    /// counts, or finds the monitor closed and is never put in the mailbox.
    /// Changes still in the subscription's own queue, not yet in the mailbox,
    /// and a change the entry task holds but has not landed are dropped either
    /// way and count as none.
    pub(crate) fn demonitor(&mut self, id: MonitorId, flush: bool) -> Option<bool> {
        let entry = self.state.as_mut()?.entries.remove(&id)?;
        let landed = entry.counts.close();
        if flush {
            entry.shared.flush();
        }
        let dispatched = entry.counts.dispatched.load(Ordering::SeqCst);
        Some(landed <= dispatched)
    }

    /// Ends every monitor.
    pub(crate) fn clear(&mut self) {
        self.state = None;
    }
}

/// The change a queued delivery stands for.
fn change_of(monitor: MonitorId, pending: Pending) -> GroupChange {
    let Pending { kind, group, peer } = pending;
    match kind {
        PendingKind::Join => GroupChange::Join {
            monitor,
            group,
            members: vec![peer],
        },
        PendingKind::Leave => GroupChange::Leave {
            monitor,
            group,
            members: vec![peer],
        },
    }
}

/// Builds a `Resync` from the members taken under the lock. Runs after the
/// lock is released.
fn resync_of(scope: &PgScopeInner, monitor: MonitorId, groups: Taken) -> GroupChange {
    #[cfg(test)]
    scope.resync_built();
    #[cfg(not(test))]
    let _ = scope;
    GroupChange::Resync {
        monitor,
        groups: groups
            .into_iter()
            .map(|(name, slots)| (name, peers_of(&slots)))
            .collect(),
    }
}

/// The entry task: waits for the subscription to have something to deliver,
/// takes a slot of the watcher's mailbox, makes the watcher's message and puts
/// it in the mailbox, one change at a time and in order.
///
/// The change enters through an ordinary mailbox slot, after the messages
/// queued before it. A full mailbox makes the task wait for room, while the
/// subscription keeps queuing up to its capacity and then marks itself. The
/// task ends with the subscription, a revoke, or the watcher's mailbox.
pub(crate) async fn group_forward<A: Actor>(
    handle: ActorHandle<A>,
    task: GroupTask<A::Message>,
    token: CancellationToken,
) -> Released {
    let GroupTask {
        scope,
        id,
        shared,
        counts,
        wake,
        map,
    } = task;
    loop {
        loop {
            let work = scope.lock().subs.has_work(id);
            match work {
                None => return Released::default(),
                Some(true) => break,
                Some(false) => {}
            }
            tokio::select! {
                biased;
                _ = token.cancelled() => return Released::default(),
                () = handle.mailbox_close() => return Released::default(),
                () = wake.notified() => {}
            }
        }
        // A slot first, the change after: the change is taken from the queue
        // only once there is room to put it, so the queue's bound holds the
        // changes the watcher has not room for.
        let permit = tokio::select! {
            biased;
            _ = token.cancelled() => return Released::default(),
            permit = handle.reserve_signal(false) => permit,
        };
        let Some(permit) = permit else {
            return Released::default();
        };
        let taken = scope.lock().take(id);
        let change = match taken {
            Take::Gone => return Released::default(),
            Take::Idle => {
                drop(permit);
                continue;
            }
            Take::Change(pending) => change_of(id, pending),
            Take::Resync(groups) => resync_of(&scope, id, groups),
        };
        match catch_sync(|| (map.map())(change)).map_err(payload_into_string) {
            Ok(message) => {
                if !counts.land() {
                    drop(permit);
                    return Released {
                        run: end_value(message),
                        end: None,
                    };
                }
                let tag = SignalTag {
                    shared: shared.clone(),
                };
                if !permit.send_signal(tag, message) {
                    // The mailbox closed and dropped the change with it.
                    counts.unland();
                    return Released::default();
                }
            }
            Err(panic) => {
                drop(permit);
                // Kept before the raise: a panic that lands after the
                // watcher's message loop read its stop lane for the last
                // time raises where no reader is left, and the stop reads
                // the panic from the incarnation's record.
                handle
                    .stop_lane_ref()
                    .abort_record()
                    .keep_map_panic(panic.clone());
                shared.spend();
                #[cfg(feature = "tracing")]
                caught_log!(warn,
                    actor_id = %handle.id(),
                    panic = %handle.log_fields().panic_text(&panic),
                    "a group monitor's map function panicked; the actor fails with that panic"
                );
                let _ = crate::system::raise_noted(
                    None,
                    handle.stop_lane_ref(),
                    StopReason::Failure(ActorError::Panic(panic.clone())),
                    crate::telemetry::StopOrigin::MapPanic,
                );
                return Released {
                    run: Some(panic),
                    end: None,
                };
            }
        }
    }
}

impl<A: Actor> ActorContext<A> {
    /// Watches one process group: the actor is told with a
    /// [`GroupChange`], made by `map`, each time a member joins or leaves
    /// `group` (a member that ends leaves first, so a change names it before
    /// it reads `Stopped`). Returns the monitor's id and the group's members
    /// at the instant the monitor began, so the changes that follow are
    /// exactly the ones after the list.
    ///
    /// A monitor delivers until it is revoked with
    /// [`demonitor`](Self::demonitor) (queued changes stay) or
    /// [`demonitor_flush`](Self::demonitor_flush) (queued changes are
    /// dropped), and ends with the actor, which drops the changes it had not
    /// yet handled. Both answer `true` when no change of the monitor is queued
    /// or will be delivered, and `false` when one is: with `demonitor` it is
    /// delivered, with `demonitor_flush` it is dropped. A change that was
    /// being made when the call came is not delivered and counts as none. The
    /// one exception is the watcher's own stop: a change that was already in
    /// its mailbox when the stop began, or that reaches it while the stop
    /// runs, is not delivered, and the answer may count it as queued. The
    /// monitor keeps a queue as long as the actor's mailbox:
    /// when the actor handles changes slower than the group makes them and
    /// the queue is full, the changes that do not fit are not kept and one
    /// [`Resync`](GroupChange::Resync) follows the queue, holding the group as
    /// it is then. A [`Leave`](GroupChange::Leave) and a
    /// [`Down`](crate::types::Down) of the same member are two notices from
    /// two sources, and their order is not specified.
    ///
    /// # Errors
    /// [`MonitorError::Stopping`] when the actor is stopping or has stopped.
    ///
    /// Erlang/OTP: `pg:monitor/2`, which answers `{Ref, Members}` and then
    /// sends `{Ref, join | leave, Group, Pids}`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, ActorSystem, GroupChange,
    /// };
    /// use tokio::sync::mpsc;
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Watcher {
    ///     system: std::sync::Arc<ActorSystem>,
    ///     seen: mpsc::UnboundedSender<GroupChange>,
    /// }
    ///
    /// impl Actor for Watcher {
    ///     type Message = GroupChange;
    ///     type Response = ();
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let scope = self.system.pg();
    ///         let (_id, members) = ctx
    ///             .monitor_group(&scope, "workers", |change| change)
    ///             .expect("the watcher is running");
    ///         assert!(members.is_empty());
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(
    ///         &mut self,
    ///         change: GroupChange,
    ///         _: &mut ActorContext<Self>,
    ///     ) -> ActorResult<()> {
    ///         let _ = self.seen.send(change);
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let system = ActorSystem::create("monitor-group-example")?;
    /// let (seen, mut changes) = mpsc::unbounded_channel();
    /// let _watcher = Watcher { system: system.clone(), seen }
    ///     .spawn()
    ///     .on_system(&system)
    ///     .await?;
    /// let worker = Worker.spawn().on_system(&system).await?;
    /// system.pg().join("workers", &worker)?;
    /// match changes.recv().await {
    ///     Some(GroupChange::Join { group, members, .. }) => {
    ///         assert_eq!(&*group, "workers");
    ///         assert_eq!(members[0].actor_id, *worker.id());
    ///     }
    ///     other => panic!("expected a join, got {other:?}"),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn monitor_group<F>(
        &mut self,
        scope: &PgScope,
        group: impl Into<Arc<str>>,
        map: F,
    ) -> Result<(MonitorId, Vec<PeerRef>), MonitorError>
    where
        F: Fn(GroupChange) -> A::Message + Send + Sync + 'static,
    {
        let group = group.into();
        let (id, mut initial) = self.register_group(scope, SubKind::Group(group), map)?;
        let members = initial
            .pop()
            .map(|(_, slots)| peers_of(&slots))
            .unwrap_or_default();
        Ok((id, members))
    }

    /// Watches a whole process-group scope: the actor is told with a
    /// [`GroupChange`], made by `map`, each time any member joins or leaves
    /// any group of the scope. Returns the monitor's id and every group of
    /// the scope with its members at the instant the monitor began.
    ///
    /// The queue, the overflow, the revoke and the end behave as for
    /// [`monitor_group`](Self::monitor_group); a
    /// [`Resync`](GroupChange::Resync) of a scope monitor lists every group of
    /// the scope that has a member.
    ///
    /// # Errors
    /// [`MonitorError::Stopping`] when the actor is stopping or has stopped.
    ///
    /// Erlang/OTP: `pg:monitor_scope/1`, which answers `{Ref, #{Group => Pids}}`
    /// and then sends the same notifications as `pg:monitor/2`.
    #[allow(clippy::type_complexity)]
    pub fn monitor_scope<F>(
        &mut self,
        scope: &PgScope,
        map: F,
    ) -> Result<(MonitorId, Vec<(Arc<str>, Vec<PeerRef>)>), MonitorError>
    where
        F: Fn(GroupChange) -> A::Message + Send + Sync + 'static,
    {
        let (id, initial) = self.register_group(scope, SubKind::Scope, map)?;
        let groups = initial
            .into_iter()
            .map(|(name, slots)| (name, peers_of(&slots)))
            .collect();
        Ok((id, groups))
    }

    /// Subscribes, records the monitor and starts its task.
    fn register_group<F>(
        &mut self,
        scope: &PgScope,
        kind: SubKind,
        map: F,
    ) -> Result<(MonitorId, Taken), MonitorError>
    where
        F: Fn(GroupChange) -> A::Message + Send + Sync + 'static,
    {
        if matches!(self.status(), ActorStatus::Stopping | ActorStatus::Stopped) {
            return Err(MonitorError::Stopping);
        }
        let id = MonitorId::next();
        let handle = self.self_handle();
        let capacity = handle.mailbox_capacity().max(1);
        let inner = scope.inner().clone();
        let (wake, initial) = inner.subscribe(id, kind, capacity);
        let shared = Arc::new(MonitorShared::repeating(id));
        let counts = Arc::new(Counts::default());
        let live = self.groups_mut().live();
        let record = handle.stop_lane_ref().abort_record().clone();
        let token = CancellationToken::new();
        let owner = self.owner();
        let map: Arc<MapFn<A::Message>> = Arc::new(map);
        let task = GroupTask {
            scope: inner.clone(),
            id,
            shared: shared.clone(),
            counts: counts.clone(),
            wake,
            map: MapHold {
                map: Some(map),
                owner,
            },
        };
        self.groups_mut().insert(
            id,
            GroupEntry::new(shared, token.clone(), inner, counts, record, live),
        );
        self.spawn_group_forwarder(id, group_forward(handle, task, token));
        Ok((id, initial))
    }
}
