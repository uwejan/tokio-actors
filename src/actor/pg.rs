//! Process groups.
//!
//! A [`PgScope`] is a named table of groups, each group a set of members, an
//! actor run named by its id and its incarnation. Any holder of a handle of
//! the scope's system can join the actor it reaches to a group, leave it, and
//! list a group's members. An actor that joins one group twice is listed twice
//! and has to leave twice, as a process joined twice to an Erlang/OTP `pg`
//! group is.
//!
//! A scope belongs to one [`ActorSystem`]: [`ActorSystem::pg`] is the default
//! scope and [`ActorSystem::pg_scope`] a scope of any other name. A scope is
//! made on first use and lives as long as its system, through a shutdown and a
//! [`reactivate`](ActorSystem::reactivate). Only actors of the scope's system
//! can join it.
//!
//! A member is removed from every group when its run ends, before the actor's
//! status reads `Stopped`, so a read made after
//! [`wait_stopped`](crate::actor::handle::ActorHandle::wait_stopped) never
//! lists an actor that has ended. A restarted actor is a new incarnation and
//! belongs to no group until it joins. Changes to a group reach an actor that
//! watches it through [`ActorContext::monitor_group`] and
//! [`ActorContext::monitor_scope`].
//!
//! The scope's table sits behind one lock that only this module and the group
//! monitors take, and no user code runs under it: a read copies a shared list out of the lock and
//! makes the handles after it is released.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use crate::actor::handle::ActorHandle;
use crate::actor::panic::contain_waker_panic;
use crate::actor::pg_monitor::{SubscriptionList, Wakes};
use crate::actor::runtime::{end_value, log_late_drop};
use crate::actor::Actor;
use crate::error::PgError;
use crate::system::ActorSystem;
use crate::telemetry::Mode;
use crate::types::{ActorId, Incarnation, PeerRef};

#[cfg(doc)]
use crate::actor::context::ActorContext;

/// A member: one run of one actor.
pub(crate) type MemberKey = (ActorId, Incarnation);

/// What `ActorSystem::pg` names the default scope.
const DEFAULT_SCOPE: &str = "pg";

/// One member of a group, with how many times it joined.
#[derive(Clone)]
pub(crate) struct Slot {
    pub(crate) peer: PeerRef,
    pub(crate) count: u32,
    /// The typed `ActorHandle<A>` of the member, downcast by `members::<A>`.
    handle: Arc<dyn Any + Send + Sync>,
}

/// The members of one group. The list is shared with readers by `Arc`: a
/// reader takes a clone under the lock and builds its answer after releasing
/// it, and a change made while a reader still holds the list copies it once
/// (copy on write). Without a reader a change is `O(1)`: a member's place is
/// found through `index`, and a removal moves the last member into the gap.
#[derive(Default)]
pub(crate) struct Members {
    slots: Arc<Vec<Slot>>,
    index: HashMap<MemberKey, usize>,
}

/// What a removal of one join left.
enum Removed {
    /// The member joined more than once and still has joins left.
    Still,
    /// The member's last join is gone; its slot is returned so that it is
    /// dropped after the lock is released.
    Gone(Slot),
}

impl Members {
    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Adds one join of `key`. True when the member is new to the group.
    fn add(
        &mut self,
        key: &MemberKey,
        handle: impl FnOnce() -> Arc<dyn Any + Send + Sync>,
    ) -> bool {
        if let Some(at) = self.index.get(key) {
            Arc::make_mut(&mut self.slots)[*at].count += 1;
            return false;
        }
        let slots = Arc::make_mut(&mut self.slots);
        self.index.insert(key.clone(), slots.len());
        slots.push(Slot {
            peer: PeerRef {
                actor_id: key.0.clone(),
                incarnation: key.1,
            },
            count: 1,
            handle: handle(),
        });
        true
    }

    /// Removes one join of `key`; `None` when it has none here.
    fn remove_one(&mut self, key: &MemberKey) -> Option<Removed> {
        let at = *self.index.get(key)?;
        if self.slots[at].count > 1 {
            Arc::make_mut(&mut self.slots)[at].count -= 1;
            return Some(Removed::Still);
        }
        Some(Removed::Gone(self.take_slot(key, at)))
    }

    /// Removes the member with every join it holds.
    fn remove_all(&mut self, key: &MemberKey) -> Option<Slot> {
        let at = *self.index.get(key)?;
        Some(self.take_slot(key, at))
    }

    fn take_slot(&mut self, key: &MemberKey, at: usize) -> Slot {
        self.index.remove(key);
        let slots = Arc::make_mut(&mut self.slots);
        let slot = slots.swap_remove(at);
        if let Some(moved) = slots.get(at) {
            let moved_key = (moved.peer.actor_id.clone(), moved.peer.incarnation);
            self.index.insert(moved_key, at);
        }
        slot
    }

    /// The list as it is now, shared.
    pub(crate) fn snapshot(&self) -> Arc<Vec<Slot>> {
        self.slots.clone()
    }
}

/// The members a shared list names, each as many times as it joined.
pub(crate) fn peers_of(slots: &[Slot]) -> Vec<PeerRef> {
    let total = slots.iter().map(|slot| slot.count as usize).sum();
    let mut peers = Vec::with_capacity(total);
    for slot in slots {
        for _ in 0..slot.count {
            peers.push(slot.peer.clone());
        }
    }
    peers
}

/// Everything a scope holds, behind its one lock.
#[derive(Default)]
pub(crate) struct ScopeTable {
    pub(crate) groups: HashMap<Arc<str>, Members>,
    /// For each member, the groups it is in, so its end finds them without
    /// scanning the scope.
    by_member: HashMap<MemberKey, Vec<Arc<str>>>,
    pub(crate) subs: SubscriptionList,
}

impl ScopeTable {
    /// Adds one join and publishes it to the group's watchers.
    fn add(
        &mut self,
        group: Arc<str>,
        key: &MemberKey,
        handle: impl FnOnce() -> Arc<dyn Any + Send + Sync>,
        wakes: &mut Wakes,
    ) {
        let group = match self.groups.get_key_value(&*group) {
            Some((existing, _)) => existing.clone(),
            None => group,
        };
        let members = self.groups.entry(group.clone()).or_default();
        let fresh = members.add(key, handle);
        if fresh {
            self.by_member
                .entry(key.clone())
                .or_default()
                .push(group.clone());
        }
        let peer = PeerRef {
            actor_id: key.0.clone(),
            incarnation: key.1,
        };
        self.subs.publish_join(&group, &peer, wakes);
    }

    /// Removes one join and publishes it. `Err` when the member has none in
    /// the group; `Ok` holds the slot of a member that is gone from the
    /// group.
    fn leave(
        &mut self,
        group: &str,
        key: &MemberKey,
        wakes: &mut Wakes,
    ) -> Result<Option<Slot>, PgError> {
        let Some(members) = self.groups.get_mut(group) else {
            return Err(PgError::NotJoined);
        };
        let removed = members.remove_one(key).ok_or(PgError::NotJoined)?;
        let (name, _) = self
            .groups
            .get_key_value(group)
            .expect("the group was just read");
        let name = name.clone();
        let peer = PeerRef {
            actor_id: key.0.clone(),
            incarnation: key.1,
        };
        self.subs.publish_leave(&name, &peer, wakes);
        match removed {
            Removed::Still => Ok(None),
            Removed::Gone(slot) => {
                self.forget(group, key);
                Ok(Some(slot))
            }
        }
    }

    /// The member is out of `group`: drops the bookkeeping that names it, and
    /// the group itself when no member is left.
    fn forget(&mut self, group: &str, key: &MemberKey) {
        if let Some(list) = self.by_member.get_mut(key) {
            list.retain(|name| &**name != group);
            if list.is_empty() {
                self.by_member.remove(key);
            }
        }
        if self.groups.get(group).is_some_and(Members::is_empty) {
            self.groups.remove(group);
        }
    }

    /// Removes the member from every group, one `Leave` per join it held.
    /// Returns the slots removed, to be dropped after the lock is released.
    fn remove_member(&mut self, key: &MemberKey, wakes: &mut Wakes) -> Vec<Slot> {
        let Some(names) = self.by_member.remove(key) else {
            return Vec::new();
        };
        let peer = PeerRef {
            actor_id: key.0.clone(),
            incarnation: key.1,
        };
        let mut removed = Vec::with_capacity(names.len());
        for name in names {
            let Some(members) = self.groups.get_mut(&*name) else {
                continue;
            };
            if let Some(slot) = members.remove_all(key) {
                for _ in 0..slot.count {
                    self.subs.publish_leave(&name, &peer, wakes);
                }
                removed.push(slot);
            }
            if self.groups.get(&*name).is_some_and(Members::is_empty) {
                self.groups.remove(&*name);
            }
        }
        removed
    }
}

/// What a test runs when a `Resync` is built.
#[cfg(test)]
pub(crate) type ResyncProbe = Box<dyn Fn(&PgScopeInner) + Send + Sync>;

/// What a scope is made of; shared by every [`PgScope`] clone, the system's
/// table of scopes and the entry tasks of the monitors that watch it.
pub(crate) struct PgScopeInner {
    name: Arc<str>,
    system_name: Arc<str>,
    /// The telemetry state of the system the scope belongs to, which is the
    /// system's identity (see `AbortRecord::spawned_in`): a handle of another
    /// system, one of the same name included, is foreign to the scope. The
    /// weak reference holds no strong count and keeps the allocation, so a
    /// system created after this one is gone never has the same address.
    system: Weak<Mode>,
    table: Mutex<ScopeTable>,
    /// How many times the table's lock was taken.
    #[cfg(test)]
    pub(crate) locks: std::sync::atomic::AtomicUsize,
    /// How many `Resync` messages were built from this scope.
    #[cfg(test)]
    pub(crate) resyncs: std::sync::atomic::AtomicUsize,
    /// Runs each time a `Resync` is built, at the point where its members are
    /// copied.
    #[cfg(test)]
    pub(crate) resync_probe: Mutex<Option<ResyncProbe>>,
}

impl PgScopeInner {
    pub(crate) fn new(name: Arc<str>, system_name: Arc<str>, system: Weak<Mode>) -> Self {
        Self {
            name,
            system_name,
            system,
            table: Mutex::new(ScopeTable::default()),
            #[cfg(test)]
            locks: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            resyncs: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            resync_probe: Mutex::new(None),
        }
    }

    /// A `Resync` is being built from members copied out of this scope.
    #[cfg(test)]
    pub(crate) fn resync_built(&self) {
        self.resyncs
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(probe) = self.resync_probe.lock().unwrap().as_ref() {
            probe(self);
        }
    }

    /// The scope's table. A holder of the lock that unwound left the table
    /// consistent (every step of a change is a plain data change), so a
    /// poisoned lock is taken as it is.
    pub(crate) fn lock(&self) -> MutexGuard<'_, ScopeTable> {
        #[cfg(test)]
        self.locks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Removes the run `key` from every group it joined, as the run ends.
    pub(crate) fn remove_member(&self, key: &MemberKey) {
        let mut wakes = Wakes::new();
        let removed = {
            let mut table = self.lock();
            table.remove_member(key, &mut wakes)
        };
        wake_all(wakes);
        drop_slots(removed, &key.0);
    }
}

/// Wakes every entry task a change concerns, each under its own catch so one
/// waker's panic stops no other wake. Called after the scope's lock is
/// released.
pub(crate) fn wake_all(wakes: Wakes) {
    for notify in wakes.into_inner() {
        contain_waker_panic("pg change", None, || notify.notify_one());
    }
}

/// Drops the slots a removal took out, after the lock is released; the drop of
/// a handle is caught.
fn drop_slots(slots: Vec<Slot>, owner: &ActorId) {
    for slot in slots {
        if let Some(panic) = end_value(slot) {
            // The slot holds a member's handle, whose drop runs no code of
            // the member's: the text is the crate's, never the member's
            // failure, so the member's sensitivity does not apply.
            log_late_drop(
                &panic,
                &crate::actor::runtime::Owner::new(owner.clone(), false),
            );
        }
    }
}

/// A named table of process groups owned by one [`ActorSystem`].
///
/// Obtained from [`ActorSystem::pg`] or [`ActorSystem::pg_scope`]; cloning it
/// shares the scope. Joining and leaving are synchronous and never wait for
/// another actor. The order of [`members`](Self::members) is not specified.
///
/// Erlang/OTP: a `pg` scope, `pg:join/3`, `pg:leave/3`, `pg:get_members/2`
/// and `pg:which_groups/1`. Unlike `pg`, a scope belongs to one system, so a
/// handle of another system is refused, and a member that has ended is never
/// listed. There is no dispatch or broadcast helper: list the members and send
/// to them.
///
/// # Examples
/// ```rust
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     ActorResult, ActorSystem, PgError,
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
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let system = ActorSystem::create("pg-scope-example")?;
/// let worker = Idle.spawn().on_system(&system).await?;
/// let scope = system.pg();
/// scope.join("workers", &worker)?;
/// assert_eq!(scope.members::<Idle>("workers").len(), 1);
/// scope.leave("workers", &worker)?;
/// assert_eq!(scope.leave("workers", &worker), Err(PgError::NotJoined));
/// assert!(scope.groups().is_empty());
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct PgScope {
    inner: Arc<PgScopeInner>,
}

impl std::fmt::Debug for PgScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgScope")
            .field("name", &self.inner.name)
            .field("system", &self.inner.system_name)
            .finish_non_exhaustive()
    }
}

impl PgScope {
    pub(crate) fn from_inner(inner: Arc<PgScopeInner>) -> Self {
        Self { inner }
    }

    pub(crate) fn inner(&self) -> &Arc<PgScopeInner> {
        &self.inner
    }

    /// The scope's name; `"pg"` for [`ActorSystem::pg`].
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Adds the actor `handle` reaches to `group`, creating the group when it
    /// has no member. The member is this run of the actor: a restarted actor
    /// is a new run and has to join again. An actor that joins one group
    /// twice is listed twice and has to leave twice.
    ///
    /// # Errors
    /// - [`PgError::ForeignSystem`] when the actor belongs to another system
    ///   than the scope.
    /// - [`PgError::NoProc`] when the run has ended, or ends while the join
    ///   is made: whichever of the join and the end comes second, the member
    ///   is never listed after the end.
    pub fn join<A: Actor>(
        &self,
        group: impl Into<Arc<str>>,
        handle: &ActorHandle<A>,
    ) -> Result<(), PgError> {
        let record = handle.stop_lane_ref().abort_record();
        if !record.spawned_in(&self.inner.system) {
            return Err(PgError::ForeignSystem);
        }
        let key: MemberKey = (handle.id().clone(), record.incarnation());
        let group = group.into();
        let mut wakes = Wakes::new();
        {
            let mut table = self.inner.lock();
            // Counted under the lock and before the member is listed: the
            // guard that ends the run marks it ended first and reads the
            // count after, so a member this join adds is either seen by the
            // guard's sweep, which waits for this lock, or refused here.
            if !record.pg_join_begin() {
                return Err(PgError::NoProc);
            }
            table.add(group, &key, || Arc::new(handle.clone()), &mut wakes);
        }
        wake_all(wakes);
        Ok(())
    }

    /// Removes one join of the actor from `group`. The member leaves the
    /// group when its last join is gone.
    ///
    /// # Errors
    /// - [`PgError::NotJoined`] when the actor has no join in the group, an
    ///   ended actor included (an ended actor is in no group).
    /// - [`PgError::ForeignSystem`] when the actor belongs to another system
    ///   than the scope.
    pub fn leave<A: Actor>(&self, group: &str, handle: &ActorHandle<A>) -> Result<(), PgError> {
        let record = handle.stop_lane_ref().abort_record();
        if !record.spawned_in(&self.inner.system) {
            return Err(PgError::ForeignSystem);
        }
        let key: MemberKey = (handle.id().clone(), record.incarnation());
        let mut wakes = Wakes::new();
        let removed = {
            let mut table = self.inner.lock();
            let removed = table.leave(group, &key, &mut wakes)?;
            record.pg_join_end();
            removed
        };
        wake_all(wakes);
        drop_slots(removed.into_iter().collect(), &key.0);
        Ok(())
    }

    /// The members of `group` whose actor type is `A`, each as many times as
    /// it joined. The order is not specified. Members of another actor type
    /// are skipped; [`members_any`](Self::members_any) lists them all.
    pub fn members<A: Actor>(&self, group: &str) -> Vec<ActorHandle<A>> {
        let slots = {
            let table = self.inner.lock();
            table.groups.get(group).map(Members::snapshot)
        };
        let Some(slots) = slots else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for slot in slots.iter() {
            if let Some(handle) = slot.handle.downcast_ref::<ActorHandle<A>>() {
                for _ in 0..slot.count {
                    out.push(handle.clone());
                }
            }
        }
        out
    }

    /// Every member of `group`, whatever its actor type, each as many times as
    /// it joined. The order is not specified.
    pub fn members_any(&self, group: &str) -> Vec<PeerRef> {
        let slots = {
            let table = self.inner.lock();
            table.groups.get(group).map(Members::snapshot)
        };
        slots.map(|slots| peers_of(&slots)).unwrap_or_default()
    }

    /// The groups that have at least one member. The order is not specified.
    pub fn groups(&self) -> Vec<Arc<str>> {
        self.inner.lock().groups.keys().cloned().collect()
    }
}

impl ActorSystem {
    /// The system's default process-group scope, named `"pg"`, made on first
    /// use. Every call returns the same scope.
    pub fn pg(&self) -> PgScope {
        self.pg_scope(DEFAULT_SCOPE)
    }

    /// The process-group scope of this system named `name`, made on first
    /// use. Scopes of different names are independent, and so are scopes of
    /// different systems. A scope outlives a shutdown of its system and is
    /// still there after [`reactivate`](Self::reactivate).
    pub fn pg_scope(&self, name: impl Into<Arc<str>>) -> PgScope {
        let name = name.into();
        let mut scopes = self
            .pg_scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let inner = scopes
            .entry(name.clone())
            .or_insert_with(|| {
                Arc::new(PgScopeInner::new(
                    name,
                    Arc::from(self.name()),
                    Arc::downgrade(self.telemetry_mode()),
                ))
            })
            .clone();
        PgScope::from_inner(inner)
    }

    /// Removes the run `key` from every scope of this system, as the run
    /// ends. Only a run that joined a group gets here.
    pub(crate) fn pg_remove_member(&self, key: &MemberKey) {
        #[cfg(test)]
        self.pg_sweeps
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let scopes: Vec<Arc<PgScopeInner>> = {
            let scopes = self
                .pg_scopes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            scopes.values().cloned().collect()
        };
        for scope in scopes {
            scope.remove_member(key);
        }
    }
}

#[cfg(test)]
#[path = "pg_tests.rs"]
mod pg_tests;
