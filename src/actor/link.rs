//! Links and the trap flag.
//!
//! An actor that calls [`link`](crate::actor::context::ActorContext::link)
//! is tied to the actor it names: when either ends, the other is told. This
//! module holds the pieces behind it:
//!
//! - the state both ends of one link share, which makes one exit and one
//!   cancel agree on who wins;
//! - the inbox in an actor's shared block, where the other end's link, unlink
//!   and exit traffic waits in order until the actor's run loop takes it;
//! - the entry an actor keeps for each link, and the task that waits on the
//!   other end's status plane and acts when it ends;
//! - the trap map an actor that traps exits gives, and the stop-time reads of
//!   the notices an ending trapping actor is still owed.
//!
//! A link is bound to one run of each end. Neither end's entry holds a handle
//! to the other end: the task holds the other end's status receiver only, so
//! a link keeps nothing of the other actor alive.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::actor::context::{ActorContext, Released};
use crate::actor::handle::{ActorHandle, ForwardPermit};
use crate::actor::monitor::{peek_final, resolve_target, IntoMonitorTarget, MonitorShared};
use crate::actor::panic::{catch_sync, payload_into_string};
use crate::actor::runtime::{end_value, log_late_drop};
use crate::actor::subtree::{AbortRecord, Coupling};
use crate::actor::Actor;
use crate::error::{ActorError, LinkError};
use crate::system::{
    raise_kill_declared, wait_final_reason, ActorSystem, KillKey, KillRaised, ResolvedTarget,
};
use crate::types::{
    ActorId, ActorStatus, DownReason, Incarnation, LinkExit, MonitorId, PeerRef, SignalTag,
    StatusRx, StopLane, StopReason,
};

/// The link is in force.
const ACTIVE: u8 = 0;
/// One end cancelled the link before either end acted on it.
const CANCELLED: u8 = 1;
/// One end acted on the other end's exit.
const FIRED: u8 = 2;

/// The state both ends of one link hold. One compare-exchange decides who
/// acts: the end that reacts to the other end's exit, or the end that
/// cancels the link.
#[derive(Debug)]
pub(crate) struct LinkState {
    state: AtomicU8,
    /// Whether each end's exit rides the priority lane, indexed by
    /// [`side`](Self::side).
    priority: [AtomicBool; 2],
}

impl LinkState {
    /// A link in force, both ends on the ordinary lane.
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicU8::new(ACTIVE),
            priority: [AtomicBool::new(false), AtomicBool::new(false)],
        }
    }

    fn swap(&self, from: u8, to: u8) -> bool {
        self.state
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// An end acts on the other end's exit, from `Active` to `Fired`. Only the
    /// winner acts.
    pub(crate) fn fire(&self) -> bool {
        self.swap(ACTIVE, FIRED)
    }

    /// An end cancels the link, from `Active` to `Cancelled`. True means no
    /// exit will be acted on from here on.
    pub(crate) fn cancel(&self) -> bool {
        self.swap(ACTIVE, CANCELLED)
    }

    /// The exit did not land, so it is still owed: from `Fired` back to
    /// `Active`.
    pub(crate) fn unfire(&self) {
        self.swap(FIRED, ACTIVE);
    }

    /// Whether the link is in force.
    pub(crate) fn is_active(&self) -> bool {
        self.state.load(Ordering::Acquire) == ACTIVE
    }

    /// Which of the two flags belongs to the end that is `own` when the other
    /// end is `peer`: 0 for the lower incarnation.
    pub(crate) fn side(own: Incarnation, peer: Incarnation) -> usize {
        usize::from(own > peer)
    }

    /// Whether the end on `side` takes its exit through the priority lane.
    pub(crate) fn priority(&self, side: usize) -> bool {
        self.priority[side].load(Ordering::Acquire)
    }

    /// Sets whether the end on `side` takes its exit through the priority
    /// lane.
    pub(crate) fn set_priority(&self, side: usize, on: bool) {
        self.priority[side].store(on, Ordering::Release);
    }
}

/// What waits in an actor's signal inbox, in the order it was pushed.
#[derive(Debug)]
pub(crate) enum InboxItem {
    /// The other end made a link to this actor. It carries the state both
    /// ends share and the other end's status plane, never a handle, so the
    /// item can be installed after the other end ended.
    Link {
        state: Arc<LinkState>,
        from: PeerRef,
        plane: StatusRx,
    },
    /// The other end cancelled the link whose shared state this is.
    Unlink {
        from: Incarnation,
        state: Arc<LinkState>,
    },
    /// An exit signal sent with `exit`, queued because this actor traps
    /// exits. `priority`: the sender asked that it ride the priority lane.
    Exit {
        from: PeerRef,
        reason: StopReason,
        priority: bool,
    },
}

/// The inbox and the pair table of one incarnation's shared block.
#[derive(Debug, Default)]
pub(crate) struct SignalBlock {
    pub(crate) inbox: VecDeque<InboxItem>,
    /// The states of the links whose lower incarnation owns this block, keyed
    /// by the higher incarnation of the pair.
    pub(crate) pairs: BTreeMap<Incarnation, Arc<LinkState>>,
}

/// How a push into an inbox ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushResult {
    /// The item waits in the inbox.
    Pushed,
    /// The inbox is sealed: its owner is ending.
    Sealed,
}

/// How a push of an exit signal ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitPush {
    /// The target traps exits and the signal waits in its inbox.
    Pushed,
    /// The inbox is sealed: the target is ending.
    Sealed,
    /// The target does not trap exits, and nothing was pushed.
    NotTrapping,
    /// The target traps exits and as many exit signals wait for it as its
    /// bound allows: nothing was pushed.
    Full,
}

/// One exit signal counted against its target's bound, from the drain that
/// took it out of the inbox until its delivery takes a mailbox slot or ends
/// without one. Dropping it gives the count back on every path: the task
/// that carries it ending, aborted or dropped with its runtime included.
pub(crate) struct ExitSlot(AbortRecord);

impl Drop for ExitSlot {
    fn drop(&mut self) {
        self.0.release_exit();
    }
}

// ---------------------------------------------------------------------------
// The trap map
// ---------------------------------------------------------------------------

/// The function that turns an exit into one of the actor's messages.
pub(crate) type TrapFn<M> = dyn Fn(LinkExit) -> M + Send + Sync + 'static;

/// The map an actor that traps exits gave, shared with the tasks of its
/// links so each reads the map that stands when the other end's exit is seen.
/// The map is the user's, so its drop can panic: the last owner drops it
/// inside a catch and logs the panic, and the actor goes on.
pub(crate) struct TrapMap<M> {
    owner: crate::actor::runtime::Owner,
    map: Mutex<Option<Arc<TrapFn<M>>>>,
}

impl<M> TrapMap<M> {
    /// The map that stands now.
    pub(crate) fn get(&self) -> Option<Arc<TrapFn<M>>> {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn replace(&self, map: Option<Arc<TrapFn<M>>>) {
        let old = std::mem::replace(
            &mut *self.map.lock().unwrap_or_else(|p| p.into_inner()),
            map,
        );
        if let Some(old) = old {
            drop_map(old, &self.owner);
        }
    }
}

impl<M> Drop for TrapMap<M> {
    fn drop(&mut self) {
        let map = self.map.get_mut().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(map) = map {
            drop_map(map, &self.owner);
        }
    }
}

/// Drops one owner's clone of a user's map inside a catch.
fn drop_map<M>(map: Arc<TrapFn<M>>, owner: &crate::actor::runtime::Owner) {
    if let Some(panic) = end_value(map) {
        log_late_drop(&panic, owner);
    }
}

/// An actor's trap map, made with the first call that needs it.
pub(crate) struct TrapCell<M> {
    shared: Option<Arc<TrapMap<M>>>,
}

impl<M> Default for TrapCell<M> {
    fn default() -> Self {
        Self { shared: None }
    }
}

impl<M> TrapCell<M> {
    /// The cell the tasks of the links read, made if it is not yet.
    pub(crate) fn share(&mut self, owner: &crate::actor::runtime::Owner) -> Arc<TrapMap<M>> {
        self.shared
            .get_or_insert_with(|| {
                Arc::new(TrapMap {
                    owner: owner.clone(),
                    map: Mutex::new(None),
                })
            })
            .clone()
    }

    /// Stores the map.
    pub(crate) fn set(&mut self, owner: &crate::actor::runtime::Owner, map: Arc<TrapFn<M>>) {
        self.share(owner).replace(Some(map));
    }

    /// Forgets the map.
    pub(crate) fn clear(&mut self) {
        if let Some(shared) = &self.shared {
            shared.replace(None);
        }
    }

    /// The map that stands now.
    pub(crate) fn get(&self) -> Option<Arc<TrapFn<M>>> {
        self.shared.as_ref().and_then(|shared| shared.get())
    }
}

/// Runs the user's map, catching a panic.
fn run_trap<M>(map: &TrapFn<M>, exit: LinkExit) -> Result<M, String> {
    catch_sync(|| map(exit)).map_err(payload_into_string)
}

// ---------------------------------------------------------------------------
// The link table
// ---------------------------------------------------------------------------

/// One link in its owner's table, or one exit signal waiting to be delivered
/// (an entry with a `signal`, which has no peer to wait for).
pub(crate) struct LinkEntry {
    shared: Arc<MonitorShared>,
    state: Arc<LinkState>,
    token: CancellationToken,
    peer_id: ActorId,
    peer_incarnation: Option<Incarnation>,
    /// The other end's status receiver; `None` for a link that read `NoProc`
    /// when it was made and for an exit signal.
    plane: Option<StatusRx>,
    /// True when the other end was gone when the link was made, so the exit
    /// reads `NoProc`.
    noproc: bool,
    /// The reason of an exit signal this entry delivers.
    signal: Option<StopReason>,
    /// The entry of a spawned run that had ended before the link was made: it
    /// keeps the run's plane and reads the reason there, but it is a fire that
    /// already won, so it is not counted as a link and `unlink` does not find
    /// it.
    late: bool,
}

impl Drop for LinkEntry {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

impl LinkEntry {
    /// The exit this entry owes now, if the other end has ended and the link
    /// is in force.
    fn owed(&self) -> Option<LinkExit> {
        if !self.state.is_active() {
            return None;
        }
        let reason = if let Some(reason) = &self.signal {
            DownReason::Stopped(reason.clone())
        } else if self.noproc {
            DownReason::NoProc
        } else {
            DownReason::Stopped(peek_final(self.plane.as_ref()?)?)
        };
        Some(LinkExit {
            actor_id: self.peer_id.clone(),
            incarnation: self.peer_incarnation,
            reason,
        })
    }

    /// Whether the entry is a link in force: not an exit signal and not the
    /// entry of a run that had ended before its link was made.
    fn is_link(&self) -> bool {
        self.signal.is_none() && !self.late
    }

    /// Whether the entry must stay in the table after its task ended: its
    /// exit is queued, or it was restored and the stop-time stage owes it.
    fn keeps(&self) -> bool {
        self.shared.is_sent() || (self.shared.is_pending() && self.state.is_active())
    }
}

/// What a cancelled link leaves for the caller to finish.
pub(crate) struct Unlinked {
    /// True when the cancel was in time: no exit will be acted on.
    pub(crate) cancelled: bool,
    /// The other end's block, to tell it, when the cancel was in time.
    partner: Option<AbortRecord>,
    state: Arc<LinkState>,
}

/// The links an actor holds, with the exit signals it is delivering.
#[derive(Default)]
pub(crate) struct LinkTable {
    entries: BTreeMap<MonitorId, LinkEntry>,
    /// The entry of each link, by the other end's incarnation.
    by_peer: BTreeMap<Incarnation, MonitorId>,
    /// How many entries are links (an exit signal's entry is not).
    linked: usize,
    /// The block of the actor that holds the table, set by the first insert:
    /// the count of links is recorded there for a snapshot to read.
    own: Option<AbortRecord>,
}

impl LinkTable {
    /// The links held and not yet delivered or cancelled.
    pub(crate) fn len(&self) -> usize {
        self.linked
    }

    /// Whether anything waits in the table.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether a link to this run of the other end is held.
    pub(crate) fn contains(&self, peer: Incarnation) -> bool {
        self.by_peer.contains_key(&peer)
    }

    /// Whether an entry with this id is held.
    pub(crate) fn contains_id(&self, id: MonitorId) -> bool {
        self.entries.contains_key(&id)
    }

    /// Names the block of the actor that holds the table, where the count of
    /// links is recorded; the first call is kept.
    fn bind(&mut self, own: &AbortRecord) {
        if self.own.is_none() {
            self.own = Some(own.clone());
        }
    }

    fn insert(&mut self, id: MonitorId, entry: LinkEntry) {
        if entry.is_link() {
            self.linked += 1;
            if let Some(peer) = entry.peer_incarnation {
                self.by_peer.insert(peer, id);
            }
            self.note_links();
        }
        self.entries.insert(id, entry);
    }

    /// Records the number of links held in the actor's block.
    fn note_links(&self) {
        if let Some(own) = &self.own {
            own.set_coupling(
                Coupling::Link,
                u32::try_from(self.linked).unwrap_or(u32::MAX),
            );
        }
    }

    /// Removes an entry; dropping it cancels its task.
    pub(crate) fn remove(&mut self, id: MonitorId) -> Option<LinkEntry> {
        let entry = self.entries.remove(&id)?;
        if entry.is_link() {
            self.linked = self.linked.saturating_sub(1);
            if let Some(peer) = entry.peer_incarnation {
                if self.by_peer.get(&peer) == Some(&id) {
                    self.by_peer.remove(&peer);
                }
            }
            self.note_links();
        }
        Some(entry)
    }

    /// The state of the link held to this run of the other end, when it is in
    /// force.
    fn live_state(&self, peer: Incarnation) -> Option<&Arc<LinkState>> {
        let entry = self.entries.get(self.by_peer.get(&peer)?)?;
        entry.state.is_active().then_some(&entry.state)
    }

    /// Cancels the link held to `peer`. `None`: no link is held to it.
    pub(crate) fn unlink(&mut self, peer: Incarnation) -> Option<Unlinked> {
        let id = *self.by_peer.get(&peer)?;
        let entry = self.entries.get(&id)?;
        let state = entry.state.clone();
        if !state.cancel() {
            // The exit was already acted on; its entry stays until it is
            // delivered.
            return Some(Unlinked {
                cancelled: false,
                partner: None,
                state,
            });
        }
        let entry = self.remove(id)?;
        entry.shared.revoke();
        let partner = entry.plane.as_ref().map(|plane| plane.record().clone());
        Some(Unlinked {
            cancelled: true,
            partner,
            state,
        })
    }

    /// Removes the link the other end cancelled, when this end still holds
    /// the entry of that same link.
    fn remove_cancelled(&mut self, from: Incarnation, state: &Arc<LinkState>) {
        let Some(id) = self.by_peer.get(&from).copied() else {
            return;
        };
        if self
            .entries
            .get(&id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.state, state))
        {
            self.remove(id);
        }
    }

    /// Whether a queued exit of this id is wanted: the entry is held.
    pub(crate) fn wants(&self, tag: &SignalTag) -> bool {
        !tag.shared.is_flushed() && self.entries.contains_key(&tag.shared.id())
    }

    /// Removes the entry whose exit was just dispatched.
    pub(crate) fn finish(&mut self, id: MonitorId) {
        self.remove(id);
    }

    /// Whether the stage must look in the queues: some exit was taken and
    /// has not been seen yet.
    pub(crate) fn in_flight(&self, seen: &HashSet<MonitorId>) -> bool {
        self.entries
            .iter()
            .any(|(id, entry)| entry.shared.is_sent() && !seen.contains(id))
    }

    /// The stage closes every entry nobody has taken. A closed entry whose
    /// exit is owed and whose owner can deliver it (`deliver`) is returned, in
    /// the order the entries were made, with the exit to deliver. A closed
    /// entry that owes nothing, or whose owner has no map, is removed.
    pub(crate) fn stage_pass(&mut self, deliver: bool) -> Vec<(MonitorId, LinkExit)> {
        let mut owed = Vec::new();
        let mut gone = Vec::new();
        for (id, entry) in &self.entries {
            if !entry.shared.claim_for_stage() {
                continue;
            }
            match entry.owed() {
                Some(exit) if deliver => owed.push((*id, exit)),
                _ => gone.push(*id),
            }
        }
        for id in gone {
            self.remove(id);
        }
        owed
    }

    /// Removes every entry.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.by_peer.clear();
        self.linked = 0;
        self.note_links();
    }

    /// An entry's task ended. The entry stays when its exit is queued or the
    /// stage owes it, and is removed otherwise; a task that ended by a panic
    /// leaves nothing to wait for.
    pub(crate) fn reaped(&mut self, id: MonitorId, panicked: bool) {
        let keep = !panicked && self.entries.get(&id).is_some_and(LinkEntry::keeps);
        if !keep {
            self.remove(id);
        }
    }
}

/// Gives the pair back to the block that holds it when the task ends and the
/// link is not in force. The pair is removed only when the table still
/// holds this link's state, so a later link between the same two runs is
/// never taken for this one.
struct LinkEnd {
    lower: Option<AbortRecord>,
    own: Incarnation,
    peer: Option<Incarnation>,
    state: Arc<LinkState>,
}

impl Drop for LinkEnd {
    fn drop(&mut self) {
        if self.state.is_active() {
            return;
        }
        if let (Some(lower), Some(peer)) = (&self.lower, self.peer) {
            lower.remove_pair(self.own, peer, &self.state);
        }
    }
}

// ---------------------------------------------------------------------------
// The entry's task
// ---------------------------------------------------------------------------

/// What an entry's task needs.
pub(crate) struct LinkTask<A: Actor> {
    shared: Arc<MonitorShared>,
    state: Arc<LinkState>,
    side: usize,
    plane: Option<StatusRx>,
    peer_id: ActorId,
    peer_incarnation: Option<Incarnation>,
    noproc: bool,
    signal: Option<StopReason>,
    trap: Arc<TrapMap<A::Message>>,
    system: Arc<ActorSystem>,
    supervisor: bool,
    end: LinkEnd,
    /// The count an exit signal holds against its target's bound; `None`
    /// for a link.
    slot: Option<ExitSlot>,
}

/// What the entry's task saw.
enum Ended {
    /// The other end ended with this reason.
    Peer(StopReason),
    /// An exit signal with this reason arrived.
    Signal(StopReason),
    /// The other end was not running when the link was made.
    NoProc,
}

impl Ended {
    fn down_reason(&self) -> DownReason {
        match self {
            Ended::Peer(reason) | Ended::Signal(reason) => DownReason::Stopped(reason.clone()),
            Ended::NoProc => DownReason::NoProc,
        }
    }

    /// The reason an owner that does not trap exits ends with, or `None` when
    /// the exit leaves it running: an orderly `Graceful` end of the other
    /// end does not end its partner.
    fn cause(&self, peer: &ActorId) -> Option<StopReason> {
        match self {
            Ended::Peer(StopReason::Graceful) | Ended::Signal(StopReason::Graceful) => None,
            Ended::Peer(reason) | Ended::Signal(reason) => Some(reason.clone()),
            Ended::NoProc => Some(StopReason::Failure(ActorError::NoProc(peer.clone()))),
        }
    }
}

/// Raises a `Kill` on the owner that carries `cause`, through the one
/// function every `Kill` raise on a registered actor goes through.
pub(crate) fn raise_caused(
    system: &ActorSystem,
    key: KillKey<'_>,
    status_rx: &StatusRx,
    lane: &StopLane,
    cause: StopReason,
) -> KillRaised {
    raise_kill_declared(
        Some(system),
        key,
        status_rx,
        lane,
        None,
        None,
        || {},
        Some(cause),
    )
}

/// Ends the owner as the other end's exit asks of one that does not trap
/// exits.
fn end_owner<A: Actor>(
    handle: &ActorHandle<A>,
    system: &ActorSystem,
    ended: &Ended,
    peer: &ActorId,
) {
    if let Some(cause) = ended.cause(peer) {
        let status_rx = handle.status_rx();
        raise_caused(
            system,
            KillKey::Id(handle.id()),
            &status_rx,
            handle.stop_lane_ref(),
            cause,
        );
    }
}

/// The entry's task: waits for the other end to end, then reads the owner's
/// trap flag and acts, once.
///
/// An owner that does not trap exits ends with the other end's reason (an
/// orderly `Graceful` end leaves it running). An owner that traps exits gets
/// the exit through an ordinary mailbox slot, after every message the other
/// end sent before it ended, as Erlang/OTP orders an `'EXIT'` message after
/// the sender's earlier messages; a full mailbox makes the task wait for
/// room, and the wait ends with the owner's mailbox, a cancel, or the stage.
/// A supervisor with no map drops an exit that is not its child's.
pub(crate) async fn link_forward<A: Actor>(
    handle: ActorHandle<A>,
    task: LinkTask<A>,
    token: CancellationToken,
) -> Released {
    let LinkTask {
        shared,
        state,
        side,
        plane,
        peer_id,
        peer_incarnation,
        noproc,
        signal,
        trap,
        system,
        supervisor,
        end,
        slot,
    } = task;
    // Declared after the parts it follows, so it drops first.
    let _end = end;
    let ended = match (signal, noproc, plane) {
        (Some(reason), _, _) => Ended::Signal(reason),
        (None, true, _) | (None, false, None) => Ended::NoProc,
        (None, false, Some(mut rx)) => {
            tokio::select! {
                biased;
                _ = token.cancelled() => return Released::default(),
                // The owner's mailbox closed: the stage ran before it and
                // delivered what was owed.
                () = handle.mailbox_close() => return Released::default(),
                reason = wait_final_reason(&mut rx) => Ended::Peer(reason),
            }
        }
    };
    // The flag is read now, when the exit is seen, and the map that stands
    // now is the one used.
    let trapping = handle.stop_lane_ref().abort_record().trap();
    let map = if trapping { trap.get() } else { None };
    let Some(map) = map else {
        if trapping && supervisor {
            // A supervisor with no map drops an exit that is not its
            // child's: no log, no stop.
            state.fire();
        } else if state.fire() {
            end_owner(&handle, &system, &ended, &peer_id);
        }
        return Released::default();
    };
    let priority = state.priority(side);
    // The exit waits for a slot in the mailbox. A cancel or the stage ends
    // the wait; nothing awaits between the slot and the send.
    let permit = tokio::select! {
        biased;
        _ = token.cancelled() => return release_map(map, &handle, Released::default()),
        permit = handle.reserve_signal(priority) => permit,
    };
    let Some(permit) = permit else {
        drop(slot);
        return release_map(map, &handle, Released::default());
    };
    // The exit holds a mailbox slot from here on, so the mailbox bounds it:
    // it leaves its target's count of waiting exit signals.
    drop(slot);
    // The map runs before the cell is taken, so the time between the take and
    // the send is a few instructions: the stage, which waits for a taken exit
    // to land, waits for no user code. A cancel that wins meanwhile drops the
    // message the map made.
    if !shared.is_pending() {
        return release_map(map, &handle, Released::default());
    }
    let exit = LinkExit {
        actor_id: peer_id.clone(),
        incarnation: peer_incarnation,
        reason: ended.down_reason(),
    };
    let made = run_trap(&*map, exit);
    let released = match made {
        Ok(message) => {
            if !shared.take() {
                drop(permit);
                Released {
                    run: end_value(message),
                    end: None,
                }
            } else if !state.fire() {
                // The link was cancelled, or the other end acted, meanwhile:
                // no exit is owed.
                shared.spend();
                drop(permit);
                Released {
                    run: end_value(message),
                    end: None,
                }
            } else {
                put_exit(permit, &shared, &state, message);
                Released::default()
            }
        }
        Err(panic) => {
            drop(permit);
            if shared.take() {
                shared.spend();
                if state.fire() {
                    // Kept before the raise, as a monitor's map panic is: a
                    // panic that lands after the owner's message loop read
                    // its stop lane for the last time raises where no
                    // reader is left, and the stop reads the panic from the
                    // incarnation's record.
                    handle
                        .stop_lane_ref()
                        .abort_record()
                        .keep_map_panic(panic.clone());
                    #[cfg(feature = "tracing")]
                    caught_log!(warn,
                        actor_id = %handle.id(),
                        panic = %handle.log_fields().panic_text(&panic),
                        "a trap map panicked; the actor fails with that panic"
                    );
                    let _ = crate::system::raise_noted(
                        None,
                        handle.stop_lane_ref(),
                        StopReason::Failure(ActorError::Panic(panic.clone())),
                        crate::telemetry::StopOrigin::MapPanic,
                    );
                }
            }
            Released {
                run: Some(panic),
                end: None,
            }
        }
    };
    release_map(map, &handle, released)
}

/// Drops the task's clone of the user's map inside a catch, and keeps the
/// first panic of the task's own drops.
fn release_map<A: Actor, M>(
    map: Arc<TrapFn<M>>,
    handle: &ActorHandle<A>,
    mut released: Released,
) -> Released {
    if let Some(panic) = end_value(map) {
        log_late_drop(&panic, &handle.owner());
    }
    if released.run.is_none() && released.end.is_some() {
        released.run = released.end.take();
    }
    released
}

/// Puts an exit whose cell was just taken and whose state fired in the queue
/// the permit was reserved on. A push that finds the queue closed drops the
/// message, so the exit is not spent: the cell reads `Pending` and the state
/// `Active` again, and the stage owes it.
fn put_exit<A: Actor>(
    permit: ForwardPermit<'_, A>,
    shared: &Arc<MonitorShared>,
    state: &LinkState,
    message: A::Message,
) {
    let tag = SignalTag {
        shared: shared.clone(),
    };
    if !permit.send_signal(tag, message) {
        shared.restore();
        state.unfire();
    }
}

// ---------------------------------------------------------------------------
// The context side
// ---------------------------------------------------------------------------

/// What a new entry is made from.
struct Spec {
    state: Arc<LinkState>,
    side: usize,
    plane: Option<StatusRx>,
    peer_id: ActorId,
    peer_incarnation: Option<Incarnation>,
    noproc: bool,
    signal: Option<StopReason>,
    /// See [`LinkEntry::late`].
    late: bool,
    /// The block that holds the pair of a link to a live run.
    lower: Option<AbortRecord>,
    /// See [`LinkTask::slot`].
    slot: Option<ExitSlot>,
}

/// Makes an entry and starts its task.
fn add_entry<A: Actor>(ctx: &mut ActorContext<A>, spec: Spec) {
    let Spec {
        state,
        side,
        plane,
        peer_id,
        peer_incarnation,
        noproc,
        signal,
        late,
        lower,
        slot,
    } = spec;
    let id = MonitorId::next();
    let shared = Arc::new(MonitorShared::unwatched(id));
    let token = CancellationToken::new();
    let own = ctx.stop_lane().abort_record().incarnation();
    let task = LinkTask::<A> {
        shared: shared.clone(),
        state: state.clone(),
        side,
        plane: plane.clone(),
        peer_id: peer_id.clone(),
        peer_incarnation,
        noproc,
        signal: signal.clone(),
        trap: ctx.trap_share(),
        system: ctx.resolved_system(),
        supervisor: ctx.supervision_ref().is_some(),
        end: LinkEnd {
            lower,
            own,
            peer: peer_incarnation,
            state: state.clone(),
        },
        slot,
    };
    let entry = LinkEntry {
        shared,
        state,
        token: token.clone(),
        peer_id,
        peer_incarnation,
        plane,
        noproc,
        signal,
        late,
    };
    let handle = ctx.self_handle().clone();
    // An exit signal that rides the priority lane is put there before the
    // run loop reads its next branch, so it is handled ahead of the ordinary
    // messages already queued, as a link made with the priority flag is. The
    // task is polled once here: it reaches its slot and its send without
    // waiting unless the lane is full, in which case the rest of it runs on
    // the forwarder plane as any entry's does.
    let at_once = task.signal.is_some() && state_priority(&task);
    let own_record = ctx.stop_lane().abort_record().clone();
    let table = ctx.link_table();
    table.bind(&own_record);
    table.insert(id, entry);
    let fut = link_forward(handle, task, token);
    if !at_once {
        ctx.spawn_link_forwarder(id, fut);
        return;
    }
    let mut fut = Box::pin(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    // The trap map runs in this poll, so the poll is a turn for the turn
    // watchdog, as a turn that handles a message is, and a panic that escapes
    // it ends the task on the forwarder plane, recorded as any forwarder's
    // panic is, in place of unwinding the run loop.
    let clock = ctx.turn_clock();
    clock.begin();
    let polled = catch_sync(|| fut.as_mut().poll(&mut cx));
    clock.end();
    match polled {
        Ok(std::task::Poll::Ready(released)) => {
            ctx.spawn_link_forwarder(id, async move { released });
        }
        Ok(std::task::Poll::Pending) => ctx.spawn_link_forwarder(id, fut),
        Err(payload) => ctx.spawn_link_forwarder(id, rethrow(payload)),
    }
}

/// A forwarder task that ends by the panic an inline poll caught.
async fn rethrow(payload: crate::actor::panic::PanicPayload) -> Released {
    std::panic::resume_unwind(payload)
}

/// Whether the task's own end of the link takes its exit through the
/// priority lane.
fn state_priority<A: Actor>(task: &LinkTask<A>) -> bool {
    task.state.priority(task.side)
}

/// `ctx.link` and `ctx.link_priority`.
///
/// `spawned` is true for the link of a run this actor has just spawned: a run
/// known to have existed, whose end before the link is made still reaches the
/// owner as its own reason, as if the link had been there before the run began.
/// That exit is a fire that already won, so `unlink` answers false for it and
/// it still arrives.
pub(crate) fn link<A: Actor, T: IntoMonitorTarget>(
    ctx: &mut ActorContext<A>,
    target: T,
    priority: bool,
    spawned: bool,
) -> Result<(), LinkError> {
    if matches!(ctx.status(), ActorStatus::Stopping | ActorStatus::Stopped) {
        return Err(LinkError::Stopping);
    }
    let ResolvedTarget {
        actor_id,
        incarnation,
        status_rx,
        ..
    } = resolve_target(&target, &ctx.resolved_system());
    let own_record = ctx.stop_lane().abort_record().clone();
    let own = own_record.incarnation();
    if status_rx
        .as_ref()
        .is_some_and(|rx| rx.record().same_block(&own_record))
    {
        // A link to oneself is nothing, as in Erlang/OTP.
        return Ok(());
    }
    let live = status_rx
        .as_ref()
        .filter(|rx| peek_final(rx).is_none())
        .cloned();
    let Some(rx) = live else {
        if let Some(plane) = status_rx.filter(|_| spawned) {
            // The run ended before this call. The entry keeps its plane and
            // its task reads the reason there, as the monitor's does, so a
            // `Kill` that carried a cause gives the cause on both halves;
            // the link rules then decide (`Graceful` leaves a non-trapping
            // owner running, a trapping owner reads a `LinkExit`, any other
            // reason ends the owner with it). No pair exists for an ended
            // run, and the exit is a fire that already won: `unlink` answers
            // false and the exit still arrives.
            let state = Arc::new(LinkState::new());
            state.set_priority(0, priority);
            add_entry(
                ctx,
                Spec {
                    state,
                    side: 0,
                    plane: Some(plane),
                    peer_id: actor_id,
                    peer_incarnation: incarnation,
                    noproc: false,
                    signal: None,
                    late: true,
                    lower: None,
                    slot: None,
                },
            );
            return Ok(());
        }
        // The other end is gone: the link delivers `NoProc` and holds no
        // pair.
        let peer = incarnation.unwrap_or(own);
        let noproc = Spec {
            state: Arc::new(LinkState::new()),
            side: LinkState::side(own, peer),
            plane: None,
            peer_id: actor_id,
            peer_incarnation: incarnation,
            noproc: true,
            signal: None,
            late: false,
            lower: None,
            slot: None,
        };
        add_entry(ctx, noproc);
        return Ok(());
    };
    let peer = rx.record().incarnation();
    let side = LinkState::side(own, peer);
    if let Some(state) = ctx.link_table().live_state(peer) {
        // A repeated call is one link; it sets this end's lane.
        state.set_priority(side, priority);
        return Ok(());
    }
    let lower = if own < peer {
        own_record.clone()
    } else {
        rx.record().clone()
    };
    let state = loop {
        let (state, _) = own_record.pair_state(own, rx.record());
        if state.is_active() {
            break state;
        }
        // A pair whose link was acted on and whose task has not yet taken
        // it back: this call makes a new one.
        lower.remove_pair(own, peer, &state);
    };
    state.set_priority(side, priority);
    add_entry(
        ctx,
        Spec {
            state: state.clone(),
            side,
            plane: Some(rx.clone()),
            peer_id: actor_id,
            peer_incarnation: Some(peer),
            noproc: false,
            signal: None,
            late: false,
            lower: Some(lower),
            slot: None,
        },
    );
    // The other end installs its entry from its own run loop, in the order
    // the items were pushed. The item carries this end's status plane, so it
    // is installed whether or not this end is still running; a sealed inbox
    // is fine, the other end is ending and this end's entry reads its plane.
    let item = InboxItem::Link {
        state,
        from: PeerRef {
            actor_id: ctx.actor_id().clone(),
            incarnation: own,
        },
        plane: ctx.self_handle().status_rx(),
    };
    rx.record().push_link(item);
    Ok(())
}

/// `ctx.unlink`.
pub(crate) fn unlink<A: Actor, T: IntoMonitorTarget>(ctx: &mut ActorContext<A>, target: T) -> bool {
    let resolved = resolve_target(&target, &ctx.resolved_system());
    let Some(peer) = resolved.incarnation else {
        return false;
    };
    let own = ctx.stop_lane().abort_record().incarnation();
    let Some(done) = ctx.link_table().unlink(peer) else {
        return false;
    };
    if !done.cancelled {
        return false;
    }
    if let Some(partner) = done.partner {
        let lower = if own < peer {
            ctx.stop_lane().abort_record().clone()
        } else {
            partner.clone()
        };
        lower.remove_pair(own, peer, &done.state);
        partner.push_unlink(own, done.state);
    }
    true
}

/// `ctx.trap_exit`.
pub(crate) fn trap_exit<A: Actor>(ctx: &mut ActorContext<A>, map: Arc<TrapFn<A::Message>>) {
    let owner = ctx.owner();
    ctx.trap_cell().set(&owner, map);
    // As many exit signals may wait for a trapping actor as its mailbox
    // holds; the bound is written before the flag lets the first one in.
    let cap = ctx.self_handle().mailbox_capacity();
    let record = ctx.stop_lane().abort_record();
    record.set_exit_cap(cap);
    record.set_trap(true);
}

/// `ctx.untrap_exit`: the flag first, the map after, so a task that reads the
/// flag set finds a map or degrades. A supervisor's flag stays set.
pub(crate) fn untrap_exit<A: Actor>(ctx: &mut ActorContext<A>) {
    if ctx.supervision_ref().is_none() {
        ctx.stop_lane().abort_record().set_trap(false);
    }
    ctx.trap_cell().clear();
}

/// The run loop's system branch took `SystemMessage::Signals`: installs the
/// links, applies the cancels and acts on the exit signals queued, in order.
pub(crate) fn drain_signals<A: Actor>(ctx: &mut ActorContext<A>) {
    let record = ctx.stop_lane().abort_record().clone();
    let items = record.take_signals();
    for item in items {
        match item {
            InboxItem::Link { state, from, plane } => install(ctx, state, from, plane),
            InboxItem::Unlink { from, state } => {
                ctx.link_table().remove_cancelled(from, &state);
            }
            InboxItem::Exit {
                from,
                reason,
                priority,
            } => {
                // The push counted the signal; the slot gives the count back
                // on whichever path the signal takes from here.
                let slot = ExitSlot(record.clone());
                exit_signal(ctx, from, reason, priority, slot);
            }
        }
    }
}

/// Installs this end's entry of a link the other end made.
fn install<A: Actor>(
    ctx: &mut ActorContext<A>,
    state: Arc<LinkState>,
    from: PeerRef,
    plane: StatusRx,
) {
    if !state.is_active() || ctx.link_table().contains(from.incarnation) {
        return;
    }
    let own = ctx.stop_lane().abort_record().incarnation();
    let lower = if own < from.incarnation {
        ctx.stop_lane().abort_record().clone()
    } else {
        plane.record().clone()
    };
    add_entry(
        ctx,
        Spec {
            state,
            side: LinkState::side(own, from.incarnation),
            plane: Some(plane),
            peer_id: from.actor_id,
            peer_incarnation: Some(from.incarnation),
            noproc: false,
            signal: None,
            late: false,
            lower: Some(lower),
            slot: None,
        },
    );
}

/// Acts on an exit signal taken from the inbox. The flag is read now: an
/// actor that stopped trapping since the signal was queued takes the signal
/// as one that does not trap. `slot` holds the signal's count against this
/// actor's bound: it moves into the delivery's task, or drops here.
fn exit_signal<A: Actor>(
    ctx: &mut ActorContext<A>,
    from: PeerRef,
    reason: StopReason,
    priority: bool,
    slot: ExitSlot,
) {
    let trapping = ctx.stop_lane().abort_record().trap();
    let mapped = trapping && ctx.trap_cell().get().is_some();
    if mapped {
        let state = Arc::new(LinkState::new());
        state.set_priority(0, priority);
        add_entry(
            ctx,
            Spec {
                state,
                side: 0,
                plane: None,
                peer_id: from.actor_id,
                peer_incarnation: Some(from.incarnation),
                noproc: false,
                signal: Some(reason),
                late: false,
                lower: None,
                slot: Some(slot),
            },
        );
        return;
    }
    drop(slot);
    if trapping && ctx.supervision_ref().is_some() {
        return;
    }
    let ended = Ended::Signal(reason);
    let handle = ctx.self_handle().clone();
    let system = ctx.resolved_system();
    end_owner(&handle, &system, &ended, &from.actor_id);
}

impl<A: Actor> crate::actor::monitor::MonitorState<A> {
    /// The links this actor holds.
    pub(crate) fn links(&mut self) -> &mut LinkTable {
        &mut self.links
    }

    /// Whether a link entry of this id is held.
    pub(crate) fn holds_link(&self, id: MonitorId) -> bool {
        self.links.contains_id(id)
    }

    /// The exit an ending actor owes for a closed entry, made with the trap
    /// map that stands. `None`: the entry is gone, the link was cancelled or
    /// acted on elsewhere meanwhile, or no map stands.
    pub(crate) fn link_owed_message(
        &mut self,
        id: MonitorId,
        exit: LinkExit,
    ) -> Option<Result<A::Message, String>> {
        let entry = self.links.entries.get(&id)?;
        if !entry.shared.take_closed() {
            return None;
        }
        if !entry.state.fire() {
            return None;
        }
        self.exit_message(exit)
    }

    /// Makes the actor's message from an exit with the trap map that stands.
    pub(crate) fn exit_message(&self, exit: LinkExit) -> Option<Result<A::Message, String>> {
        let map = self.trap.get()?;
        let made = run_trap(&*map, exit);
        drop_map(map, &self.owner);
        Some(made)
    }
}

#[cfg(test)]
mod late_entry_tests {
    use super::*;

    fn held() -> StopReason {
        StopReason::Failure(ActorError::user("carried by a Kill"))
    }

    /// The entry of a spawned run that had ended before its link was made
    /// owes the reason the monitor half reads from the same plane. The plane
    /// closed with no reason written and a `Kill` on its lane that carries a
    /// cause: the exit and the wait for the final reason both give the cause,
    /// never a plain `Kill`. The entry is not a link: it is not counted, not
    /// found by the other end's incarnation, and `unlink` finds nothing.
    #[tokio::test]
    async fn a_late_entry_owes_the_cause_a_killed_run_carried() {
        let (lane, _rx) = StopLane::new();
        let record = lane.abort_record().clone();
        let (tx, mut plane) = record.status_channel();
        lane.try_raise_kill_caused(held());
        drop(tx);

        let peer = record.incarnation();
        let entry = LinkEntry {
            shared: Arc::new(MonitorShared::unwatched(MonitorId::next())),
            state: Arc::new(LinkState::new()),
            token: CancellationToken::new(),
            peer_id: ActorId::from("late-kid"),
            peer_incarnation: Some(peer),
            plane: Some(plane.clone()),
            noproc: false,
            signal: None,
            late: true,
        };
        let owed = entry.owed().expect("the run has ended");
        assert_eq!(owed.reason, DownReason::Stopped(held()));
        assert_eq!(wait_final_reason(&mut plane).await, held());

        let mut table = LinkTable::default();
        table.insert(MonitorId::next(), entry);
        assert_eq!(table.len(), 0, "a late entry is not counted as a link");
        assert!(!table.contains(peer), "a late entry is not found by peer");
        assert!(table.unlink(peer).is_none(), "unlink finds nothing");
        assert!(!table.is_empty(), "the exit is still owed");
    }
}
