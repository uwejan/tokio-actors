//! Actor system providing named actor registration and coordinated shutdown.
//!
//! Internal module; the public pieces are re-exported at the crate root.

use std::any::{Any, TypeId};
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use dashmap::DashMap;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant;

use crate::actor::handle::ActorHandle;
use crate::actor::runtime::saturating_deadline;
use crate::actor::supervision::KILL_GRACE;
use crate::actor::Actor;
use crate::error::{SendError, SpawnError};
use crate::types::{ActorId, ActorStatus, ShutdownReport, StopLane, StopOutcome, StopReason};

// ---------------------------------------------------------------------------
// Global systems registry
// ---------------------------------------------------------------------------

static SYSTEMS: OnceLock<DashMap<String, Arc<ActorSystem>>> = OnceLock::new();

fn systems() -> &'static DashMap<String, Arc<ActorSystem>> {
    SYSTEMS.get_or_init(DashMap::new)
}

// ---------------------------------------------------------------------------
// ShutdownPolicy
// ---------------------------------------------------------------------------

/// Policy controlling how [`ActorSystem::shutdown`] behaves.
#[derive(Debug, Clone)]
pub struct ShutdownPolicy {
    /// Maximum wall-clock time for the entire shutdown sequence.
    /// After this deadline, all remaining actors are force-stopped.
    ///
    /// Default: 30 seconds.
    pub timeout: Duration,
    /// Maximum time to wait for each individual actor to stop gracefully.
    /// If an actor doesn't stop within this period, it receives `StopReason::Kill`.
    ///
    /// Default: 5 seconds.
    pub per_actor_timeout: Duration,
}

impl Default for ShutdownPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            per_actor_timeout: Duration::from_secs(5),
        }
    }
}

/// Configuration for creating a new [`ActorSystem`].
#[derive(Debug, Clone, Default)]
pub struct SystemConfig {
    /// Shutdown policy for this system.
    pub shutdown_policy: ShutdownPolicy,
}

// ---------------------------------------------------------------------------
// Type-erased actor handle
// ---------------------------------------------------------------------------

struct AnyActorHandle {
    type_id: TypeId,
    handle: Box<dyn Any + Send + Sync>,
    /// Clone of the actor's stop lane. Enough on its own to signal a stop or
    /// kill without downcasting, and cheap to clone out of a `DashMap` `Ref`
    /// before an `.await` (raising it is synchronous and infallible anyway).
    stop_lane: StopLane,
    /// Runtime status plane, threaded straight through from the
    /// `ActorHandle`: the watch channel is created before the actor's task
    /// is spawned, so this is always available at registration time (unlike
    /// `abort`, below).
    status_rx: watch::Receiver<ActorStatus>,
    /// The task's abort handle. `None` for the brief window between
    /// registration (which happens before the task is spawned, so the
    /// name-claim is visible pre-spawn) and the post-spawn `attach_abort`
    /// call; treated as not-yet-abortable by a shutdown sweep that lands in
    /// that window.
    abort: Option<AbortHandle>,
    /// True for actors registered via the top-level `SpawnBuilder` path;
    /// false for supervised children (`spawn_child` and its restart path).
    /// System shutdown only signals roots directly - each root's own
    /// supervision cascade takes its subtree down in turn.
    is_root: bool,
    /// Monotonic registration counter (deliberately not a wall-clock
    /// timestamp): shutdown stops roots in reverse registration order, and a
    /// counter is race-free where two registrations landing in the same
    /// clock tick would not be.
    registration_seq: u64,
}

impl AnyActorHandle {
    fn new<A: Actor>(handle: &ActorHandle<A>, is_root: bool, registration_seq: u64) -> Self {
        Self {
            type_id: TypeId::of::<ActorHandle<A>>(),
            handle: Box::new(handle.clone()),
            stop_lane: handle.stop_lane(),
            status_rx: handle.status_rx(),
            abort: None,
            is_root,
            registration_seq,
        }
    }

    fn downcast<A: Actor>(&self) -> Option<ActorHandle<A>> {
        if self.type_id == TypeId::of::<ActorHandle<A>>() {
            self.handle.downcast_ref::<ActorHandle<A>>().cloned()
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
    name: Option<String>,
    /// The registration this guard owns. Removal on drop is checked against
    /// this sequence (see [`ActorSystem::unregister_by_id`]/
    /// [`ActorSystem::unregister_by_name`]) so a stale teardown - one whose
    /// guard is only dropping now, well after a fresh registration already
    /// reused the same id/name - can never remove that newer registration
    /// out from under it. Defense in depth for the death-event race: `by_id`
    /// has no occupancy check on insert (unlike `by_name`), so an anonymous
    /// child's id can otherwise be silently reused before the old guard
    /// drops.
    registration_seq: u64,
}

impl RegistryGuard {
    pub(crate) fn new(
        system: Arc<ActorSystem>,
        id: ActorId,
        name: Option<String>,
        registration_seq: u64,
    ) -> Self {
        Self {
            system,
            id,
            name,
            registration_seq,
        }
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        self.system
            .unregister_by_id(&self.id, self.registration_seq);
        if let Some(name) = &self.name {
            self.system.unregister_by_name(name, self.registration_seq);
        }
    }
}

// ---------------------------------------------------------------------------
// SystemPhase
// ---------------------------------------------------------------------------

/// Lifecycle phase of an [`ActorSystem`]'s registry.
///
/// `Active` accepts registrations. `shutdown`/`shutdown_with` moves the
/// system to `ShuttingDown` immediately and to `Defunct` once every root has
/// been stopped; both reject new registrations with
/// [`SpawnError::SystemShuttingDown`]. Unlike a plain one-way flag, `Defunct`
/// is not permanent: [`ActorSystem::reactivate`] moves it back to `Active`,
/// so a system's name is never poisoned forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemPhase {
    Active,
    ShuttingDown,
    Defunct,
}

impl SystemPhase {
    const fn as_u8(self) -> u8 {
        match self {
            SystemPhase::Active => 0,
            SystemPhase::ShuttingDown => 1,
            SystemPhase::Defunct => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            0 => SystemPhase::Active,
            1 => SystemPhase::ShuttingDown,
            _ => SystemPhase::Defunct,
        }
    }
}

// ---------------------------------------------------------------------------
// ActorSystem
// ---------------------------------------------------------------------------

/// A named actor registry with coordinated shutdown.
///
/// `ActorSystem` is a phone book, not a runtime. It does not create or own a
/// Tokio runtime. Actors spawn on whatever runtime is current via
/// `Handle::try_current()`.
pub struct ActorSystem {
    name: String,
    by_name: DashMap<String, AnyActorHandle>,
    by_id: DashMap<ActorId, AnyActorHandle>,
    shutdown_policy: ShutdownPolicy,
    /// `Active` until `shutdown`/`shutdown_with` is called (moves to
    /// `ShuttingDown`, then to `Defunct` once every root has stopped).
    /// `register_actor` rejects every registration attempt in either
    /// non-`Active` phase with `SpawnError::SystemShuttingDown` (OTP
    /// application-controller parity). Not a one-way flag:
    /// [`ActorSystem::reactivate`] moves a `Defunct` system back to `Active`.
    phase: AtomicU8,
    /// Source of `AnyActorHandle::registration_seq`.
    registration_seq: AtomicU64,
    /// Source of every child incarnation token this system ever hands out -
    /// fresh spawns and restarts alike, so no two incarnations of any child,
    /// under any name, in any supervisor's registry, are ever equal (see
    /// [`next_incarnation`](Self::next_incarnation)).
    incarnation_seq: AtomicU64,
    /// This system's reaper feed, spawned lazily on first use (see
    /// [`reaper_handle`](Self::reaper_handle)).
    reaper: OnceLock<mpsc::Sender<ReaperEntry>>,
}

impl std::fmt::Debug for ActorSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorSystem")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl ActorSystem {
    // - Construction -------------------------------------------------------

    fn empty(name: String, shutdown_policy: ShutdownPolicy) -> Self {
        Self {
            name,
            by_name: DashMap::new(),
            by_id: DashMap::new(),
            shutdown_policy,
            phase: AtomicU8::new(SystemPhase::Active.as_u8()),
            registration_seq: AtomicU64::new(0),
            incarnation_seq: AtomicU64::new(0),
            reaper: OnceLock::new(),
        }
    }

    /// Returns the default system (named `"default"`), creating it lazily.
    ///
    /// This is intentionally not `std::default::Default` because it returns
    /// `Arc<ActorSystem>` (shared ownership), not an owned value.
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> Arc<ActorSystem> {
        let reg = systems();
        reg.entry("default".to_string())
            .or_insert_with(|| {
                Arc::new(Self::empty(
                    "default".to_string(),
                    ShutdownPolicy::default(),
                ))
            })
            .value()
            .clone()
    }

    /// Creates a new named system and registers it in the global systems map.
    ///
    /// Returns [`SpawnError::SystemNameTaken`] if a system with this name
    /// already exists.
    pub fn create(name: impl Into<String>) -> Result<Arc<ActorSystem>, SpawnError> {
        let name = name.into();
        let reg = systems();
        match reg.entry(name.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(SpawnError::SystemNameTaken(name)),
            dashmap::mapref::entry::Entry::Vacant(v) => {
                let system = Arc::new(Self::empty(name, ShutdownPolicy::default()));
                v.insert(system.clone());
                Ok(system)
            }
        }
    }

    /// Creates a new named system with custom configuration.
    ///
    /// Returns [`SpawnError::SystemNameTaken`] if a system with this name
    /// already exists.
    pub fn create_with(
        name: impl Into<String>,
        config: SystemConfig,
    ) -> Result<Arc<ActorSystem>, SpawnError> {
        let name = name.into();
        let reg = systems();
        match reg.entry(name.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(SpawnError::SystemNameTaken(name)),
            dashmap::mapref::entry::Entry::Vacant(v) => {
                let system = Arc::new(Self::empty(name, config.shutdown_policy));
                v.insert(system.clone());
                Ok(system)
            }
        }
    }

    /// Looks up a named system. Returns `None` if no system with this name exists.
    pub fn get_named(name: &str) -> Option<Arc<ActorSystem>> {
        systems().get(name).map(|entry| entry.value().clone())
    }

    /// Lists the names of all registered systems.
    pub fn all() -> Vec<String> {
        systems().iter().map(|e| e.key().clone()).collect()
    }

    /// Returns this system's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    // - Actor lookup -------------------------------------------------------

    /// Looks up a named actor, returning a typed handle.
    ///
    /// Returns `None` if the name is not registered or if the registered actor
    /// is a different type (type mismatch is silent, matching OTP's
    /// `whereis/1 -> undefined` semantics).
    pub fn get<A: Actor>(&self, name: &str) -> Option<ActorHandle<A>> {
        self.by_name.get(name).and_then(|e| e.downcast::<A>())
    }

    /// Looks up an actor by [`ActorId`], returning a typed handle.
    ///
    /// Returns `None` if the ID is not registered or if the registered actor
    /// is a different type (type mismatch is silent, matching OTP's
    /// `whereis/1 -> undefined` semantics - the same contract as
    /// [`get`](Self::get), keyed by ID instead of by name).
    ///
    /// An `ActorId` is stable across restarts: a supervised child keeps the
    /// same ID from incarnation to incarnation even though its `ActorHandle`
    /// is replaced underneath it, so an ID captured once (for example from
    /// [`ChildInfo::id`](crate::types::ChildInfo)) remains a valid lookup key
    /// for the lifetime of the child's spec.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult, ActorSystem};
    ///
    /// #[derive(Default)]
    /// struct Counter(i64);
    ///
    /// impl Actor for Counter {
    ///     type Message = i64;
    ///     type Response = i64;
    ///     async fn handle(&mut self, msg: i64, _ctx: &mut ActorContext<Self>) -> ActorResult<i64> {
    ///         self.0 += msg;
    ///         Ok(self.0)
    ///     }
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let handle = Counter::default().spawn().named("counter").await?;
    ///     let id = handle.id().clone();
    ///
    ///     let sys = ActorSystem::default();
    ///     let by_id = sys.get_by_id::<Counter>(&id).expect("registered above");
    ///     assert_eq!(by_id.send(1).await?, 1);
    ///     Ok(())
    /// }
    /// ```
    pub fn get_by_id<A: Actor>(&self, id: &ActorId) -> Option<ActorHandle<A>> {
        self.by_id.get(id).and_then(|e| e.downcast::<A>())
    }

    /// Stops a named actor gracefully.
    ///
    /// Delivered through the actor's stop lane (see the `runtime` module
    /// docs): a synchronous, infallible signal observed ahead of the system
    /// channel and the mailbox alike, at the actor's next turn boundary.
    ///
    /// # Errors
    /// - [`SendError::NotFound`] if no actor is registered under `name`.
    /// - [`SendError::Closed`] if an actor was registered under `name` but
    ///   has already stopped.
    pub async fn stop(&self, name: &str) -> Result<(), SendError> {
        // Clone the lane out of the `Ref` and drop the guard before checking
        // it: holding a `DashMap` guard across an await point can deadlock
        // against a concurrent writer on the same shard (the lane check
        // itself is synchronous, but this keeps the same discipline as every
        // other method here).
        let lane = {
            let entry = self.by_name.get(name).ok_or(SendError::NotFound)?;
            entry.stop_lane.clone()
        };
        if lane.is_closed() {
            return Err(SendError::Closed);
        }
        lane.raise(StopReason::Graceful);
        Ok(())
    }

    /// Force-kills a named actor, bypassing all lifecycle callbacks.
    ///
    /// Same stop-lane delivery as [`stop`](Self::stop), at `Kill` severity.
    ///
    /// # Errors
    /// - [`SendError::NotFound`] if no actor is registered under `name`.
    /// - [`SendError::Closed`] if an actor was registered under `name` but
    ///   has already stopped.
    pub async fn kill(&self, name: &str) -> Result<(), SendError> {
        // Same guard-before-check discipline as `stop`.
        let lane = {
            let entry = self.by_name.get(name).ok_or(SendError::NotFound)?;
            entry.stop_lane.clone()
        };
        if lane.is_closed() {
            return Err(SendError::Closed);
        }
        lane.raise(StopReason::Kill);
        Ok(())
    }

    /// Lists all registered actor names in this system.
    pub fn registered(&self) -> Vec<String> {
        self.by_name.iter().map(|e| e.key().clone()).collect()
    }

    // - Visibility -----------------------------------------------------------

    /// Snapshot of every actor id currently registered in this system,
    /// named or anonymous. Enumeration parity with `erlang:processes/0`
    /// (erlang.org/docs/28): every actor is visible through its system,
    /// regardless of how it was spawned.
    pub fn actor_ids(&self) -> Vec<ActorId> {
        self.by_id.iter().map(|e| e.key().clone()).collect()
    }

    /// Reads an actor's current status by id. Returns `None` if the id is
    /// not registered in this system.
    pub fn actor_status(&self, id: &ActorId) -> Option<ActorStatus> {
        self.by_id.get(id).map(|e| *e.status_rx.borrow())
    }

    /// Force-kills an actor by id, bypassing all lifecycle callbacks -
    /// untrappable, matching `erlang:exit(Pid, kill)` (erlang.org/docs/28).
    /// Returns `false` if the id is not registered. Runs the same
    /// `Kill -> grace -> abort -> grace` ladder as the shutdown sweep,
    /// rather than a bare `Kill` send.
    pub async fn kill_by_id(&self, id: &ActorId) -> bool {
        // Guard-before-await discipline, as in `stop`/`kill`: clone the
        // pieces `force_stop` needs out of the `DashMap` guard before any
        // `.await`.
        let (status_rx, stop_lane, abort) = match self.by_id.get(id) {
            Some(e) => (
                Some(e.status_rx.clone()),
                Some(e.stop_lane.clone()),
                e.abort.clone(),
            ),
            None => return false,
        };
        force_stop(status_rx, stop_lane, abort).await;
        true
    }

    // - Internal registration (used by spawn path) --------------------------

    /// Registers a spawned actor, returning its registration sequence
    /// number (the source of [`RegistryGuard`]'s seq-checked removal).
    /// `is_root` distinguishes a top-level (`SpawnBuilder`) spawn, which
    /// system shutdown signals directly, from a supervised child
    /// (`spawn_child`), which is taken down by its own supervisor's shutdown
    /// cascade instead.
    ///
    /// Returns [`SpawnError::SystemShuttingDown`] unless the system is
    /// currently `Active`: OTP's application controller rejects new
    /// registrations while stopping, and (unlike a one-way flag) this also
    /// covers a `Defunct` system that has not yet been
    /// [`reactivate`](Self::reactivate)d.
    pub(crate) fn register_actor<A: Actor>(
        &self,
        id: &ActorId,
        name: Option<&str>,
        handle: &ActorHandle<A>,
        is_root: bool,
    ) -> Result<u64, SpawnError> {
        if SystemPhase::from_u8(self.phase.load(Ordering::Acquire)) != SystemPhase::Active {
            return Err(SpawnError::SystemShuttingDown(self.name.clone()));
        }

        let seq = self.registration_seq.fetch_add(1, Ordering::Relaxed);
        if let Some(n) = name {
            let entry = self.by_name.entry(n.to_string());
            match entry {
                dashmap::mapref::entry::Entry::Occupied(_) => {
                    return Err(SpawnError::NameTaken {
                        name: n.to_string(),
                        system: self.name.clone(),
                    });
                }
                dashmap::mapref::entry::Entry::Vacant(v) => {
                    v.insert(AnyActorHandle::new(handle, is_root, seq));
                }
            }
        }
        self.by_id
            .insert(id.clone(), AnyActorHandle::new(handle, is_root, seq));
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

    /// Same seq-checked removal as [`unregister_by_id`](Self::unregister_by_id), for `by_name`.
    pub(crate) fn unregister_by_name(&self, name: &str, registration_seq: u64) {
        self.by_name
            .remove_if(name, |_, entry| entry.registration_seq == registration_seq);
    }

    /// Draws the next child incarnation token for this system: every
    /// registration a supervisor ever makes for a child - the initial spawn
    /// included, not just restarts - draws from this single monotonic
    /// counter. No two incarnations of any child anywhere in this system ever
    /// collide, so a stale watcher completion (from a superseded instance,
    /// including one that shared its predecessor's name after a
    /// terminate/delete/respawn cycle) can never be mistaken for the fresh
    /// one's death.
    pub(crate) fn next_incarnation(&self) -> u64 {
        self.incarnation_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Moves a `Defunct` system (one whose `shutdown`/`shutdown_with` has
    /// fully completed) back to `Active`, so its name can be registered into
    /// again instead of staying poisoned forever.
    ///
    /// Returns `false` without effect if the system is not currently
    /// `Defunct` - either still `Active` (nothing to reactivate), or
    /// `ShuttingDown` (a shutdown is still in flight, and reactivating out
    /// from under it would race that shutdown's own final phase transition).
    pub fn reactivate(&self) -> bool {
        self.phase
            .compare_exchange(
                SystemPhase::Defunct.as_u8(),
                SystemPhase::Active.as_u8(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Attaches the task's abort handle to the just-created registry entries.
    /// Called once, immediately after `handle.spawn(...)`: the `AbortHandle`
    /// does not exist before then, so registration (the name-claim) and this
    /// attach step are necessarily two steps, with a brief window between
    /// them where the entry's `abort` is `None` (see [`AnyActorHandle::abort`]).
    pub(crate) fn attach_abort(&self, id: &ActorId, name: Option<&str>, abort: AbortHandle) {
        if let Some(mut entry) = self.by_id.get_mut(id) {
            entry.abort = Some(abort.clone());
        }
        if let Some(name) = name {
            if let Some(mut entry) = self.by_name.get_mut(name) {
                entry.abort = Some(abort);
            }
        }
    }

    /// Returns this system's reaper feed, spawning its backing task on first
    /// use. Every supervised child's link guard feeds a deadline into it on
    /// drop (see `ChildLinkGuard` in the `supervision` module); the task
    /// itself never blocks anything and aborts a child's task only if it has
    /// not exited by its scheduled deadline.
    pub(crate) fn reaper_handle(&self, rt: &Handle) -> mpsc::Sender<ReaperEntry> {
        self.reaper
            .get_or_init(|| {
                let (tx, rx) = mpsc::channel(REAPER_CHANNEL_CAPACITY);
                rt.spawn(reaper_loop(rx));
                tx
            })
            .clone()
    }

    // - Shutdown -------------------------------------------------------------

    /// Shuts down all registered actors using the system's stored policy.
    pub async fn shutdown(&self) -> ShutdownReport {
        self.shutdown_with(self.shutdown_policy.clone()).await
    }

    /// Shuts down the system: stops every ROOT actor (one spawned via the
    /// top-level `SpawnBuilder`, not a supervised child) in reverse
    /// registration order, sequentially, escalating from
    /// `StopReason::ParentRequest` up through `Kill` and finally a task abort
    /// as needed. Each root's own supervision cascade takes its
    /// subtree down in turn - the system never signals a child directly, so a
    /// supervisor mid-restart when shutdown begins is never raced, and its
    /// children are never restarted (the parent's own message loop, the only
    /// thing that would evaluate a restart, has already exited by the time
    /// its children are stopped).
    ///
    /// New registrations are rejected with `SpawnError::SystemShuttingDown`
    /// from the moment this is called (OTP application-controller parity).
    ///
    /// Bounded by `policy.timeout`: once the global deadline passes, every
    /// remaining root is force-stopped (`Kill` + `abort`) CONCURRENTLY instead
    /// of sequentially, so this always returns close to the deadline
    /// regardless of how many roots are still outstanding. A defensive final
    /// sweep force-stops anything still left in the registry afterward
    /// (normally nothing: supervised children vanish via their own guard
    /// drops during the cascade above).
    ///
    /// Registry entries are removed as their actors die (the existing
    /// `RegistryGuard` drop machinery) - never cleared up front, so a lookup
    /// racing the early part of shutdown still finds a live actor.
    pub async fn shutdown_with(&self, policy: ShutdownPolicy) -> ShutdownReport {
        self.phase
            .store(SystemPhase::ShuttingDown.as_u8(), Ordering::Release);
        let deadline = saturating_deadline(Instant::now(), policy.timeout);

        let mut roots = self.snapshot_roots();
        // Reverse registration order: most-recently-registered stops first.
        roots.sort_by_key(|r| std::cmp::Reverse(r.registration_seq));

        let mut outcomes: Vec<(ActorId, StopOutcome)> = Vec::with_capacity(roots.len());
        let mut cut = roots.len();
        for (idx, root) in roots.iter().enumerate() {
            match self
                .stop_root_sequential(
                    &root.id,
                    root.status_rx.clone(),
                    policy.per_actor_timeout,
                    deadline,
                )
                .await
            {
                Some(outcome) => outcomes.push((root.id.clone(), outcome)),
                None => {
                    // The global deadline was hit mid-ladder: this root and
                    // everything after it (in stop order) move to the
                    // concurrent sweep below instead of continuing one at a
                    // time.
                    cut = idx;
                    break;
                }
            }
        }

        if cut < roots.len() {
            let stragglers: Vec<ActorId> = roots[cut..].iter().map(|r| r.id.clone()).collect();
            outcomes.extend(self.concurrent_force_sweep(&stragglers).await);
        }

        // Defensive final sweep: anything still registered that wasn't
        // already reported above. Normally empty - supervised children
        // vanish via their own guard drops as part of each root's cascade.
        // These are non-root leftovers, so they are reported in `swept`,
        // never `outcomes`.
        let reported: HashSet<ActorId> = outcomes.iter().map(|(id, _)| id.clone()).collect();
        let leftover: Vec<ActorId> = self
            .by_id
            .iter()
            .map(|e| e.key().clone())
            .filter(|id| !reported.contains(id))
            .collect();
        let mut swept: Vec<(ActorId, StopOutcome)> = Vec::new();
        if !leftover.is_empty() {
            swept.extend(self.concurrent_force_sweep(&leftover).await);
        }

        // Shutdown has fully completed: move to `Defunct` rather than
        // leaving the one-way `ShuttingDown` flag set forever, so
        // `reactivate` can return this system's name to service.
        self.phase
            .store(SystemPhase::Defunct.as_u8(), Ordering::Release);

        ShutdownReport { outcomes, swept }
    }

    /// Snapshots every currently-registered ROOT actor's id, registration
    /// sequence, and a clone of its status receiver. The snapshot is
    /// independent of the registry entry from this point on: the `status_rx`
    /// clone stays valid even after the entry is removed by a
    /// `RegistryGuard` drop.
    fn snapshot_roots(&self) -> Vec<RootSnapshot> {
        self.by_id
            .iter()
            .filter(|e| e.is_root)
            .map(|e| RootSnapshot {
                id: e.key().clone(),
                registration_seq: e.registration_seq,
                status_rx: e.status_rx.clone(),
            })
            .collect()
    }

    /// Runs one root through the escalation ladder (`ParentRequest` -> `Kill`
    /// -> `abort()`), each wait bounded by whichever is tighter: the tier's
    /// own bound, or the time left before the global deadline. Returns `None`
    /// the instant the global deadline is hit without having reached a
    /// terminal status - the caller then hands this root (and every root
    /// after it) to the concurrent force sweep instead of continuing the
    /// sequential ladder.
    async fn stop_root_sequential(
        &self,
        id: &ActorId,
        mut status_rx: watch::Receiver<ActorStatus>,
        per_actor_timeout: Duration,
        deadline: Instant,
    ) -> Option<StopOutcome> {
        // Tier 1: ParentRequest (vetoable via pre_stop).
        self.send_stop(id, StopReason::ParentRequest);
        if wait_terminal(&mut status_rx, time_left(deadline).min(per_actor_timeout)).await {
            return Some(StopOutcome::Graceful);
        }
        if Instant::now() >= deadline {
            return None;
        }

        // Tier 2: Kill (unvetoable, but still only observed cooperatively -
        // an actor stuck inside a callback never reaches the select! that
        // would see it).
        self.send_stop(id, StopReason::Kill);
        if wait_terminal(&mut status_rx, time_left(deadline).min(KILL_GRACE)).await {
            return Some(StopOutcome::Killed);
        }
        if Instant::now() >= deadline {
            return None;
        }

        // Tier 3: abort() - the last resort for an actor that never yielded
        // back to the run loop (so Kill was never even observed).
        self.abort_by_id(id);
        if wait_terminal(&mut status_rx, time_left(deadline).min(KILL_GRACE)).await {
            return Some(StopOutcome::Aborted);
        }

        // Nothing left to hand off to: the concurrent sweep would do exactly
        // what tiers 2-3 just did. A genuinely non-yielding task.
        Some(StopOutcome::Unresponsive)
    }

    /// Force-stops a batch of roots CONCURRENTLY: `Kill` -> grace -> `abort()`
    /// -> grace, skipping the vetoable `ParentRequest` tier entirely. Used
    /// once the global deadline is breached (remaining roots) and for the
    /// defensive leftover sweep - in both cases graceful is no longer worth
    /// waiting for. Every id is reported exactly once.
    async fn concurrent_force_sweep(&self, ids: &[ActorId]) -> Vec<(ActorId, StopOutcome)> {
        let mut set = JoinSet::new();
        for id in ids.iter().cloned() {
            let (status_rx, stop_lane, abort) = match self.by_id.get(&id) {
                Some(e) => (
                    Some(e.status_rx.clone()),
                    Some(e.stop_lane.clone()),
                    e.abort.clone(),
                ),
                None => (None, None, None),
            };
            set.spawn(async move {
                let outcome = force_stop(status_rx, stop_lane, abort).await;
                (id, outcome)
            });
        }

        let mut results = Vec::with_capacity(ids.len());
        while let Some(joined) = set.join_next().await {
            if let Ok(pair) = joined {
                results.push(pair);
            }
        }
        results
    }

    /// Clones the lane out of the map guard and drops the guard before
    /// raising it. A missing entry (the actor already died on its own) is a
    /// silent no-op: the caller's subsequent status wait finds it already
    /// terminal.
    fn send_stop(&self, id: &ActorId, reason: StopReason) {
        if let Some(lane) = self.by_id.get(id).map(|e| e.stop_lane.clone()) {
            lane.raise(reason);
        }
    }

    fn abort_by_id(&self, id: &ActorId) {
        if let Some(entry) = self.by_id.get(id) {
            if let Some(abort) = &entry.abort {
                abort.abort();
            }
        }
    }
}

/// Snapshot of a root actor taken at the start of shutdown: independent of
/// the registry entry (the `status_rx` clone stays valid even after
/// `RegistryGuard` removes the entry).
struct RootSnapshot {
    id: ActorId,
    registration_seq: u64,
    status_rx: watch::Receiver<ActorStatus>,
}

/// Time remaining until `deadline`, floored at zero.
fn time_left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Waits, bounded by `bound`, for `status_rx` to reach a terminal state:
/// `ActorStatus::Stopped`, or the sender dropped without ever writing it (an
/// aborted task - terminal too, matching `ActorHandle::wait_stopped`).
/// Returns `true` if terminal was observed within `bound`.
async fn wait_terminal(status_rx: &mut watch::Receiver<ActorStatus>, bound: Duration) -> bool {
    tokio::time::timeout(bound, async {
        while *status_rx.borrow() != ActorStatus::Stopped {
            if status_rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await
    .is_ok()
}

/// Force-stop path shared by the concurrent sweep: `Kill` -> grace -> `abort`
/// -> grace. A `None` `status_rx` means the id was never actually found in
/// the registry (already gone by the time the sweep looked) - reported
/// `Graceful` since there is nothing left to do.
async fn force_stop(
    status_rx: Option<watch::Receiver<ActorStatus>>,
    stop_lane: Option<StopLane>,
    abort: Option<AbortHandle>,
) -> StopOutcome {
    let Some(mut status_rx) = status_rx else {
        return StopOutcome::Graceful;
    };

    if let Some(lane) = &stop_lane {
        lane.raise(StopReason::Kill);
    }
    if wait_terminal(&mut status_rx, KILL_GRACE).await {
        return StopOutcome::Killed;
    }

    if let Some(abort) = &abort {
        abort.abort();
    }
    if wait_terminal(&mut status_rx, KILL_GRACE).await {
        return StopOutcome::Aborted;
    }

    StopOutcome::Unresponsive
}

// ---------------------------------------------------------------------------
// Reaper - the abort-on-deadline backstop fed by every child link guard
// ---------------------------------------------------------------------------

/// Capacity of the reaper's feed channel. Internal, not user-configurable: a
/// full channel just makes the feeding guard abort its child immediately
/// instead of scheduling a delayed abort (see `ChildLinkGuard::drop` in the
/// `supervision` module) - always at least as prompt, so this only trades a
/// later deadline for an earlier one under contention, never a lost one.
const REAPER_CHANNEL_CAPACITY: usize = 1024;

/// One pending abort deadline, fed by a dying child's link guard.
pub(crate) struct ReaperEntry {
    deadline: Instant,
    abort: AbortHandle,
}

impl ReaperEntry {
    pub(crate) fn new(deadline: Instant, abort: AbortHandle) -> Self {
        Self { deadline, abort }
    }
}

impl PartialEq for ReaperEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for ReaperEntry {}

impl PartialOrd for ReaperEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for ReaperEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        // Reversed so a `BinaryHeap` (a max-heap by default) pops the
        // SOONEST deadline first, turning it into a min-heap by deadline.
        other.deadline.cmp(&self.deadline)
    }
}

/// One task per [`ActorSystem`], lazily spawned on first use: aborts a
/// child's task if it has not exited by its guard-scheduled deadline.
/// Entries only ever arrive from a link guard's `Drop` - a synchronous,
/// non-blocking `try_send` - and are never awaited by anything: a full or
/// closed feed just means the guard aborts its child immediately instead.
async fn reaper_loop(mut rx: mpsc::Receiver<ReaperEntry>) {
    let mut heap: BinaryHeap<ReaperEntry> = BinaryHeap::new();
    let mut channel_open = true;

    loop {
        if !channel_open {
            match heap.pop() {
                Some(due) => {
                    tokio::time::sleep_until(due.deadline).await;
                    if !due.abort.is_finished() {
                        due.abort.abort();
                    }
                }
                None => break,
            }
            continue;
        }

        match heap.peek() {
            Some(next) => {
                let deadline = next.deadline;
                tokio::select! {
                    entry = rx.recv() => match entry {
                        Some(e) => heap.push(e),
                        None => channel_open = false,
                    },
                    _ = tokio::time::sleep_until(deadline) => {
                        if let Some(due) = heap.pop() {
                            if !due.abort.is_finished() {
                                due.abort.abort();
                            }
                        }
                    }
                }
            }
            None => match rx.recv().await {
                Some(e) => heap.push(e),
                None => channel_open = false,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests (pub(crate) internals, Rust Book Ch 11.3)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[derive(Default)]
    struct Dummy;

    impl Actor for Dummy {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    fn dummy_handle(id: &ActorId, system_name: &str) -> ActorHandle<Dummy> {
        let (tx, _rx) = mpsc::channel(1);
        let (system_tx, _system_rx) = mpsc::channel(1);
        let (_status_tx, status_rx) = watch::channel(ActorStatus::Running);
        let (stop_lane, _stop_rx) = StopLane::new();
        ActorHandle::new(
            id.clone(),
            tx,
            system_tx,
            stop_lane,
            1,
            status_rx,
            system_name.into(),
        )
    }

    // Reproduces the death-event race defended against by `RegistryGuard`'s
    // seq-checked removal: `by_id` has no occupancy check on insert (unlike
    // `by_name`), so an anonymous child's id can be silently reused by a
    // fresh registration before an older, still-tearing-down instance's
    // guard drops. That stale guard must not remove the newer entry.
    #[tokio::test]
    async fn stale_registry_guard_drop_never_removes_a_newer_registration() {
        let sys = ActorSystem::create(format!("seq-guard-{}", uuid::Uuid::new_v4())).unwrap();
        let id = ActorId::from("anon-child");

        let old_handle = dummy_handle(&id, sys.name());
        let old_seq = sys
            .register_actor::<Dummy>(&id, None, &old_handle, false)
            .unwrap();
        let old_guard = RegistryGuard::new(sys.clone(), id.clone(), None, old_seq);

        // A newer registration lands under the SAME id before the old
        // guard drops.
        let new_handle = dummy_handle(&id, sys.name());
        let new_seq = sys
            .register_actor::<Dummy>(&id, None, &new_handle, false)
            .unwrap();
        assert_ne!(old_seq, new_seq);

        drop(old_guard);

        let entry = sys
            .by_id
            .get(&id)
            .expect("the stale guard's drop must not remove the newer registration");
        assert_eq!(entry.registration_seq, new_seq);
    }

    #[tokio::test]
    async fn matching_seq_guard_drop_removes_its_own_registration() {
        let sys = ActorSystem::create(format!("seq-guard-{}", uuid::Uuid::new_v4())).unwrap();
        let id = ActorId::from("solo-child");

        let handle = dummy_handle(&id, sys.name());
        let seq = sys
            .register_actor::<Dummy>(&id, None, &handle, false)
            .unwrap();
        let guard = RegistryGuard::new(sys.clone(), id.clone(), None, seq);

        drop(guard);

        assert!(
            sys.by_id.get(&id).is_none(),
            "a guard whose seq still matches the live registration must remove it"
        );
    }

    // - Reaper ----------------------------------------------------------------

    #[tokio::test]
    async fn reaper_aborts_a_child_that_outlives_its_deadline() {
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(reaper_loop(rx));

        let join = tokio::spawn(std::future::pending::<()>());
        let abort = join.abort_handle();
        tx.send(ReaperEntry::new(
            Instant::now() + Duration::from_millis(30),
            abort,
        ))
        .await
        .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), join).await;
        match result {
            Ok(Err(err)) => assert!(err.is_cancelled(), "expected a cancelled join error"),
            other => {
                panic!("expected the reaper to abort the child by its deadline, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn reaper_never_disturbs_a_child_that_already_exited() {
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(reaper_loop(rx));

        let join = tokio::spawn(async { 7u32 });
        let abort = join.abort_handle();
        // Let the task actually finish before the reaper ever hears about it.
        assert_eq!(join.await.unwrap(), 7);

        tx.send(ReaperEntry::new(Instant::now(), abort))
            .await
            .unwrap();
        // Give the reaper a moment to process the (already-moot) deadline;
        // an already-finished task's `abort()` is a no-op either way, so
        // there is nothing further to observe here beyond "does not panic".
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn reaper_pops_the_soonest_deadline_first() {
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(reaper_loop(rx));

        let late = tokio::spawn(std::future::pending::<()>());
        let soon = tokio::spawn(std::future::pending::<()>());

        // Fed out of order: the later deadline is enqueued first.
        tx.send(ReaperEntry::new(
            Instant::now() + Duration::from_millis(300),
            late.abort_handle(),
        ))
        .await
        .unwrap();
        tx.send(ReaperEntry::new(
            Instant::now() + Duration::from_millis(30),
            soon.abort_handle(),
        ))
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(2), soon)
            .await
            .expect("soon must be reaped by its own, earlier deadline")
            .expect_err("soon must be cancelled, not completed");
        assert!(
            !late.is_finished(),
            "late's deadline has not arrived yet; it must still be running"
        );
        late.abort();
    }
}
