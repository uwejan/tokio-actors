//! Actor system providing named actor registration and coordinated shutdown.
//!
//! Internal module; the public pieces are re-exported at the crate root.

use std::any::{Any, TypeId};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinSet};

use crate::actor::handle::ActorHandle;
use crate::actor::supervision::KILL_GRACE;
use crate::actor::Actor;
use crate::error::{SendError, SpawnError};
use crate::types::{ActorId, ActorStatus, ShutdownReport, StopOutcome, StopReason, SystemMessage};

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
    /// Clone of the actor's system-channel sender. Enough on its own to
    /// signal `Stop` without downcasting, and cheap to clone out of a
    /// `DashMap` `Ref` before an `.await`.
    system_tx: mpsc::Sender<SystemMessage>,
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
            system_tx: handle.system_tx(),
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
}

impl RegistryGuard {
    pub(crate) fn new(system: Arc<ActorSystem>, id: ActorId, name: Option<String>) -> Self {
        Self { system, id, name }
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        self.system.unregister_by_id(&self.id);
        if let Some(name) = &self.name {
            self.system.by_name.remove(name);
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
    /// Set once `shutdown`/`shutdown_with` begins; `register_actor` rejects
    /// every registration attempt afterward with
    /// `SpawnError::SystemShuttingDown` (OTP application-controller parity).
    /// Never reset.
    shutting_down: AtomicBool,
    /// Source of `AnyActorHandle::registration_seq`.
    registration_seq: AtomicU64,
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
            shutting_down: AtomicBool::new(false),
            registration_seq: AtomicU64::new(0),
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
    /// Returns [`SendError::Closed`] if no actor with the given name is registered.
    pub async fn stop(&self, name: &str) -> Result<(), SendError> {
        // Clone the sender out of the `Ref` and drop the guard before the
        // `.await`: holding a `DashMap` guard across an await point
        // can deadlock against a concurrent writer on the same shard.
        let tx = {
            let entry = self.by_name.get(name).ok_or(SendError::Closed)?;
            entry.system_tx.clone()
        };
        tx.send(SystemMessage::Stop(StopReason::Graceful))
            .await
            .map_err(|_| SendError::Closed)
    }

    /// Force-kills a named actor, bypassing all lifecycle callbacks.
    ///
    /// Returns [`SendError::Closed`] if no actor with the given name is registered.
    pub async fn kill(&self, name: &str) -> Result<(), SendError> {
        // Same guard-before-await discipline as `stop`.
        let tx = {
            let entry = self.by_name.get(name).ok_or(SendError::Closed)?;
            entry.system_tx.clone()
        };
        tx.send(SystemMessage::Stop(StopReason::Kill))
            .await
            .map_err(|_| SendError::Closed)
    }

    /// Lists all registered actor names in this system.
    pub fn registered(&self) -> Vec<String> {
        self.by_name.iter().map(|e| e.key().clone()).collect()
    }

    // - Internal registration (used by spawn path) --------------------------

    /// Registers a spawned actor. `is_root` distinguishes a top-level
    /// (`SpawnBuilder`) spawn, which system shutdown signals directly, from a
    /// supervised child (`spawn_child`), which is taken down by its own
    /// supervisor's shutdown cascade instead.
    ///
    /// Returns [`SpawnError::SystemShuttingDown`] once shutdown has begun:
    /// OTP's application controller rejects new registrations while stopping.
    pub(crate) fn register_actor<A: Actor>(
        &self,
        id: &ActorId,
        name: Option<&str>,
        handle: &ActorHandle<A>,
        is_root: bool,
    ) -> Result<(), SpawnError> {
        if self.shutting_down.load(Ordering::Acquire) {
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
        Ok(())
    }

    pub(crate) fn unregister_by_id(&self, id: &ActorId) {
        self.by_id.remove(id);
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
        self.shutting_down.store(true, Ordering::Release);
        let deadline = Instant::now() + policy.timeout;

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
        let reported: HashSet<ActorId> = outcomes.iter().map(|(id, _)| id.clone()).collect();
        let leftover: Vec<ActorId> = self
            .by_id
            .iter()
            .map(|e| e.key().clone())
            .filter(|id| !reported.contains(id))
            .collect();
        if !leftover.is_empty() {
            outcomes.extend(self.concurrent_force_sweep(&leftover).await);
        }

        ShutdownReport { outcomes }
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
        self.send_stop(id, StopReason::ParentRequest).await;
        if wait_terminal(&mut status_rx, time_left(deadline).min(per_actor_timeout)).await {
            return Some(StopOutcome::Graceful);
        }
        if Instant::now() >= deadline {
            return None;
        }

        // Tier 2: Kill (unvetoable, but still only observed cooperatively -
        // an actor stuck inside a callback never reaches the select! that
        // would see it).
        self.send_stop(id, StopReason::Kill).await;
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
            let (status_rx, system_tx, abort) = match self.by_id.get(&id) {
                Some(e) => (
                    Some(e.status_rx.clone()),
                    Some(e.system_tx.clone()),
                    e.abort.clone(),
                ),
                None => (None, None, None),
            };
            set.spawn(async move {
                let outcome = force_stop(status_rx, system_tx, abort).await;
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

    /// Clones the sender out of the map guard and drops the guard before the
    /// send `.await`s. A missing entry (the actor already died on its
    /// own) is a silent no-op: the caller's subsequent status wait finds it
    /// already terminal.
    async fn send_stop(&self, id: &ActorId, reason: StopReason) {
        let tx = self.by_id.get(id).map(|e| e.system_tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(SystemMessage::Stop(reason)).await;
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
    system_tx: Option<mpsc::Sender<SystemMessage>>,
    abort: Option<AbortHandle>,
) -> StopOutcome {
    let Some(mut status_rx) = status_rx else {
        return StopOutcome::Graceful;
    };

    if let Some(tx) = &system_tx {
        let _ = tx.send(SystemMessage::Stop(StopReason::Kill)).await;
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
