//! Peer monitors.
//!
//! An actor that calls [`monitor`](crate::actor::context::ActorContext::monitor)
//! gets a notice, a [`Down`], when the actor it names has ended. This module
//! holds the pieces behind it:
//!
//! - the state cell every monitor has, which makes one notice and one revoke
//!   agree on who wins;
//! - the entry the watcher keeps for each monitor, and the task that waits on
//!   the watched actor's status plane and puts the notice in the watcher's
//!   mailbox;
//! - the targets a monitor accepts;
//! - the reads the stop-time stage uses to deliver the notices an ending
//!   watcher is still owed.
//!
//! A monitor is bound to one run of the watched actor. The task holds the
//! watched run's status receiver only, never a handle to it, so a monitor
//! keeps nothing of the watched actor alive.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::actor::context::Released;
use crate::actor::handle::{ActorHandle, ForwardPermit};
use crate::actor::panic::{catch_sync, payload_into_string};
use crate::actor::runtime::{end_value, log_late_drop};
use crate::actor::subtree::AbortRecord;
use crate::actor::{Actor, ActorEnvelope};
use crate::error::ActorError;
use crate::system::{wait_final_reason, ActorSystem, MonitorKey, ResolvedTarget};
use crate::types::{
    ActorId, ActorStatus, Down, DownReason, Incarnation, MonitorId, SignalTag, StatusRx, StopReason,
};

/// The notice has not been made, sent or revoked.
const PENDING: u8 = 0;
/// The task took the notice: it is in the queue or is being put there.
const SENT: u8 = 1;
/// The watcher revoked the monitor before the notice was taken.
const CANCELLED: u8 = 2;
/// The watcher revoked the monitor after the notice was taken, and asked for
/// the queued notice to be dropped.
const FLUSHED: u8 = 3;
/// The stop-time stage took the monitor: no task puts a notice in the
/// mailbox from here on, and the stage delivers the notice itself.
const CLOSED: u8 = 4;
/// The notice was taken and its map function panicked, so none exists.
const SPENT: u8 = 5;

/// The function that turns a notice into one of the watcher's messages.
pub(crate) type MapFn<M> = dyn Fn(Down) -> M + Send + Sync + 'static;

/// Decrements the watched run's `monitored_by` count exactly once, however
/// many of the paths that end a monitor reach it.
#[derive(Debug)]
pub(crate) struct MonitorCount {
    /// The watched run's block; `None` for a monitor that never had a live
    /// run to count on.
    record: Option<AbortRecord>,
    released: AtomicBool,
}

impl MonitorCount {
    /// A count on `record`, which gains one watcher now.
    fn on(record: AbortRecord) -> Self {
        record.add_monitored_by();
        Self {
            record: Some(record),
            released: AtomicBool::new(false),
        }
    }

    /// No count: the monitor had no live run to watch.
    fn none() -> Self {
        Self {
            record: None,
            released: AtomicBool::new(false),
        }
    }

    /// Gives the watcher back to the watched run, once.
    pub(crate) fn release(&self) {
        if let Some(record) = &self.record {
            if !self.released.swap(true, Ordering::AcqRel) {
                record.sub_monitored_by();
            }
        }
    }
}

impl Drop for MonitorCount {
    fn drop(&mut self) {
        self.release();
    }
}

/// The state every monitor shares between its entry in the watcher's map, its
/// task and the notice in the watcher's mailbox. One compare-exchange decides
/// who acts: the task that sends the notice, the watcher that revokes it, or
/// the stage that delivers it at the watcher's stop.
#[derive(Debug)]
pub(crate) struct MonitorShared {
    id: MonitorId,
    state: AtomicU8,
    count: MonitorCount,
}

impl MonitorShared {
    fn new(id: MonitorId, count: MonitorCount) -> Self {
        Self {
            id,
            state: AtomicU8::new(PENDING),
            count,
        }
    }

    /// The cell of a subscription that delivers many notices: it reads
    /// `Sent` from the start and never counts on a watched run, so only
    /// [`flush`](Self::flush) changes it, and [`is_flushed`](Self::is_flushed)
    /// tells the dispatch to drop what is queued.
    pub(crate) fn repeating(id: MonitorId) -> Self {
        Self {
            id,
            state: AtomicU8::new(SENT),
            count: MonitorCount::none(),
        }
    }

    fn swap(&self, from: u8, to: u8) -> bool {
        self.state
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The monitor this state belongs to.
    pub(crate) fn id(&self) -> MonitorId {
        self.id
    }

    /// The task takes the notice, from `Pending` to `Sent`. Only the winner
    /// puts a notice in the mailbox.
    pub(crate) fn take(&self) -> bool {
        self.swap(PENDING, SENT)
    }

    /// The stage takes a notice it closed, from `Closed` to `Sent`.
    pub(crate) fn take_closed(&self) -> bool {
        self.swap(CLOSED, SENT)
    }

    /// The stage claims a monitor nobody has taken: from here no task sends
    /// into the mailbox for it.
    pub(crate) fn claim_for_stage(&self) -> bool {
        self.swap(PENDING, CLOSED)
    }

    /// The notice did not land, so it is still owed: from `Sent` back to
    /// `Pending`.
    pub(crate) fn restore(&self) {
        self.swap(SENT, PENDING);
    }

    /// The map function panicked, so no notice exists: from `Sent` to
    /// `Spent`.
    pub(crate) fn spend(&self) {
        self.swap(SENT, SPENT);
    }

    /// The watcher revokes the monitor. True means no notice will be queued
    /// from here on.
    pub(crate) fn revoke(&self) -> bool {
        self.swap(PENDING, CANCELLED) || self.swap(CLOSED, CANCELLED)
    }

    /// As [`revoke`](Self::revoke), and a notice already taken is marked
    /// flushed so that it is dropped, not delivered, when it is dispatched.
    /// True means the revoke was in time; false means a notice was already
    /// queued and the flush dropped it.
    pub(crate) fn flush(&self) -> bool {
        if self.revoke() {
            return true;
        }
        self.swap(SENT, FLUSHED);
        false
    }

    /// Whether the watcher flushed the notice.
    pub(crate) fn is_flushed(&self) -> bool {
        self.state.load(Ordering::Acquire) == FLUSHED
    }

    /// Whether the notice was taken and its delivery is not yet known.
    pub(crate) fn is_sent(&self) -> bool {
        self.state.load(Ordering::Acquire) == SENT
    }

    /// The raw state, for the tests that pin the cell.
    #[cfg(test)]
    pub(crate) fn state_for_test(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }
}

/// One monitor in its watcher's map.
pub(crate) struct MonitorEntry<M> {
    shared: Arc<MonitorShared>,
    token: CancellationToken,
    /// The watched run's status receiver; `None` when nothing was registered
    /// under the key.
    plane: Option<StatusRx>,
    actor_id: ActorId,
    incarnation: Option<Incarnation>,
    /// True when the target was gone when the monitor was made, so the
    /// notice reads `NoProc`.
    noproc: bool,
    /// Held to deliver the notice at the watcher's stop; dropped inside a
    /// catch, because the closure is the user's.
    map: Option<Arc<MapFn<M>>>,
    /// The watcher whose closure `map` is, named by the log of its drop
    /// with the watcher's sensitivity.
    owner: crate::actor::runtime::Owner,
}

impl<M> Drop for MonitorEntry<M> {
    fn drop(&mut self) {
        self.token.cancel();
        self.shared.count.release();
        if let Some(map) = self.map.take() {
            if let Some(panic) = end_value(map) {
                log_late_drop(&panic, &self.owner);
            }
        }
    }
}

impl<M> MonitorEntry<M> {
    /// The notice this entry owes now, if its target has ended: the reason
    /// the watched run's plane carries, or `NoProc` for a target that was
    /// gone when the monitor was made.
    fn owed_reason(&self) -> Option<DownReason> {
        if self.noproc {
            return Some(DownReason::NoProc);
        }
        let plane = self.plane.as_ref()?;
        peek_final(plane).map(DownReason::Stopped)
    }

    fn down(&self, reason: DownReason) -> Down {
        Down {
            monitor: self.shared.id,
            actor_id: self.actor_id.clone(),
            incarnation: self.incarnation,
            reason,
        }
    }
}

/// The plane's final reason if it is written, else (the plane ended without
/// one) the reason a reader of an ended plane takes, the cause a `Kill` on the
/// stop lane carries or `Kill`, and `None` while the run goes on. One read;
/// nothing waits. It gives the reason `wait_final_reason` gives for the same
/// plane, so a monitor and a link to one run never carry two reasons.
pub(crate) fn peek_final(rx: &StatusRx) -> Option<StopReason> {
    let reason = rx.borrow().reason.clone();
    if reason.is_some() {
        return reason;
    }
    if rx.status() == ActorStatus::Stopped || rx.has_changed().is_err() {
        return Some(rx.reason_or_kill());
    }
    None
}

/// Runs the user's map function, catching a panic.
fn run_map<M>(map: &MapFn<M>, down: Down) -> Result<M, String> {
    catch_sync(|| map(down)).map_err(payload_into_string)
}

/// The monitors an actor holds, with the messages the stop-time stage took
/// from its queues while it looked for notices.
pub(crate) struct MonitorState<A: Actor> {
    pub(crate) owner: crate::actor::runtime::Owner,
    entries: BTreeMap<MonitorId, MonitorEntry<A::Message>>,
    /// The links this actor holds, and the exit signals it is delivering.
    pub(crate) links: crate::actor::link::LinkTable,
    /// The map the actor gave to `trap_exit`.
    pub(crate) trap: crate::actor::link::TrapCell<A::Message>,
    /// The group and scope monitors this actor holds. Their tasks are
    /// forwarders of the context.
    pub(crate) groups: crate::actor::pg_monitor::GroupTable<A>,
    /// The requests this actor has in flight.
    pub(crate) requests: crate::actor::request::RequestTable<A>,
    /// The receive timeout, the idle hook, the message to handle next and the
    /// aliases, in a box of their own made with the first use: an actor with
    /// only a monitor or a link holds one null pointer here and none of the
    /// message cell.
    turn: Option<Box<crate::actor::turn::TurnState<A>>>,
    /// Messages the stage took from the mailbox and the priority lane that
    /// are not notices. They are dropped with the context, one at a time,
    /// each drop caught, as the queue's other messages are.
    stash: Vec<ActorEnvelope<A>>,
    /// The block of the actor that holds the state, set by the first monitor:
    /// the count of monitors is recorded there for a snapshot to read.
    own: Option<crate::actor::subtree::AbortRecord>,
}

impl<A: Actor> Drop for MonitorState<A> {
    fn drop(&mut self) {
        for message in self.stash.drain(..) {
            if let Some(panic) = end_value(message) {
                log_late_drop(&panic, &self.owner);
            }
        }
    }
}

impl<A: Actor> MonitorState<A> {
    pub(crate) fn new(owner: crate::actor::runtime::Owner) -> Self {
        Self {
            entries: BTreeMap::new(),
            links: crate::actor::link::LinkTable::default(),
            trap: crate::actor::link::TrapCell::default(),
            groups: crate::actor::pg_monitor::GroupTable::default(),
            requests: crate::actor::request::RequestTable::default(),
            turn: None,
            stash: Vec::new(),
            own: None,
            owner,
        }
    }

    /// The turn state, if any was made.
    pub(crate) fn turn(&self) -> Option<&crate::actor::turn::TurnState<A>> {
        self.turn.as_deref()
    }

    /// The turn state, if any was made.
    pub(crate) fn turn_mut(&mut self) -> Option<&mut crate::actor::turn::TurnState<A>> {
        self.turn.as_deref_mut()
    }

    /// The turn state, made with the first use.
    pub(crate) fn turn_made(&mut self) -> &mut crate::actor::turn::TurnState<A> {
        let owner = &self.owner;
        self.turn
            .get_or_insert_with(|| Box::new(crate::actor::turn::TurnState::new(owner.clone())))
    }

    /// The monitors held and not yet delivered or revoked.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn contains(&self, id: MonitorId) -> bool {
        self.entries.contains_key(&id)
    }

    /// Names the block of the actor that holds the state, where the count of
    /// monitors is recorded; the first call is kept.
    pub(crate) fn bind(&mut self, own: &crate::actor::subtree::AbortRecord) {
        if self.own.is_none() {
            self.own = Some(own.clone());
        }
    }

    pub(crate) fn insert(&mut self, id: MonitorId, entry: MonitorEntry<A::Message>) {
        self.entries.insert(id, entry);
        self.note_monitors();
    }

    /// Records the number of monitors held in the actor's block, for a
    /// snapshot.
    fn note_monitors(&self) {
        if let Some(own) = &self.own {
            own.set_coupling(
                crate::actor::subtree::Coupling::Monitor,
                u32::try_from(self.entries.len()).unwrap_or(u32::MAX),
            );
        }
    }

    /// Removes a monitor; dropping the entry cancels its task and gives the
    /// watched run's count back.
    pub(crate) fn remove(&mut self, id: MonitorId) -> Option<MonitorEntry<A::Message>> {
        let entry = self.entries.remove(&id);
        if entry.is_some() {
            self.note_monitors();
        }
        entry
    }

    /// Revokes the monitor: true means no notice will be queued. A plain
    /// revoke that came too late, because the notice is already queued, keeps
    /// the entry: the notice is dispatched in the loop as it always was, and
    /// at a stop it is still owed, so the stage delivers it once. A flush
    /// drops the queued notice, so its entry goes with it.
    pub(crate) fn demonitor(&mut self, id: MonitorId, flush: bool) -> bool {
        let Some(entry) = self.entries.get(&id) else {
            return false;
        };
        let revoked = if flush {
            entry.shared.flush()
        } else {
            entry.shared.revoke()
        };
        if revoked || flush || !entry.shared.is_sent() {
            self.entries.remove(&id);
            self.note_monitors();
        }
        revoked
    }

    /// Removes every monitor and every link.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.note_monitors();
        self.links.clear();
        self.requests.clear();
    }

    /// Whether the stage must look in the queues: some monitor's notice was
    /// taken and has not been seen yet.
    pub(crate) fn in_flight(&self, seen: &HashSet<MonitorId>) -> bool {
        self.entries
            .iter()
            .any(|(id, entry)| entry.shared.is_sent() && !seen.contains(id))
            || self.links.in_flight(seen)
            || self.requests.in_flight(seen)
    }

    /// Whether a notice of this monitor or link is queued and wanted: it is
    /// held and its notice is not flushed.
    pub(crate) fn wants(&self, tag: &SignalTag) -> bool {
        !tag.shared.is_flushed()
            && (self.entries.contains_key(&tag.shared.id())
                || self.links.wants(tag)
                || self.requests.wants(tag))
    }

    /// Keeps a message the stage took from a queue until the context drops.
    pub(crate) fn stash(&mut self, message: ActorEnvelope<A>) {
        self.stash.push(message);
    }

    /// The stage closes every monitor nobody has taken. A closed monitor
    /// whose target has ended is owed; it is returned, in monitor order, with
    /// the reason to deliver. A closed monitor whose target goes on is not
    /// owed (a notice that would fire after the stage began is not owed) and
    /// is removed.
    pub(crate) fn close_pass(&mut self) -> Vec<(MonitorId, DownReason)> {
        self.requests.close_all();
        let mut owed = Vec::new();
        let mut not_owed = Vec::new();
        for (id, entry) in &self.entries {
            if !entry.shared.claim_for_stage() {
                continue;
            }
            match entry.owed_reason() {
                Some(reason) => owed.push((*id, reason)),
                None => not_owed.push(*id),
            }
        }
        let removed = !not_owed.is_empty();
        for id in not_owed {
            self.entries.remove(&id);
        }
        if removed {
            self.note_monitors();
        }
        owed
    }

    /// Takes the closed monitor's notice and makes the watcher's message
    /// from it. `None`: the monitor is gone or was revoked meanwhile.
    pub(crate) fn owed_message(
        &mut self,
        id: MonitorId,
        reason: DownReason,
    ) -> Option<Result<A::Message, String>> {
        let entry = self.entries.get(&id)?;
        if !entry.shared.take_closed() {
            return None;
        }
        let map = entry.map.clone()?;
        let down = entry.down(reason);
        Some(run_map(&*map, down))
    }

    /// Removes the monitor or link whose notice was just dispatched. The ids
    /// of the two families come from one counter.
    pub(crate) fn finish(&mut self, id: MonitorId) {
        if self.entries.remove(&id).is_some() {
            self.note_monitors();
        }
        self.links.finish(id);
    }
}

/// What an entry's task needs.
pub(crate) struct ForwardEntry<A: Actor> {
    pub(crate) shared: Arc<MonitorShared>,
    /// The watched run's plane; `None` for a monitor that reads `NoProc`.
    pub(crate) plane: Option<StatusRx>,
    pub(crate) actor_id: ActorId,
    pub(crate) incarnation: Option<Incarnation>,
    pub(crate) noproc: bool,
    pub(crate) priority: bool,
    /// The task's clone of the watcher's map, held from the entry's creation,
    /// so a task dropped before its first poll drops it the same way a task
    /// that ran does.
    pub(crate) map: MapHold<A::Message>,
}

/// Gives the watched run's count back when the entry's task ends, however it
/// ends: its future completes, it is aborted, or it unwinds.
struct EntryEnd(Arc<MonitorShared>);

impl Drop for EntryEnd {
    fn drop(&mut self) {
        self.0.count.release();
    }
}

/// The task's clone of the watcher's map closure. The closure is the user's, so
/// its drop can panic, and it has two owners (the entry and the task) of which
/// either can drop it last. The guard drops the clone through `end_value` on
/// every path the task ends by, including a task dropped before its first
/// poll (the guard is built with the entry, not inside the task), so a panic
/// of that drop is logged and the actor goes on, as it does when the entry
/// drops the closure last.
pub(crate) struct MapHold<M> {
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

/// The entry's task: waits for the watched run to end, then puts the notice
/// in the watcher's mailbox (or priority lane), once.
///
/// The notice enters through an ordinary mailbox slot, after every message
/// the watched actor sent before it ended, as Erlang/OTP orders a `DOWN`
/// after the sender's earlier messages. A full mailbox makes the task wait for
/// room; the wait ends with the watcher's mailbox, a revoke, or the stage.
pub(crate) async fn monitor_forward<A: Actor>(
    handle: ActorHandle<A>,
    entry: ForwardEntry<A>,
    token: CancellationToken,
) -> Released {
    let ForwardEntry {
        shared,
        plane,
        actor_id,
        incarnation,
        noproc,
        priority,
        map,
    } = entry;
    // Declared after `map`, so it drops first: the watched run's count is
    // given back before the closure's drop can run user code.
    let _end = EntryEnd(shared.clone());
    let reason = match (noproc, plane) {
        (false, Some(mut rx)) => {
            tokio::select! {
                biased;
                _ = token.cancelled() => return Released::default(),
                // The watcher's mailbox closed: the stage ran before it and
                // delivered what was owed.
                () = handle.mailbox_close() => return Released::default(),
                reason = wait_final_reason(&mut rx) => DownReason::Stopped(reason),
            }
        }
        _ => DownReason::NoProc,
    };
    // The notice waits for a slot in the mailbox. A revoke or the stage
    // ends the wait; nothing awaits between the slot and the send.
    let permit = tokio::select! {
        biased;
        _ = token.cancelled() => return Released::default(),
        permit = handle.reserve_signal(priority) => permit,
    };
    let Some(permit) = permit else {
        return Released::default();
    };
    // The map function runs before the cell is taken, so the time between
    // the take and the send is a few instructions: the stage, which waits
    // for a taken notice to land, waits for no user code. A revoke that wins
    // meanwhile drops the message the map made.
    if !shared.is_pending() {
        return Released::default();
    }
    let down = Down {
        monitor: shared.id(),
        actor_id,
        incarnation,
        reason,
    };
    let made = run_map(map.map(), down);
    match made {
        Ok(message) => {
            if shared.take() {
                put_notice(permit, &shared, message);
                Released::default()
            } else {
                drop(permit);
                Released {
                    run: end_value(message),
                    end: None,
                }
            }
        }
        Err(panic) => {
            drop(permit);
            if !shared.take() {
                // Revoked while the map ran: nothing was owed, and the
                // owner is not failed for a monitor it has already released.
                return Released::default();
            }
            // Kept before the raise: a panic that lands after the watcher's
            // message loop read its stop lane for the last time raises where
            // no reader is left, and the stop reads the panic from the
            // incarnation's record.
            handle
                .stop_lane_ref()
                .abort_record()
                .keep_map_panic(panic.clone());
            shared.spend();
            #[cfg(feature = "tracing")]
            caught_log!(warn,
                actor_id = %handle.id(),
                panic = %handle.log_fields().panic_text(&panic),
                "a monitor's map function panicked; the actor fails with that panic"
            );
            let _ = crate::system::raise_noted(
                None,
                handle.stop_lane_ref(),
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

/// Puts a notice whose cell was just taken in the queue the permit was
/// reserved on. A push that finds the queue closed drops the message, so the
/// notice is not spent: the cell reads `Pending` again and the stage owes it.
pub(crate) fn put_notice<A: Actor>(
    permit: ForwardPermit<'_, A>,
    shared: &Arc<MonitorShared>,
    message: A::Message,
) {
    let tag = SignalTag {
        shared: shared.clone(),
    };
    if !permit.send_signal(tag, message) {
        shared.restore();
    }
}

impl MonitorShared {
    /// Whether the notice is still untaken.
    pub(crate) fn is_pending(&self) -> bool {
        self.state.load(Ordering::Acquire) == PENDING
    }

    /// The cell of a link's entry: it counts on no watched run, because a
    /// link is counted in its owner's table only.
    pub(crate) fn unwatched(id: MonitorId) -> Self {
        Self::new(id, MonitorCount::none())
    }

    /// The cell of a request: it counts on no watched run, because a request
    /// is counted in its asker's table only.
    pub(crate) fn for_request(id: MonitorId) -> Arc<Self> {
        Arc::new(Self::unwatched(id))
    }
}

/// Builds an entry and its shared state for a target resolved to `target`,
/// counting the monitor on the watched run when there is one to count on.
/// Returns the entry for the watcher's map and the parts of the task.
pub(crate) fn new_entry<A: Actor>(
    id: MonitorId,
    target: ResolvedTarget,
    priority: bool,
    map: Arc<MapFn<A::Message>>,
    owner: crate::actor::runtime::Owner,
) -> (MonitorEntry<A::Message>, ForwardEntry<A>, CancellationToken) {
    build_entry::<A>(id, target, priority, false, map, owner)
}

/// As [`new_entry`], for the monitor of a spawn the watcher has just made: a
/// run that has already ended by the time the monitor is made still gives its
/// own reason, never `NoProc`, because the watcher knows the run existed.
pub(crate) fn new_spawned_entry<A: Actor>(
    id: MonitorId,
    target: ResolvedTarget,
    map: Arc<MapFn<A::Message>>,
    owner: crate::actor::runtime::Owner,
) -> (MonitorEntry<A::Message>, ForwardEntry<A>, CancellationToken) {
    build_entry::<A>(id, target, false, true, map, owner)
}

fn build_entry<A: Actor>(
    id: MonitorId,
    target: ResolvedTarget,
    priority: bool,
    spawned: bool,
    map: Arc<MapFn<A::Message>>,
    owner: crate::actor::runtime::Owner,
) -> (MonitorEntry<A::Message>, ForwardEntry<A>, CancellationToken) {
    let ResolvedTarget {
        actor_id,
        incarnation,
        status_rx,
        ..
    } = target;
    // One read decides `NoProc`, so a target that ends while the monitor is
    // made reads either `NoProc` or its real reason, never both. A spawned
    // run is known to have existed, so only a missing plane reads `NoProc`.
    let noproc = match &status_rx {
        None => true,
        Some(rx) => !spawned && peek_final(rx).is_some(),
    };
    let count = match (&status_rx, noproc) {
        (Some(rx), false) => MonitorCount::on(rx.record().clone()),
        _ => MonitorCount::none(),
    };
    let shared = Arc::new(MonitorShared::new(id, count));
    let token = CancellationToken::new();
    let plane = if noproc { None } else { status_rx.clone() };
    let entry = MonitorEntry {
        shared: shared.clone(),
        token: token.clone(),
        plane: status_rx,
        actor_id: actor_id.clone(),
        incarnation,
        noproc,
        map: Some(map.clone()),
        owner: owner.clone(),
    };
    let task = ForwardEntry {
        shared,
        plane,
        actor_id,
        incarnation,
        noproc,
        priority,
        map: MapHold {
            map: Some(map),
            owner,
        },
    };
    (entry, task, token)
}

mod sealed {
    use super::{ActorSystem, ExitVia, ResolvedTarget};

    /// The answer of a resolution, opaque outside the crate.
    #[derive(Debug)]
    pub struct Resolved(pub(crate) ResolvedTarget);

    /// How a target was named, opaque outside the crate.
    #[derive(Debug)]
    pub struct Route(pub(crate) ExitVia);

    /// What a monitor target resolves through. Sealed: only the types this
    /// crate lists name a target.
    pub trait Sealed {
        /// The actor, or the absence of one, this target names, resolved in
        /// `system` for an id or a name.
        fn resolve(&self, system: &ActorSystem) -> Resolved;

        /// How the target is named, which decides where an exit signal finds
        /// its registry entry.
        fn route(&self) -> Route;
    }
}

/// How an exit signal's target was named: the registry entry it is raised on
/// is keyed by the id, by the name, or found through the system of the handle.
#[derive(Debug, Clone)]
pub(crate) enum ExitVia {
    /// An actor id, looked up in the sender's system.
    Id,
    /// A registered name, looked up in the sender's system.
    Name,
    /// A handle: its own system, reached by identity through the handle's
    /// record (`None` once that system is gone), never by name.
    Handle(Option<Arc<crate::system::ActorSystem>>),
}

/// What [`ActorContext::monitor`](crate::actor::context::ActorContext::monitor)
/// and [`monitor_priority`](crate::actor::context::ActorContext::monitor_priority)
/// accept as the actor to watch.
///
/// - A reference to an [`ActorHandle`] watches the run that handle reaches.
/// - A reference to an [`ActorId`], or a name as `&str` or `&String`, is
///   resolved once, when the monitor is made, in the watcher's own system. A
///   key that matches no actor gives a notice with
///   [`DownReason::NoProc`](crate::types::DownReason::NoProc), as Erlang/OTP's
///   `monitor/2` on a missing name does.
///
/// The trait is sealed; no other type names a target.
///
/// # Examples
/// ```rust
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     ActorResult, Down,
/// };
///
/// struct Idle;
///
/// impl Actor for Idle {
///     type Message = ();
///     type Response = ();
///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
///
/// struct Watcher;
///
/// impl Actor for Watcher {
///     type Message = Down;
///     type Response = ();
///     async fn handle(&mut self, _: Down, _: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let target = Idle.spawn().named("target-of-the-watch").await?;
/// let watcher = Watcher.spawn().await?;
/// # drop(watcher);
/// // By handle, by id and by name.
/// fn accepts<T: tokio_actors::IntoMonitorTarget>(_: T) {}
/// accepts(&target);
/// accepts(target.id());
/// accepts("target-of-the-watch");
/// # Ok(())
/// # }
/// ```
pub trait IntoMonitorTarget: sealed::Sealed {}

impl<T: sealed::Sealed + ?Sized> IntoMonitorTarget for T {}

impl<B: Actor> sealed::Sealed for &ActorHandle<B> {
    fn resolve(&self, _system: &ActorSystem) -> sealed::Resolved {
        sealed::Resolved(ResolvedTarget {
            actor_id: self.id().clone(),
            incarnation: Some(self.incarnation()),
            status_rx: Some(self.status_rx()),
            lane: Some(self.stop_lane_ref().clone()),
        })
    }

    fn route(&self) -> sealed::Route {
        sealed::Route(ExitVia::Handle(self.system()))
    }
}

impl sealed::Sealed for &ActorId {
    fn resolve(&self, system: &ActorSystem) -> sealed::Resolved {
        sealed::Resolved(system.resolve_monitor_target(MonitorKey::Id(self)))
    }

    fn route(&self) -> sealed::Route {
        sealed::Route(ExitVia::Id)
    }
}

impl sealed::Sealed for &str {
    fn resolve(&self, system: &ActorSystem) -> sealed::Resolved {
        sealed::Resolved(system.resolve_monitor_target(MonitorKey::Name(self)))
    }

    fn route(&self) -> sealed::Route {
        sealed::Route(ExitVia::Name)
    }
}

impl sealed::Sealed for &String {
    fn resolve(&self, system: &ActorSystem) -> sealed::Resolved {
        sealed::Resolved(system.resolve_monitor_target(MonitorKey::Name(self)))
    }

    fn route(&self) -> sealed::Route {
        sealed::Route(ExitVia::Name)
    }
}

/// How the caller named a target.
pub(crate) fn route_of<T: IntoMonitorTarget>(target: &T) -> ExitVia {
    sealed::Sealed::route(target).0
}

/// Resolves a target the caller named.
pub(crate) fn resolve_target<T: IntoMonitorTarget>(
    target: &T,
    system: &ActorSystem,
) -> ResolvedTarget {
    sealed::Sealed::resolve(target, system).0
}
