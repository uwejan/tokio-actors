//! The registry of a system: its entries, the guard that ends a registration, identity resolution and registration.

use std::any::{Any, TypeId};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::runtime::Handle;
use tokio::task::AbortHandle;

use super::{ActorSystem, MonitorKey, SystemPhase};
use crate::actor::handle::ActorHandle;
use crate::actor::incarnation::{
    IncarnationRef, StatusRx, StatusTx, StopLane, SubtreeHold, TurnClock,
};
use crate::actor::Actor;
use crate::error::SpawnError;
use crate::types::{ActorId, Incarnation, StopReason};

// ---------------------------------------------------------------------------
// Type-erased actor handle
// ---------------------------------------------------------------------------

/// The key of a registry entry: the actor's id. A named actor's id is its
/// name, so the one map is read by id and by name alike; both borrows hash as
/// the id does.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct RegKey(pub(super) ActorId);

impl std::borrow::Borrow<ActorId> for RegKey {
    fn borrow(&self) -> &ActorId {
        &self.0
    }
}

impl std::borrow::Borrow<str> for RegKey {
    fn borrow(&self) -> &str {
        self.0.as_str()
    }
}

pub(super) struct AnyActorHandle {
    type_id: TypeId,
    /// Whether the actor was spawned under a name, which is then its id. Only
    /// a named entry is found by a lookup by name.
    pub(super) named: bool,
    pub(super) handle: Box<dyn StoredHandle>,
    /// Clone of the actor's stop lane. Enough on its own to signal a stop or
    /// kill without downcasting, and cheap to clone out of a `DashMap` `Ref`
    /// before an `.await` (raising it is synchronous and never waits for capacity anyway).
    pub(super) stop_lane: StopLane,
    /// Runtime status plane, threaded straight through from the
    /// `ActorHandle`: the cell is created before the actor's task
    /// is spawned, so this is always available at registration time (unlike
    /// `abort`, below).
    pub(super) status_rx: StatusRx,
    /// This actor's turn clock, handed in at registration time (the clock
    /// lives with the run loop and this registry entry, never with
    /// `ActorHandle`): backs [`ActorSystem::turn_elapsed`] and
    /// [`ActorSystem::stuck_actors`] without downcasting.
    pub(super) turn_clock: TurnClock,
    /// The task's abort handle. `None` for the brief window between
    /// registration (which happens before the task is spawned, so the entry
    /// is visible pre-spawn) and the post-spawn `attach_abort` call; treated as not-yet-abortable by a shutdown sweep that lands in
    /// that window.
    pub(super) abort: Option<AbortHandle>,
    /// `None` for actors registered via the top-level `SpawnBuilder` path;
    /// for supervised children (`spawn_child` and its restart path) the id
    /// and incarnation number of the supervisor that spawned them, by which
    /// the child's telemetry identity finds its supervisor's. System shutdown
    /// walks the roots; each root's own supervision cascade takes its subtree
    /// down in turn.
    pub(super) parent: Option<(ActorId, u64)>,
    /// The registered run's incarnation number, drawn once from the process
    /// counter when the run's block is made (deliberately not a wall-clock
    /// timestamp): shutdown stops roots in reverse order of it, and a counter
    /// is race-free where two spawns landing in the same clock tick would not
    /// be. Two spawns that overlap in time are ordered by the order their
    /// blocks were made.
    pub(super) registration_seq: u64,
    /// The time this actor is given, after a `Kill` is raised on it, to
    /// reach its next turn boundary before its task is aborted. Every kill
    /// path that finds the actor through the registry reads it here.
    pub(super) kill_grace: Duration,
    /// The runtime that drives this actor's task. An abort takes effect only
    /// when that runtime next polls the task, so the kill backstop is
    /// scheduled on this runtime's reaper and on no other.
    pub(super) runtime: Handle,
    /// The earliest deadline any `Kill` raised on this incarnation asked for,
    /// in nanoseconds since the system's epoch; `0` when none did. Every
    /// raiser records its deadline here before it raises, so an attach that
    /// finds the `Kill` already raised arms the abort for the earliest of
    /// them. See [`raise_kill_declared`].
    pub(super) kill_due: u64,
}

/// What a registry entry tells about its incarnation: id, incarnation number,
/// root flag, turn clock id, actor type name and whether it is named.
type EntryFacts = (
    ActorId,
    u64,
    Option<(ActorId, u64)>,
    u64,
    &'static str,
    bool,
);

/// The numbers a snapshot reads from an actor's handle: its type name and the
/// fill of its two lanes. Each is one or two atomic reads; nothing waits and
/// no actor is asked.
#[derive(Clone, Copy, Debug)]
pub(super) struct MailView {
    pub(super) type_name: &'static str,
    #[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
    pub(super) mailbox_len: usize,
    #[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
    pub(super) mailbox_capacity: usize,
    #[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
    pub(super) priority_len: usize,
    #[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
    pub(super) priority_capacity: usize,
}

/// What the registry keeps of an actor's handle: the handle itself, for a
/// typed lookup, and the view of its mailbox, which only code that knows the
/// actor type can build. The view is a method of the stored object's own
/// table, so an entry holds no pointer for it.
pub(super) trait StoredHandle: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn mail_view(&self) -> MailView;
}

impl<A: Actor> StoredHandle for ActorHandle<A> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn mail_view(&self) -> MailView {
        MailView {
            type_name: std::any::type_name::<A>(),
            mailbox_len: self.mailbox_len(),
            mailbox_capacity: self.mailbox_capacity(),
            priority_len: self.priority_len(),
            priority_capacity: self.priority_capacity(),
        }
    }
}

impl AnyActorHandle {
    fn new<A: Actor>(
        handle: &ActorHandle<A>,
        named: bool,
        parent: Option<(ActorId, u64)>,
        registration_seq: u64,
        turn_clock: TurnClock,
        kill_grace: Duration,
        runtime: Handle,
    ) -> Self {
        Self {
            type_id: TypeId::of::<ActorHandle<A>>(),
            named,
            handle: Box::new(handle.clone()),
            stop_lane: handle.stop_lane(),
            status_rx: handle.status_rx(),
            turn_clock,
            abort: None,
            parent,
            registration_seq,
            kill_grace,
            runtime,
            kill_due: 0,
        }
    }

    /// Keeps the earlier of the recorded deadline and `window` (or the kill
    /// grace) after `now`.
    pub(super) fn declare_kill(&mut self, now: u64, window: Option<Duration>) {
        let after = window.unwrap_or(self.kill_grace);
        let due = now.saturating_add(u64::try_from(after.as_nanos()).unwrap_or(u64::MAX));
        if self.kill_due == 0 || due < self.kill_due {
            self.kill_due = due;
        }
    }

    pub(super) fn downcast<A: Actor>(&self) -> Option<ActorHandle<A>> {
        if self.type_id == TypeId::of::<ActorHandle<A>>() {
            self.handle
                .as_any()
                .downcast_ref::<ActorHandle<A>>()
                .cloned()
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// RegistryGuard - drop-based auto-unregister
// ---------------------------------------------------------------------------

pub(crate) struct RegistryGuard {
    system: Arc<ActorSystem>,
    id: ActorId,
    /// The registration this guard owns. Removal on drop is checked against
    /// this sequence (see [`ActorSystem::unregister_by_id`]) so a stale
    /// teardown - one whose guard is only dropping now, well after a fresh
    /// registration already reused the same id, which is a named actor's
    /// name - can never remove that newer registration out from under it. `by_id` refuses a second live registration of an
    /// id, so the sequence check is defense for a stale guard.
    registration_seq: u64,
    /// A sender of the actor's status plane. Released after `drop`
    /// unregistered the actor: on every path that drops the guard last, the
    /// plane reads as ended only once the name is free.
    _status: Option<StatusTx>,
    /// The actor's hold on its subtree token. Declared last, so it is
    /// released after `drop` unregistered the actor and after the status
    /// sender: the subtree ends only once the name is free.
    _subtree: Option<Arc<SubtreeHold>>,
}

impl RegistryGuard {
    pub(crate) fn new(system: Arc<ActorSystem>, id: ActorId, registration_seq: u64) -> Self {
        Self {
            system,
            id,
            registration_seq,
            _status: None,
            _subtree: None,
        }
    }

    /// Makes the guard hold a sender of the actor's status plane as well.
    pub(crate) fn with_status(mut self, status: StatusTx) -> Self {
        self._status = Some(status);
        self
    }

    /// Makes the guard hold the actor's subtree token as well.
    pub(crate) fn with_subtree(mut self, subtree: Arc<SubtreeHold>) -> Self {
        self._subtree = Some(subtree);
        self
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        #[cfg(test)]
        crate::actor::runtime::end_hooks::run(crate::actor::runtime::end_hooks::NAME, &self.id);
        // An incarnation spawned while no sink took events and ending while
        // one does is identified now, while its registry entry still names
        // it: its end and what follows it are events about it.
        if self.system.telemetry_mode().any_armed() {
            if let Some(status) = &self._status {
                self.system
                    .resolve_identity(status.abort_record(), &self.id);
            }
        }
        // The end is recorded for the supervisor, which owes a decision on the
        // death, before the entry leaves the registry and ahead of the
        // runner's join. Read only while turn tracking is on.
        let mode = self.system.telemetry_mode();
        if mode.word() & crate::telemetry::CTL_TRACK_TURNS != 0 {
            if let Some(status) = &self._status {
                let own = crate::telemetry::identity_for(
                    mode,
                    status.abort_record(),
                    Some(&self.system),
                    Some(&self.id),
                );
                if let Some(supervisor) = own.as_deref().and_then(|own| own.parent()) {
                    status.abort_record().mark_guard_ended();
                    supervisor.death_ended(self.registration_seq);
                }
            }
        }
        self.system
            .unregister_by_id(&self.id, self.registration_seq);
        #[cfg(test)]
        crate::actor::runtime::end_hooks::run(
            crate::actor::runtime::end_hooks::NAME_FREED,
            &self.id,
        );
        // The run's memberships go before its status can read `Stopped`: the
        // run is marked ended first and its count read after, and a join
        // counts before it lists, so whichever of a join and this mark comes
        // second sees the other. A run that never joined pays this one swap.
        if let Some(status) = &self._status {
            let record = status.record();
            let joined = record.pg_mark_ended();
            #[cfg(test)]
            crate::actor::runtime::end_hooks::run(
                crate::actor::runtime::end_hooks::PG_MARKED,
                &self.id,
            );
            if joined != 0 {
                self.system
                    .pg_remove_member(&(self.id.clone(), record.incarnation()));
            }
        }
    }
}

/// Shard count of `ActorSystem::by_id`.
///
/// A `DashMap` grows one shard at a time: the shard that passes seven eighths
/// of its table doubles it while holding its write lock, so every register,
/// lookup and unregister on that shard waits for the rehash. The default shard
/// count follows the CPUs the process may use (a process limited to one CPU
/// gets 4 shards), which makes each rehash cover a quarter of the registry.
/// A constant count keeps each rehash to `1 / REGISTRY_SHARDS` of the entries
/// whatever the affinity: a table growth holds one shard's write lock while it
/// rehashes only that shard's entries, so a spawn that waits on a rehash waits
/// for a small share of the table, never for the whole of a large one.
/// Registration still goes through one shard's
/// entry API, so the uniqueness check of an id, which is a named actor's name,
/// is unchanged. The count is a power of two, as `DashMap` requires.
pub(super) const REGISTRY_SHARDS: usize = 256;

/// An empty registry map with `REGISTRY_SHARDS` shards and no preallocated
/// table. An idle `ActorSystem` holds the shard locks and empty shard tables
/// of its registry map, a fixed cost at any CPU count.
pub(super) fn registry_map<K: Eq + std::hash::Hash, V>() -> DashMap<K, V> {
    DashMap::with_capacity_and_shard_amount(0, REGISTRY_SHARDS)
}

impl ActorSystem {
    /// The registry entry that holds `name`: the entry under the id `name`,
    /// when it was registered under a name. One guard is held, and an
    /// anonymous actor whose id reads as `name` is no such name.
    pub(super) fn by_name(
        &self,
        name: &str,
    ) -> Option<dashmap::mapref::one::Ref<'_, RegKey, AnyActorHandle>> {
        self.by_id.get(name).filter(|entry| entry.named)
    }

    /// Resolves the key of a monitor target in this system: the registered
    /// actor's run, or the absence of one. A key that matches nothing keeps
    /// the id the key names (a name is its actor's id) and has no run.
    pub(crate) fn resolve_monitor_target(&self, key: MonitorKey<'_>) -> ResolvedTarget {
        let (actor_id, found) = match key {
            MonitorKey::Id(id) => (
                id.clone(),
                self.by_id
                    .get(id)
                    .map(|e| (e.status_rx.clone(), e.stop_lane.clone())),
            ),
            MonitorKey::Name(name) => (
                ActorId::from(name),
                self.by_name(name)
                    .map(|e| (e.status_rx.clone(), e.stop_lane.clone())),
            ),
        };
        let (status_rx, lane) = match found {
            Some((rx, lane)) => (Some(rx), Some(lane)),
            None => (None, None),
        };
        ResolvedTarget {
            actor_id,
            incarnation: status_rx.as_ref().map(|rx| rx.record().incarnation()),
            status_rx,
            lane,
        }
    }

    // - Internal registration (used by spawn path) --------------------------

    /// What registering `name` would answer right now for a system that is
    /// not taking registrations or a key already held, read without
    /// registering anything: the refusal the registration gives for that
    /// occupant, [`SpawnError::NameTaken`] for a named one and
    /// [`SpawnError::DuplicateChild`] for an anonymous one whose id reads as
    /// `name`. The registration itself decides again.
    pub(crate) fn precheck_spawn(&self, name: Option<&str>) -> Result<(), SpawnError> {
        if SystemPhase::from_u8(self.phase.load(Ordering::Acquire)) != SystemPhase::Active {
            return Err(SpawnError::SystemShuttingDown(self.name.clone()));
        }
        if let Some(n) = name {
            let occupant = self.by_id.get(n).map(|entry| entry.named);
            match occupant {
                Some(true) => {
                    return Err(SpawnError::NameTaken {
                        name: n.to_string(),
                        system: self.name.clone(),
                    });
                }
                Some(false) => return Err(SpawnError::DuplicateChild(ActorId::from(n))),
                None => {}
            }
        }
        Ok(())
    }

    /// Registers a spawned actor, returning its registration sequence
    /// number: the incarnation number the actor's handle carries, unique in
    /// the process (the source of [`RegistryGuard`]'s seq-checked removal).
    /// `is_root` distinguishes a top-level (`SpawnBuilder`) spawn, which the
    /// system shutdown walks, from a supervised child (`spawn_child`), which
    /// its supervisor's cascade or the final sweep takes down; such a test
    /// child names no supervisor the registry holds.
    ///
    /// Returns [`SpawnError::SystemShuttingDown`] unless the system is
    /// currently `Active`: OTP's application controller rejects new
    /// registrations while stopping, and (unlike a one-way flag) this also
    /// covers a `Defunct` system that has not yet been
    /// [`reactivate`](Self::reactivate)d.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_actor<A: Actor>(
        &self,
        id: &ActorId,
        named: bool,
        handle: &ActorHandle<A>,
        is_root: bool,
        turn_clock: TurnClock,
        kill_grace: Duration,
        runtime: Handle,
    ) -> Result<u64, SpawnError> {
        let seq = handle.incarnation().number();
        // A hand-built child names a supervisor the registry never holds, so
        // its identity, like that of a child whose supervisor has ended, is
        // never guessed.
        let parent = (!is_root).then(|| (ActorId::from(""), crate::telemetry::UNKNOWN_INCARNATION));
        self.register_actor_as(
            id, named, handle, parent, turn_clock, kill_grace, runtime, seq,
        )
    }

    /// Registers the incarnation numbered `seq`, which the caller drew from
    /// [`next_incarnation`](Self::next_incarnation). The number is the
    /// registration's sequence: shutdown stops roots in reverse order of it,
    /// and a stale guard's removal is checked against it. `parent` is `None`
    /// for a root and the spawning supervisor's id and incarnation number for
    /// a supervised child. `named` says the actor was spawned under a name,
    /// which is then its id.
    ///
    /// An id already held is refused: with [`SpawnError::NameTaken`] when this
    /// registration and the occupant are both named, otherwise with
    /// [`SpawnError::DuplicateChild`]. A refusal changes nothing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_actor_as<A: Actor>(
        &self,
        id: &ActorId,
        named: bool,
        handle: &ActorHandle<A>,
        parent: Option<(ActorId, u64)>,
        turn_clock: TurnClock,
        kill_grace: Duration,
        runtime: Handle,
        seq: u64,
    ) -> Result<u64, SpawnError> {
        // One run has one number: the registration's sequence is the
        // incarnation the handle's record carries.
        debug_assert_eq!(seq, handle.incarnation().number());
        // A handle a unit test built by hand belongs to the system it is
        // registered in; every spawn has recorded its system already, and the
        // record is set once.
        #[cfg(test)]
        if let Some(me) = self.me.upgrade() {
            handle.stop_lane_ref().abort_record().set_system(&me);
        }
        // Held from the phase check through the inserts (see
        // `registration_gate`). Nothing in this section awaits or panics.
        let gate = self
            .registration_gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if SystemPhase::from_u8(self.phase.load(Ordering::Acquire)) != SystemPhase::Active {
            return Err(SpawnError::SystemShuttingDown(self.name.clone()));
        }
        #[cfg(test)]
        {
            let hook = self.after_phase_check.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }

        // One insert under the shard's write lock: a lookup by id or by name
        // finds the whole entry or nothing, and a refusal inserts nothing.
        let occupant = match self.by_id.entry(RegKey(id.clone())) {
            dashmap::mapref::entry::Entry::Occupied(held) => Some(held.get().named),
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(AnyActorHandle::new(
                    handle, named, parent, seq, turn_clock, kill_grace, runtime,
                ));
                None
            }
        };
        match occupant {
            Some(true) if named => {
                return Err(SpawnError::NameTaken {
                    name: id.as_str().to_string(),
                    system: self.name.clone(),
                });
            }
            Some(_) => return Err(SpawnError::DuplicateChild(id.clone())),
            None => {}
        }
        drop(gate);

        // Start the sweep when something asks for it and none is running.
        // The spawn path resolves a runtime via `Handle::try_current()`
        // before ever reaching here, so one is normally current - but
        // `try_current()` is used here too rather than the panicking
        // `Handle::current()`, so a registration reached some other way, with
        // no runtime current, quietly skips the spawn instead of panicking.
        // Whatever asks later makes its own attempt; either way, the next
        // call with a runtime current retries.
        self.ensure_sweep();

        Ok(seq)
    }

    /// Removes the `by_id` entry for `id`, but only if it is still the
    /// registration identified by `registration_seq`. A stale
    /// [`RegistryGuard`] whose drop is only running now - well after a
    /// fresh registration already reused this id - must never remove that
    /// newer entry (see the field doc on `RegistryGuard::registration_seq`).
    pub(crate) fn unregister_by_id(&self, id: &ActorId, registration_seq: u64) {
        self.by_id
            .remove_if(id, |_, entry| entry.registration_seq == registration_seq);
    }

    /// The identity of the incarnation behind `record`, the actor `id`,
    /// recorded now when its spawn recorded none: an incarnation spawned
    /// while no sink took events has an entry in the registry and nothing in
    /// the telemetry table, so the first event about it (or about a child
    /// that names it as supervisor) reads its number and name from its
    /// registry entry and its supervisor from the entry its own entry names
    /// by id and incarnation number, records the result and returns it.
    /// Returns `None` for an incarnation the registry does not hold under
    /// `id` (it ended, or no spawn made it), and for a supervised child whose
    /// supervisor's incarnation the registry does not hold: nothing is
    /// guessed. Every lookup is by id; no registry guard is held when this
    /// returns, so the caller may push at once.
    pub(crate) fn resolve_identity(
        &self,
        record: &IncarnationRef,
        id: &ActorId,
    ) -> Option<Arc<crate::telemetry::Identity>> {
        let mode = self.telemetry_mode();
        let key = record.key();
        if let Some(found) = mode.find(key) {
            return Some(found);
        }
        if record.has_stopped() {
            return None;
        }
        let (id, incarnation, parent, _clock, type_name, named) = self.entry_of_record(key, id)?;
        // A named actor's name is its id, read from the same entry.
        let name = named.then(|| Arc::clone(id.shared()));
        let parent = match parent {
            None => None,
            Some((parent_id, parent_seq)) => {
                Some(self.resolve_identity_of(&parent_id, parent_seq)?)
            }
        };
        // The wait token is the incarnation number, the same value a spawn
        // that recorded the identity would give, so a wait cycle through an
        // actor spawned before any sink or switch is reported like any other.
        let identity = Arc::new(
            crate::telemetry::Identity::new(id, incarnation, parent, name, None)
                .with_type(type_name),
        );
        Some(mode.bind_if_absent(key, identity))
    }

    /// [`resolve_identity`](Self::resolve_identity) for the live actor
    /// registered under `id`.
    pub(crate) fn resolve_identity_of_id(
        &self,
        id: &ActorId,
    ) -> Option<Arc<crate::telemetry::Identity>> {
        let record = self
            .by_id
            .get(id)
            .map(|entry| entry.stop_lane.abort_record().clone())?;
        self.resolve_identity(&record, id)
    }

    /// [`resolve_identity`](Self::resolve_identity) for the incarnation
    /// numbered `seq` of the actor `id`; `None` when the registry holds
    /// another incarnation under `id`, or none.
    pub(crate) fn resolve_identity_of(
        &self,
        id: &ActorId,
        seq: u64,
    ) -> Option<Arc<crate::telemetry::Identity>> {
        let record = self
            .by_id
            .get(id)
            .filter(|entry| entry.registration_seq == seq)
            .map(|entry| entry.stop_lane.abort_record().clone())?;
        self.resolve_identity(&record, id)
    }

    /// The pass of an engagement: records the identity of every incarnation
    /// the registry holds, once per engagement (see
    /// [`Mode::bind_once`](crate::telemetry::Mode::bind_once)). The entries
    /// are copied out in one walk, each shard's guard dropped once it is
    /// read, and resolved by id after the walk, outside every guard; an
    /// incarnation already recorded costs one lookup of the table. Never call
    /// it under a registry guard or with the registry mutex of the sinks held.
    pub(crate) fn bind_registered(&self) {
        let mode = self.telemetry_mode();
        mode.bind_once(|| {
            #[cfg(test)]
            self.identity_walks.fetch_add(1, Ordering::Relaxed);
            let rows: Vec<(ActorId, IncarnationRef)> = self
                .by_id
                .iter()
                .map(|entry| {
                    (
                        entry.key().0.clone(),
                        entry.stop_lane.abort_record().clone(),
                    )
                })
                .collect();
            for (id, record) in rows {
                let _ = self.resolve_identity(&record, &id);
            }
        });
    }

    /// How many passes walked the registry for identities.
    #[cfg(test)]
    pub(crate) fn identity_walks(&self) -> usize {
        self.identity_walks.load(Ordering::Relaxed)
    }

    /// The id, incarnation number, supervisor, turn clock id, actor type name
    /// and named flag of the registry entry under `id`, when its stop lane's record is
    /// at `key`. The registry guard is dropped before this returns.
    fn entry_of_record(&self, key: usize, id: &ActorId) -> Option<EntryFacts> {
        let entry = self.by_id.get(id)?;
        (entry.stop_lane.abort_record().key() == key).then(|| {
            (
                id.clone(),
                entry.registration_seq,
                entry.parent.clone(),
                entry.turn_clock.id(),
                entry.handle.mail_view().type_name,
                entry.named,
            )
        })
    }

    /// Attaches the task's abort handle to the just-created registry entry.
    /// Called once, immediately after `handle.spawn(...)`: the `AbortHandle`
    /// does not exist before then, so registration (the entry's insert) and
    /// this attach step are necessarily two steps, with a brief window between
    /// them where the entry's `abort` is `None` (see [`AnyActorHandle::abort`]).
    ///
    /// A `Kill` raised in that window found no abort handle to schedule: the
    /// caller raises and then reads the entry's abort, this stores the abort
    /// and then reads the lane, and both go through locks, so at least one
    /// of them sees the other's write. When the lane already holds a `Kill`,
    /// this arms the backstop itself, after the earliest deadline a raiser
    /// declared on the entry before its raise (see
    /// [`raise_kill_declared`]), or after the entry's own kill
    /// grace when none did; a deadline already passed arms it at zero.
    ///
    /// Only the entry of this spawn's own registration, `registration_seq`,
    /// takes the handle, checked under the same entry lock as the write: an
    /// entry a later registration made under the same id (after this
    /// incarnation ended and freed it) keeps its own abort handle, and no
    /// backstop is armed through it. A named actor has this one entry too, so
    /// a lookup by name reads the same handle.
    pub(crate) fn attach_abort(&self, id: &ActorId, abort: AbortHandle, registration_seq: u64) {
        let mut armed_with: Option<(StopLane, Duration, Handle)> = None;
        if let Some(mut entry) = self.by_id.get_mut(id) {
            if entry.registration_seq == registration_seq {
                entry.abort = Some(abort.clone());
                armed_with = Some((
                    entry.stop_lane.clone(),
                    entry.kill_grace,
                    entry.runtime.clone(),
                ));
            }
        }
        if let Some((lane, grace, runtime)) = armed_with {
            if matches!(lane.reason(), Some(StopReason::Kill)) {
                // The deadline is read after the lane: every raiser
                // declared its own before its raise, so the earliest of
                // those whose `Kill` this read saw is in the entry.
                let due = self.declared_kill_due(id, registration_seq);
                let after = match due {
                    0 => grace,
                    due => Duration::from_nanos(due.saturating_sub(self.kill_clock())),
                };
                self.arm_kill_backstop(abort, lane.abort_record(), after, &runtime);
            }
        }
    }
}

/// What a monitor key resolved to. `status_rx` is `None` when nothing is
/// registered under the key, and then there is no run to name.
#[derive(Debug)]
pub(crate) struct ResolvedTarget {
    pub(crate) actor_id: ActorId,
    pub(crate) incarnation: Option<Incarnation>,
    pub(crate) status_rx: Option<StatusRx>,
    /// The target's stop lane, from the registry entry or the handle that
    /// named it: `None` exactly when `status_rx` is.
    pub(crate) lane: Option<StopLane>,
}
