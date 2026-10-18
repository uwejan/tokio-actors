//! Actor system providing named actor registration and coordinated shutdown.
//!
//! Internal module; the public pieces are re-exported at the crate root.

use std::any::{Any, TypeId};
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap, HashSet};
#[cfg(any(test, feature = "tracing"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use dashmap::DashMap;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant;

use crate::actor::handle::ActorHandle;
use crate::actor::runtime::{bounded_wait, saturating_deadline};
use crate::actor::subtree::{AbortRecord, SubtreeHold};
use crate::actor::supervision::{rules, ABORT_SETTLE};
use crate::actor::Actor;
use crate::error::{SendError, SpawnError};
use crate::types::{
    ActorId, ActorStatus, RaiseResult, ShutdownReport, StatusValue, StopLane, StopOutcome,
    StopReason, StopReport, TurnClock,
};

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
    /// The shutdown's global deadline, counted from the call. After it, all
    /// remaining actors are force-stopped: a shutdown can run on for the
    /// longest kill grace among the roots it forces plus the abort settle time,
    /// then the longest kill grace among the leftovers it forces plus the abort
    /// settle time, and that time once more plus the abort settle time for a
    /// leftover whose abort, scheduled with the system, is issued late. A
    /// sweep already started by another call that has not
    /// completed, a dropped call included, holds an earlier deadline, which
    /// then bounds this call's leftovers too. A timeout beyond 30 years is
    /// treated as 30 years.
    ///
    /// Default: 30 seconds.
    pub timeout: Duration,
    /// Maximum time to wait for each root actor to stop after its stop request:
    /// it covers the root's `pre_stop`, which may veto, and for a root
    /// supervisor its whole teardown (its own `on_stopped`, the final
    /// deliveries of its pending child deaths and the stops of all of its
    /// children), and each wait is capped by what is left of the global
    /// deadline. If a root doesn't stop within this period, it receives
    /// `StopReason::Kill`. A period beyond 30 years is treated as 30 years. A
    /// root supervisor's window covers its children only when it is at least
    /// the sum of the children's stop budgets (each child's `Shutdown`, plus
    /// its kill grace, plus the short wait that confirms an abort), or the
    /// largest of them for a `SimpleOneForOne` pool. A shorter window escalates
    /// the root to `Kill`, and its children still running then take the path of
    /// a killed supervisor's children: workers receive `Kill`, and a child that
    /// is itself a supervisor stops in order within its own `Shutdown`. It
    /// bounds only the roots' request step: a non-root actor still registered
    /// once every root has been handled is waited on, bounded by the global
    /// deadline of [`timeout`](Self::timeout) (or the earlier deadline the
    /// system holds for a sweep already started), and force-stopped if it is
    /// still running then (see [`timeout`](Self::timeout) for how long a forced
    /// stop can run on after the deadline).
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
    /// Initial stuck-turn watchdog threshold for this system. `None` (the
    /// default) leaves the watchdog off: turns are timed only while a
    /// threshold is set, so with this left `None`,
    /// [`ActorSystem::turn_elapsed`] returns `None` for every actor and
    /// [`ActorSystem::stuck_actors`] is empty. Setting a threshold adds one
    /// monotonic clock read per callback of every actor in the
    /// system. A turn already in flight when the threshold is first applied
    /// is not timed; the next turn that actor starts is. See
    /// [`ActorSystem::set_turn_warning_threshold`] for what this controls.
    pub turn_warning_threshold: Option<Duration>,
}

impl SystemConfig {
    /// Sets the initial stuck-turn watchdog threshold (see
    /// [`ActorSystem::set_turn_warning_threshold`]).
    pub fn with_turn_warning_threshold(mut self, threshold: Duration) -> Self {
        self.turn_warning_threshold = Some(threshold);
        self
    }
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
    status_rx: watch::Receiver<StatusValue>,
    /// This actor's turn clock, handed in at registration time (the clock
    /// lives with the run loop and this registry entry, never with
    /// `ActorHandle`): backs [`ActorSystem::turn_elapsed`] and
    /// [`ActorSystem::stuck_actors`] without downcasting.
    turn_clock: TurnClock,
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
    /// The time this actor is given, after a `Kill` is raised on it, to
    /// reach its next turn boundary before its task is aborted. Every kill
    /// path that finds the actor through the registry reads it here.
    kill_grace: Duration,
    /// The runtime that drives this actor's task. An abort takes effect only
    /// when that runtime next polls the task, so the kill backstop is
    /// scheduled on this runtime's reaper and on no other.
    runtime: Handle,
}

impl AnyActorHandle {
    fn new<A: Actor>(
        handle: &ActorHandle<A>,
        is_root: bool,
        registration_seq: u64,
        turn_clock: TurnClock,
        kill_grace: Duration,
        runtime: Handle,
    ) -> Self {
        Self {
            type_id: TypeId::of::<ActorHandle<A>>(),
            handle: Box::new(handle.clone()),
            stop_lane: handle.stop_lane(),
            status_rx: handle.status_rx(),
            turn_clock,
            abort: None,
            is_root,
            registration_seq,
            kill_grace,
            runtime,
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
    /// out from under it. `by_id` refuses a second live registration of an
    /// id, so the sequence check is defense for a stale guard.
    registration_seq: u64,
    /// A sender of the actor's status plane. Released after `drop`
    /// unregistered the actor: on every path that drops the guard last, the
    /// plane reads as ended only once the name is free.
    _status: Option<watch::Sender<StatusValue>>,
    /// The actor's hold on its subtree token. Declared last, so it is
    /// released after `drop` unregistered the actor and after the status
    /// sender: the subtree ends only once the name is free.
    _subtree: Option<Arc<SubtreeHold>>,
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
            _status: None,
            _subtree: None,
        }
    }

    /// Makes the guard hold a sender of the actor's status plane as well.
    pub(crate) fn with_status(mut self, status: watch::Sender<StatusValue>) -> Self {
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
        self.system
            .unregister_by_id(&self.id, self.registration_seq);
        if let Some(name) = &self.name {
            self.system.unregister_by_name(name, self.registration_seq);
        }
        #[cfg(test)]
        crate::actor::runtime::end_hooks::run(
            crate::actor::runtime::end_hooks::NAME_FREED,
            &self.id,
        );
    }
}

// ---------------------------------------------------------------------------
// SystemPhase
// ---------------------------------------------------------------------------

/// Lifecycle phase of an [`ActorSystem`]'s registry.
///
/// `Active` accepts registrations. `shutdown`/`shutdown_with` moves the
/// system to `ShuttingDown` immediately and to `Defunct` once the last
/// shutdown in flight has completed; both reject new registrations with
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
    /// `ShuttingDown`, then to `Defunct` once the last shutdown in flight has
    /// completed). Every change happens under `registration_gate`'s exclusive
    /// side.
    /// `register_actor` rejects every registration attempt in either
    /// non-`Active` phase with `SpawnError::SystemShuttingDown` (OTP
    /// application-controller parity). Not a one-way flag:
    /// [`ActorSystem::reactivate`] moves a `Defunct` system back to `Active`.
    phase: AtomicU8,
    /// Makes a registration atomic with every phase change, and counts the
    /// shutdowns in flight: `register_actor` holds the shared side from its
    /// phase check through its inserts, and every phase change takes the
    /// exclusive side. A shutdown raises the count as it leaves `Active` and
    /// lowers it when it ends, storing `Defunct` only if it completed and was
    /// the last in flight; `reactivate` returns to `Active` only from
    /// `Defunct` with no shutdown in flight. A registration therefore either
    /// lands before a shutdown's phase change, and is in that shutdown's
    /// first snapshot, or sees a phase other than `Active` and is refused,
    /// for as long as any shutdown is in flight.
    registration_gate: std::sync::RwLock<usize>,
    /// Source of `AnyActorHandle::registration_seq`.
    registration_seq: AtomicU64,
    /// Source of every child incarnation token this system ever hands out -
    /// fresh spawns and restarts alike, so no two incarnations of any child,
    /// under any name, in any supervisor's registry, are ever equal (see
    /// [`next_incarnation`](Self::next_incarnation)).
    incarnation_seq: AtomicU64,
    /// This system's reaper feeds, one per runtime, each spawned lazily on
    /// its runtime's first use and replaced when its task has finished (see
    /// [`reaper_handle`](Self::reaper_handle)).
    reaper: Mutex<HashMap<tokio::runtime::Id, ReaperSlot>>,
    /// Stuck-turn watchdog threshold, in nanoseconds; `0` means off. Shared
    /// (via [`turn_threshold_cell`](Self::turn_threshold_cell)) directly with
    /// every actor's [`TurnClock`], so a change here takes effect for
    /// already-spawned actors without any extra signaling. See
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    turn_warning_threshold_nanos: Arc<AtomicU64>,
    /// Runs once between a system-side kill's lookup and its raise, so a test
    /// can store the target's abort handle at that exact point.
    #[cfg(test)]
    after_kill_lookup: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Runs once between a system-side kill's raise and its re-read of the
    /// registry, so a test can delay that re-read.
    #[cfg(test)]
    after_kill_raise: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Runs at each point where a shutdown is about to act on one root, so a
    /// test can change the registry at that exact point. Kept until cleared.
    #[cfg(test)]
    ladder_hook: Mutex<Option<LadderHook>>,
    /// How many times `arm_kill_backstop` ran, for the tests that pin which
    /// paths arm it.
    #[cfg(test)]
    backstop_arms: AtomicU64,
    /// Runs once inside a registration, between its phase check and its
    /// inserts.
    #[cfg(test)]
    after_phase_check: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Set once a shutdown has taken its last root snapshot.
    #[cfg(test)]
    root_snapshots_done: AtomicBool,
    /// Set by a shutdown just before it takes the registration gate's
    /// exclusive side.
    #[cfg(test)]
    shutdown_at_the_gate: AtomicBool,
    /// Guards the turn-watchdog sweep task's lazy spawn: `true` while a
    /// sweep task is currently alive for this system, `false` otherwise.
    /// Shared (via a clone) with the sweep task itself, whose own drop guard
    /// clears it - back to `false` the moment the task returns OR the
    /// runtime it was spawned on is dropped out from under it - so a system
    /// that outlives its sweep's runtime can always spawn a fresh one on the
    /// next runtime that becomes current, instead of believing forever that
    /// a sweep is still running. Only meaningful (and only compiled) when
    /// the sweep task itself exists, which requires the `tracing` feature -
    /// the sweep's sole job is emitting a `tracing::warn!`.
    #[cfg(feature = "tracing")]
    sweep_spawned: Arc<AtomicBool>,
    /// The earliest global deadline of a final sweep that started and whose
    /// shutdown has not completed: its deadline entries stand, so every
    /// sweep, one already waiting included, bounds and reports its leftovers
    /// by it. A watch, so a sweep's waits see it lowered while they run.
    /// Cleared when the last shutdown in flight completes.
    sweep_deadline: watch::Sender<Option<Instant>>,
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

    fn empty(
        name: String,
        shutdown_policy: ShutdownPolicy,
        turn_warning_threshold: Option<Duration>,
    ) -> Self {
        #[cfg(all(feature = "tracing", panic = "abort"))]
        {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                caught_log!(warn,
                    "panic = \"abort\": actor panics terminate the process; panic capture and crash-visible supervision are unavailable"
                );
            });
        }

        Self {
            name,
            by_name: DashMap::new(),
            by_id: DashMap::new(),
            shutdown_policy,
            phase: AtomicU8::new(SystemPhase::Active.as_u8()),
            registration_gate: std::sync::RwLock::new(0),
            registration_seq: AtomicU64::new(0),
            incarnation_seq: AtomicU64::new(0),
            reaper: Mutex::new(HashMap::new()),
            turn_warning_threshold_nanos: Arc::new(AtomicU64::new(threshold_to_nanos(
                turn_warning_threshold,
            ))),
            #[cfg(test)]
            after_kill_lookup: Mutex::new(None),
            #[cfg(test)]
            after_kill_raise: Mutex::new(None),
            #[cfg(test)]
            ladder_hook: Mutex::new(None),
            #[cfg(test)]
            backstop_arms: AtomicU64::new(0),
            #[cfg(test)]
            after_phase_check: Mutex::new(None),
            #[cfg(test)]
            root_snapshots_done: AtomicBool::new(false),
            #[cfg(test)]
            shutdown_at_the_gate: AtomicBool::new(false),
            #[cfg(feature = "tracing")]
            sweep_spawned: Arc::new(AtomicBool::new(false)),
            sweep_deadline: watch::channel(None).0,
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
                    None,
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
                let system = Arc::new(Self::empty(name, ShutdownPolicy::default(), None));
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
                let system = Arc::new(Self::empty(
                    name,
                    config.shutdown_policy,
                    config.turn_warning_threshold,
                ));
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
    /// - [`SendError::NotFound`] if no actor is registered under `name`,
    ///   which includes an actor whose task has ended.
    /// - [`SendError::Closed`] if an actor is registered under `name` but no
    ///   longer takes the request (its message loop has ended).
    pub async fn stop(&self, name: &str) -> Result<(), SendError> {
        // Clone the lane out of the `Ref` and drop the guard before using
        // it: holding a `DashMap` guard across an await point can deadlock
        // against a concurrent writer on the same shard (the raise itself is
        // synchronous, but this keeps the same discipline as every other
        // method here).
        let lane = {
            let entry = self.by_name.get(name).ok_or(SendError::NotFound)?;
            entry.stop_lane.clone()
        };
        match lane.try_raise(StopReason::Graceful) {
            RaiseResult::Refused => Err(SendError::Closed),
            RaiseResult::Recorded | RaiseResult::Superseded => Ok(()),
        }
    }

    /// Force-kills a named actor, bypassing all lifecycle callbacks.
    ///
    /// Same stop-lane delivery as [`stop`](Self::stop), at `Kill` severity.
    /// The actor leaves its message loop at its next turn boundary; if it
    /// does not reach one, its task is aborted once its kill grace has
    /// passed (120 ms unless its spawn builder set another), so a handler
    /// parked at an await is aborted after that grace. The abort is
    /// scheduled by the system: dropping this future never cancels it. While
    /// the actor is registered the `Kill` is accepted, including while it
    /// runs `on_stopped` and while its children are being stopped. The name
    /// leaves the registry once the actor's children are stopped, before its
    /// stop lane is sealed and before its queued messages and its value are
    /// dropped (an aborted actor's when its task ends); from then on this
    /// answers `NotFound`. A `Kill` accepted after the actor left the
    /// registry arms no abort: its one remaining await is the end of its
    /// already aborted timer and stream tasks, which waits on their drops, so
    /// its end never precedes theirs.
    ///
    /// # Errors
    /// - [`SendError::NotFound`] if no actor is registered under `name`,
    ///   which includes an actor whose task has ended.
    /// - [`SendError::Closed`] if the actor stopped taking the request
    ///   between this call's lookup of `name` and its raise: it left the
    ///   registry and sealed its lane, or its task ended. Nothing is aborted
    ///   then.
    pub async fn kill(&self, name: &str) -> Result<(), SendError> {
        // Same guard-before-use discipline as `stop`.
        let (lane, status_rx) = {
            let entry = self.by_name.get(name).ok_or(SendError::NotFound)?;
            (entry.stop_lane.clone(), entry.status_rx.clone())
        };
        #[cfg(test)]
        self.run_after_kill_lookup();
        if lane.try_raise(StopReason::Kill) == RaiseResult::Refused {
            return Err(SendError::Closed);
        }
        #[cfg(test)]
        self.run_after_kill_raise();
        // The abort is read after the raise: an entry still in the window
        // before its abort handle is stored is armed by `attach_abort`, which
        // reads the lane after storing the handle, so one of the two sees
        // the other's write. Only the incarnation the Kill was raised on is
        // armed: a later registration under the same name is not.
        let target = self
            .by_name
            .get(name)
            .filter(|entry| entry.status_rx.same_channel(&status_rx))
            .map(|entry| (entry.abort.clone(), entry.kill_grace, entry.runtime.clone()));
        if let Some((Some(abort), grace, runtime)) = target {
            self.arm_kill_backstop(abort, lane.abort_record(), grace, &runtime);
        }
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
    ///
    /// An actor whose task was aborted never writes a final status itself;
    /// its status sender closing is the terminal signal instead, and this
    /// reports [`ActorStatus::Stopped`] for that case too.
    pub fn actor_status(&self, id: &ActorId) -> Option<ActorStatus> {
        self.by_id.get(id).map(|e| read_status(&e.status_rx))
    }

    /// Force-kills an actor by id, bypassing all lifecycle callbacks -
    /// untrappable, matching `erlang:exit(Pid, kill)` (erlang.org/docs/28).
    /// Returns `None` if the id is not registered, or if its actor's lane
    /// refuses the `Kill` (it stopped taking the request between this call's
    /// lookup and its raise, where [`kill`](Self::kill) answers
    /// [`SendError::Closed`]); nothing is aborted then. Otherwise the target
    /// takes the same `Kill`, kill grace and abort as with
    /// [`kill`](Self::kill), and this call, like the shutdown sweep's force,
    /// also waits: the kill grace, then the abort, then a short settle wait,
    /// and it reports the end. The abort is scheduled by the system when the
    /// `Kill` is accepted while the actor is still registered, before this call
    /// waits: dropping the returned future never cancels it. A `Kill` accepted
    /// after the actor left the registry arms no abort: its one remaining await
    /// is the end of its already aborted timer and stream tasks, which waits on
    /// their drops, so its end never precedes theirs.
    ///
    /// The report names the step of that ladder at which the actor ended and
    /// the reason it ended with ([`StopReport`]); an actor that never yields
    /// to the runtime is reported [`StopOutcome::Unresponsive`].
    ///
    /// To force a restart of a supervised child, kill it: its supervisor
    /// restarts it per its restart type and charges the restart budget, as
    /// `exit(Child, kill)` does in Erlang/OTP. An
    /// [`ActorContext`](crate::actor::context::ActorContext) has no kill by
    /// id: a supervisor handler gets its system with
    /// [`ActorSystem::get_named`](Self::get_named) and calls `kill_by_id` on
    /// it, or calls
    /// [`stop_within`](crate::actor::handle::ActorHandle::stop_within) with
    /// [`StopReason::Kill`] on a handle it kept.
    /// [`stop_child`](crate::actor::context::ActorContext::stop_child) is
    /// the bounce that charges nothing.
    pub async fn kill_by_id(&self, id: &ActorId) -> Option<StopReport> {
        // Guard-before-await discipline, as in `stop`/`kill`: clone the
        // pieces `force_stop` needs out of the `DashMap` guard before any
        // `.await`.
        let (status_rx, lane, grace) = match self.by_id.get(id) {
            Some(e) => (e.status_rx.clone(), e.stop_lane.clone(), e.kill_grace),
            None => return None,
        };
        #[cfg(test)]
        self.run_after_kill_lookup();
        // Raise first, then read the abort and arm (see `kill`); a lane that
        // refuses the `Kill` belongs to a task past its last read of it, and
        // the call answers as `kill` does. The wait is the incarnation's own
        // grace, read with the lookup, so it holds after the entry is gone.
        if lane.try_raise(StopReason::Kill) == RaiseResult::Refused {
            return None;
        }
        let abort = self.arm_after_raise(id, &status_rx, lane.abort_record());
        let record = lane.abort_record().clone();
        Some(force_stop(status_rx, abort, record, grace, StopOutcome::Killed).await)
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_actor<A: Actor>(
        &self,
        id: &ActorId,
        name: Option<&str>,
        handle: &ActorHandle<A>,
        is_root: bool,
        turn_clock: TurnClock,
        kill_grace: Duration,
        runtime: Handle,
    ) -> Result<u64, SpawnError> {
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
                    v.insert(AnyActorHandle::new(
                        handle,
                        is_root,
                        seq,
                        turn_clock.clone(),
                        kill_grace,
                        runtime.clone(),
                    ));
                }
            }
        }
        match self.by_id.entry(id.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => {
                // A second live registration of one id is refused. The name
                // this call just claimed is released again, so the refusal
                // leaves no trace behind.
                if let Some(n) = name {
                    self.unregister_by_name(n, seq);
                }
                return Err(SpawnError::DuplicateChild(id.clone()));
            }
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(AnyActorHandle::new(
                    handle, is_root, seq, turn_clock, kill_grace, runtime,
                ));
            }
        }
        drop(gate);

        // Lazily start the turn-watchdog sweep the first time a threshold is
        // in effect at registration time. The spawn path resolves a runtime
        // via `Handle::try_current()` before ever reaching here, so one is
        // normally current - but `try_current()` is used here too rather
        // than the panicking `Handle::current()`, so a registration reached
        // some other way, with no runtime current, quietly skips the spawn
        // instead of panicking. If the threshold is set later instead, via
        // `set_turn_warning_threshold`, that method makes its own attempt;
        // either way, the next call with a runtime current retries.
        #[cfg(feature = "tracing")]
        if self.turn_warning_threshold_nanos.load(Ordering::Relaxed) != 0 {
            if let Ok(rt) = Handle::try_current() {
                self.ensure_sweep_spawned_with(&rt);
            }
        }

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
    /// collide, so a stale runner completion (from a superseded instance,
    /// including one that shared its predecessor's name after a
    /// terminate/delete/respawn cycle) can never be mistaken for the fresh
    /// one's death.
    pub(crate) fn next_incarnation(&self) -> u64 {
        self.incarnation_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Moves a `Defunct` system (one whose last `shutdown`/`shutdown_with`
    /// in flight has fully completed) back to `Active`, so its name can be
    /// registered into again instead of staying poisoned forever.
    ///
    /// Returns `false` without effect if the system is not currently
    /// `Defunct` - either still `Active` (nothing to reactivate), or
    /// `ShuttingDown` - or if any shutdown is still in flight: reactivating
    /// out from under one would let a registration land after that
    /// shutdown's last snapshot, neither stopped nor refused. A system left
    /// `ShuttingDown` by a dropped shutdown reaches `Defunct` by calling
    /// [`shutdown`](Self::shutdown) again and letting it complete.
    pub fn reactivate(&self) -> bool {
        let in_flight = self
            .registration_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *in_flight != 0
            || SystemPhase::from_u8(self.phase.load(Ordering::Acquire)) != SystemPhase::Defunct
        {
            return false;
        }
        self.phase
            .store(SystemPhase::Active.as_u8(), Ordering::Release);
        true
    }

    /// Attaches the task's abort handle to the just-created registry entries.
    /// Called once, immediately after `handle.spawn(...)`: the `AbortHandle`
    /// does not exist before then, so registration (the name-claim) and this
    /// attach step are necessarily two steps, with a brief window between
    /// them where the entry's `abort` is `None` (see [`AnyActorHandle::abort`]).
    ///
    /// A `Kill` raised in that window found no abort handle to schedule: the
    /// caller raises and then reads the entry's abort, this stores the abort
    /// and then reads the lane, and both go through locks, so at least one
    /// of them sees the other's write. When the lane already holds a `Kill`,
    /// this arms the backstop itself, with the entry's own kill grace.
    ///
    /// Only the entries of this spawn's own registration, `registration_seq`,
    /// take the handle, each checked under the same entry lock as the write:
    /// an entry a later registration made under the same id or name (after
    /// this incarnation ended and freed them) keeps its own abort handle, and
    /// no backstop is armed through it.
    pub(crate) fn attach_abort(
        &self,
        id: &ActorId,
        name: Option<&str>,
        abort: AbortHandle,
        registration_seq: u64,
    ) {
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
        if let Some(name) = name {
            if let Some(mut entry) = self.by_name.get_mut(name) {
                if entry.registration_seq == registration_seq {
                    entry.abort = Some(abort.clone());
                }
            }
        }
        if let Some((lane, grace, runtime)) = armed_with {
            if matches!(lane.reason(), Some(StopReason::Kill)) {
                self.arm_kill_backstop(abort, lane.abort_record(), grace, &runtime);
            }
        }
    }

    /// Schedules the abort of a task whose actor was just sent `Kill`: the
    /// reaper of the target's own runtime aborts it once `grace` has passed
    /// unless it has ended by then. The deadline saturates (a caller's
    /// `within` can reach `Duration::MAX`). Never blocks, never depends on
    /// the caller: the entry belongs to the reaper from here, and every end
    /// it can have (fired, or dropped unfired) aborts a task that is still
    /// running.
    ///
    /// `rt` is the runtime stored in the target's registry entry, never the
    /// caller's: an abort takes effect only when the target's own runtime
    /// next polls the task, and a caller on another runtime, or on none,
    /// would park the entry on a reaper nothing runs.
    pub(crate) fn arm_kill_backstop(
        &self,
        abort: AbortHandle,
        record: &AbortRecord,
        grace: Duration,
        rt: &Handle,
    ) {
        #[cfg(test)]
        self.backstop_arms.fetch_add(1, Ordering::SeqCst);
        let entry = ReaperEntry::new(
            saturating_deadline(Instant::now(), grace),
            abort,
            record.clone(),
        );
        self.feed_reaper(rt, entry);
    }

    /// After a `Kill` was raised on the incarnation whose status plane is
    /// `raised_on`: reads its abort handle, kill grace and runtime from the
    /// registry and schedules the abort. The read comes after the raise,
    /// never before: a handle stored by `attach_abort` before this read is
    /// armed here, and one stored after it is armed by `attach_abort`, which
    /// reads the lane after storing it. The read is exact to the incarnation
    /// (`kill_target_for`): a later registration under the same id is never
    /// armed. Returns the abort handle read: absent in the window before it
    /// is stored, and for an incarnation already gone.
    fn arm_after_raise(
        &self,
        id: &ActorId,
        raised_on: &watch::Receiver<StatusValue>,
        record: &AbortRecord,
    ) -> Option<AbortHandle> {
        #[cfg(test)]
        self.run_after_kill_raise();
        let (abort, grace, runtime) = self.kill_target_for(id, raised_on)?;
        if let Some(abort) = &abort {
            self.arm_kill_backstop(abort.clone(), record, grace, &runtime);
        }
        abort
    }

    #[cfg(test)]
    fn run_after_kill_raise(&self) {
        let hook = self.after_kill_raise.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    pub(crate) fn run_ladder_hook(&self, point: LadderPoint, id: &ActorId) {
        let hook = self.ladder_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(point, id);
        }
    }

    #[cfg(test)]
    fn run_after_kill_lookup(&self) {
        let hook = self.after_kill_lookup.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Hands `entry` to the reaper of runtime `rt`. The feed takes every
    /// entry, so the grace of the entry's task is never shortened; a closed
    /// feed (its reaper's task is gone) is retried once on the reaper `rt` has
    /// by then, and a second refusal drops the entry, which aborts its task.
    pub(crate) fn feed_reaper(&self, rt: &Handle, entry: ReaperEntry) {
        let Err(mpsc::error::SendError(entry)) = self.reaper_handle(rt).send(entry) else {
            return;
        };
        if let Err(mpsc::error::SendError(entry)) = self.reaper_handle(rt).send(entry) {
            drop(entry);
        }
    }

    /// Returns the abort handle, the kill grace and the runtime of the task
    /// registered under `id`, but only if that registration's status channel
    /// is `status_rx`'s channel, so a caller holding a handle to one
    /// incarnation can never obtain the abort handle of a later one. The
    /// abort is `None` in the window before the task's abort handle is
    /// attached; the grace and the runtime are always there.
    pub(crate) fn kill_target_for(
        &self,
        id: &ActorId,
        status_rx: &watch::Receiver<StatusValue>,
    ) -> Option<(Option<AbortHandle>, Duration, Handle)> {
        let entry = self.by_id.get(id)?;
        if !entry.status_rx.same_channel(status_rx) {
            return None;
        }
        Some((entry.abort.clone(), entry.kill_grace, entry.runtime.clone()))
    }

    // - Stuck-turn watchdog --------------------------------------------------

    /// Sets this system's stuck-turn watchdog threshold: how long a single
    /// callback of an actor may run before it is reported by
    /// [`stuck_actors`](Self::stuck_actors) (and, with the `tracing` feature
    /// enabled, logged with a warning). `None` turns the watchdog off (the
    /// default for every system); `Some(Duration::ZERO)` is treated the same
    /// as `None`, since a threshold of zero would fire on every actor at
    /// every instant.
    ///
    /// Every callback is timed: `pre_start`, `on_started`, `handle`,
    /// `pre_stop`, `on_child_stopped` and `on_stopped`. An actor stuck in its
    /// initialization or inside its stop is reported too, while its
    /// [`actor_status`](Self::actor_status) reads `Initializing`, `Running`
    /// (inside `pre_stop`) or `Stopping`.
    ///
    /// Turns are timed only while a threshold is set: while it is `None`,
    /// [`turn_elapsed`](Self::turn_elapsed) returns `None` for every actor
    /// and [`stuck_actors`](Self::stuck_actors) is empty. Setting a
    /// threshold adds one monotonic clock read per callback of every
    /// actor in this system, for as long as the threshold stays set. A turn
    /// already in flight at the moment a threshold is first set is not
    /// timed; the next turn that actor starts is.
    ///
    /// This is observation only: it never interrupts a turn, never raises a
    /// stop, and never changes how a stop resolves.
    /// [`actor_status`](Self::actor_status) still reports the status the
    /// actor holds ([`ActorStatus::Initializing`], [`ActorStatus::Running`] or
    /// [`ActorStatus::Stopping`]) for a wedged actor - `turn_elapsed` and
    /// `stuck_actors`, not `actor_status`, are how to tell. OTP parity:
    /// `erlang:system_monitor/2` with `{long_schedule, Time}`
    /// (erlang.org/docs/28). Set the threshold well above normal turn
    /// lengths to avoid false positives.
    ///
    /// With the `tracing` feature, setting a threshold also (re)starts the
    /// background task that logs actors over it: it runs on whichever
    /// runtime was current when it was started, exits when that runtime is
    /// dropped out from under it, and is started again, when a threshold is
    /// in effect, by the next call to this method or the next actor
    /// registration made with a runtime current.
    pub fn set_turn_warning_threshold(&self, threshold: Option<Duration>) {
        self.turn_warning_threshold_nanos
            .store(threshold_to_nanos(threshold), Ordering::Relaxed);
        #[cfg(feature = "tracing")]
        if threshold.is_some() {
            if let Ok(rt) = Handle::try_current() {
                self.ensure_sweep_spawned_with(&rt);
            }
            // No runtime current: nothing to spawn onto yet. The next
            // `register_actor` call (which always has one) tries again.
        }
    }

    /// Returns this system's current stuck-turn watchdog threshold, as set
    /// by [`set_turn_warning_threshold`](Self::set_turn_warning_threshold)
    /// or [`SystemConfig::turn_warning_threshold`]. `None` means the
    /// watchdog is off. Observation only - see
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    pub fn turn_warning_threshold(&self) -> Option<Duration> {
        nanos_to_threshold(self.turn_warning_threshold_nanos.load(Ordering::Relaxed))
    }

    /// How long the actor registered under `id` has been inside its current
    /// callback (`pre_start`, `on_started`, `handle`, `pre_stop`,
    /// `on_child_stopped` or `on_stopped`). `None` if it is between
    /// callbacks, not registered, or this system's threshold is currently
    /// `None` - turns are timed
    /// only while a threshold is set, so this always returns `None` for
    /// every actor while the watchdog is off, regardless of whether a turn
    /// happens to be in flight.
    ///
    /// Observation only - see
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    pub fn turn_elapsed(&self, id: &ActorId) -> Option<Duration> {
        // Checked here, not just inside `TurnClock::begin`: a turn that was
        // already in flight when the threshold was switched off mid-turn
        // keeps a nonzero token (and would keep answering `Some` from
        // `TurnClock::elapsed` alone) until it ends - this system's current
        // threshold, not the actor's own recorded token, is what decides
        // whether that turn is still reported as timed.
        if self.turn_warning_threshold_nanos.load(Ordering::Relaxed) == 0 {
            return None;
        }
        self.by_id.get(id).and_then(|e| e.turn_clock.elapsed())
    }

    /// Actors whose current callback (`pre_start`, `on_started`, `handle`,
    /// `pre_stop`, `on_child_stopped` or `on_stopped`) has run longer than the
    /// configured threshold, longest first. Empty when no threshold is set.
    ///
    /// Observation only - see
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    pub fn stuck_actors(&self) -> Vec<(ActorId, Duration)> {
        let threshold_nanos = self.turn_warning_threshold_nanos.load(Ordering::Relaxed);
        if threshold_nanos == 0 {
            return Vec::new();
        }
        let threshold = Duration::from_nanos(threshold_nanos);

        // Plain iteration, no `.await` anywhere inside: never hold a
        // `DashMap` guard across an await point. `snapshot()` reads the
        // token and the elapsed time from a single atomic load, so a turn
        // ending and a new one starting mid-iteration can never be reported
        // as a hybrid of the two.
        let mut stuck: Vec<(ActorId, Duration)> = self
            .by_id
            .iter()
            .filter_map(|e| {
                let (_, elapsed) = e.turn_clock.snapshot()?;
                (elapsed > threshold).then(|| (e.key().clone(), elapsed))
            })
            .collect();
        stuck.sort_by_key(|(_, elapsed)| std::cmp::Reverse(*elapsed));
        stuck
    }

    /// Returns a clone of this system's turn-warning threshold cell, handed
    /// to each actor's [`TurnClock`] at spawn time so every actor always
    /// reads the same cell this system writes: a threshold change made after
    /// spawn takes effect immediately, with no separate signaling needed.
    pub(crate) fn turn_threshold_cell(&self) -> Arc<AtomicU64> {
        self.turn_warning_threshold_nanos.clone()
    }

    /// Spawns the turn-watchdog sweep task for this system, unless one is
    /// already alive (`sweep_spawned` is already `true`). A no-op whenever a
    /// sweep is currently running; once that sweep's task ends - see
    /// [`turn_watchdog_sweep`]'s drop guard - the next call here spawns a
    /// fresh one.
    #[cfg(feature = "tracing")]
    fn ensure_sweep_spawned_with(&self, rt: &Handle) {
        if self
            .sweep_spawned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // Built here and moved into the spawned future as a plain captured
        // value, not constructed as the first line of the future's own
        // body: a future's captures drop along with it even if it is never
        // polled, so this way the latch clears whether the task runs to
        // completion, is aborted mid-flight, or is dropped unpolled because
        // its runtime never gets around to it before shutting down.
        let guard = SweepLatchGuard(self.sweep_spawned.clone());
        let name = self.name.clone();
        rt.spawn(async move {
            let _guard = guard;
            turn_watchdog_sweep(name).await
        });
    }

    /// Returns the reaper feed of runtime `rt`, spawning its backing task on
    /// `rt` on first use. Every supervised child's link guard and every kill
    /// path feeds a deadline into it (see `ChildLinkGuard` in the
    /// `supervision` module and [`arm_kill_backstop`](Self::arm_kill_backstop));
    /// the task itself never blocks anything and aborts a task only if it has
    /// not exited by its scheduled deadline.
    ///
    /// There is one reaper per runtime, keyed by the runtime's identity: an
    /// abort takes effect only when the runtime that drives the task next
    /// polls it, so a reaper on one runtime cannot serve another, and a
    /// runtime that is alive but never driven holds only its own entries. A
    /// slot whose task has finished (its runtime ended) is replaced on use,
    /// because a runtime's identity may be reused once the runtime is gone.
    /// The finished slots of other runtimes are pruned only when a fresh slot
    /// is inserted, never on a lookup.
    pub(crate) fn reaper_handle(&self, rt: &Handle) -> ReaperFeed {
        let id = rt.id();
        let mut slots = self
            .reaper
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = slots.get(&id) {
            if !existing.task.is_finished() {
                return existing.tx.clone();
            }
        }
        slots.retain(|_, slot| !slot.task.is_finished());
        let (tx, rx) = mpsc::unbounded_channel();
        let task = rt.spawn(reaper_loop(rx)).abort_handle();
        let fresh = tx.clone();
        slots.insert(id, ReaperSlot { tx, task });
        fresh
    }

    // - Shutdown -------------------------------------------------------------

    /// Shuts down all registered actors using the system's stored policy.
    /// See [`shutdown_with`](Self::shutdown_with); it needs a Tokio runtime
    /// with the time driver enabled.
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
    /// The system reads `Defunct` once a shutdown completes with no other
    /// shutdown in flight. A shutdown whose future is dropped before it
    /// completes leaves the system `ShuttingDown`, refusing registrations
    /// and with [`reactivate`](Self::reactivate) returning `false`, until
    /// another shutdown completes.
    ///
    /// Bounded by `policy.timeout`: once the global deadline passes, every
    /// remaining root is force-stopped (`Kill` + `abort`) CONCURRENTLY instead
    /// of sequentially, so this always returns close to the deadline regardless
    /// of how many roots are still outstanding. A final sweep handles anything
    /// still left in the registry afterward (normally nothing: supervised
    /// children vanish via their own guard drops during the cascade above). The
    /// sweep waits on every leftover at once, each wait bounded by the global
    /// deadline, so a supervisor still stopping its children in order after its
    /// own supervisor ended can finish; every leftover still running at the
    /// global deadline is then force-stopped (`Kill` + `abort`) concurrently. A
    /// leftover that nothing stops is therefore held until the global deadline
    /// and then killed. After the global deadline, a shutdown can run on for
    /// the longest kill grace among the roots it forces plus the abort settle
    /// time, then the longest kill grace among the leftovers it forces plus the
    /// abort settle time, and that time once more plus the abort settle time
    /// for a leftover whose abort, scheduled with the system, is issued late.
    /// That deadline `Kill`, and
    /// the abort one kill grace after it (counted from when the `Kill` fires),
    /// are scheduled with the system when the sweep begins, so they happen even
    /// if the future returned here is dropped before the deadline. Since a stop only ever strengthens,
    /// the system holds one sweep deadline: a sweep lowers it to its own when
    /// that is earlier, but never to before the moment it lowers it, and every
    /// sweep, one already waiting in another call included, waits on and
    /// reports its leftovers by it. It is cleared only when the last shutdown
    /// in flight completes (a dropped last shutdown leaves it, as it leaves
    /// `ShuttingDown`).
    ///
    /// A leftover reads by the instant its end became final (its stop lane's
    /// seal) against the system's deadline: `Graceful` with the reason it
    /// ended with when that instant is before the deadline and it published
    /// `Stopped`, `Killed` when it is at or after it or when this shutdown's
    /// force took a `Kill` on the leftover, `Aborted` when its task ended
    /// without publishing `Stopped`, and `Unresponsive` when even the abort
    /// did not end it within the settle time after it. The instant does not
    /// depend on which call looks or when, so concurrent calls report one end
    /// the same way.
    ///
    /// Needs a Tokio runtime with the time driver enabled (`enable_time` or
    /// `enable_all`): without it, a root's wait panics, and a leftover whose
    /// wait fails is forced at once and reported from its plane.
    ///
    /// Registry entries are removed as their actors die (the existing
    /// `RegistryGuard` drop machinery) - never cleared up front, so a lookup
    /// racing the early part of shutdown still finds a live actor.
    ///
    /// # Dropping the future
    /// Drive this future to completion, or spawn it. If it is dropped, the root
    /// it was stopping keeps the stop request it was given but is not escalated
    /// further, and the roots it had not yet reached are never asked to stop
    /// and keep running. A `Kill` it already raised on a root still registered
    /// then still ends in its abort, after that root's kill grace, and once its
    /// final sweep has begun, every leftover still running at the global
    /// deadline is killed then and aborted one kill grace later, since the
    /// system holds that deadline. The system stays shutting down, as described
    /// above, and refuses every supervised restart as well as every new
    /// registration with [`SpawnError::SystemShuttingDown`], and a refused
    /// restart is charged to the restart budget like any failed restart: a
    /// supervisor whose child then dies and is due a restart exhausts a budget
    /// whose window holds its `max_restarts` restarts, while a zero window
    /// keeps retrying until a shutdown reaches the supervisor, unless the
    /// restarts read the same instant (see
    /// [`SupervisionConfig::restart_window`](crate::actor::supervision::SupervisionConfig::restart_window)).
    /// A later call to [`shutdown`](Self::shutdown) finishes the job.
    pub async fn shutdown_with(&self, policy: ShutdownPolicy) -> ShutdownReport {
        #[cfg(test)]
        self.shutdown_at_the_gate.store(true, Ordering::SeqCst);
        // Every registration in progress completes before the change, and
        // every later one sees it (see `registration_gate`). The guard keeps
        // this shutdown counted until it ends, completed or dropped.
        let mut in_flight = ShutdownInFlight::enter(self);
        let deadline = saturating_deadline(Instant::now(), policy.timeout);

        let mut roots = self.snapshot_roots();
        // Reverse registration order: most-recently-registered stops first.
        roots.sort_by_key(|r| std::cmp::Reverse(r.registration_seq));
        // Every root incarnation this shutdown has taken on, by its
        // registration sequence (unique to one registration, so to one
        // incarnation): a later incarnation registered under a reported id
        // is a root of its own, not one already reported.
        let mut reported: HashSet<u64> = roots.iter().map(|r| r.registration_seq).collect();
        let mut outcomes = self
            .stop_roots(&roots, policy.per_actor_timeout, deadline)
            .await;

        // Every registration landed before the phase change above or is
        // refused while any shutdown is in flight (see `registration_gate`),
        // so the first snapshot holds every root a public call can produce
        // and this pass finds none through them. It stays as a defensive
        // branch: a registration that bypasses the gate (a test helper) is
        // still stopped. Each pass re-snapshots the roots and stops any root
        // incarnation not yet taken on; it ends once a snapshot holds nothing
        // new.
        loop {
            let mut late: Vec<RootSnapshot> = self
                .snapshot_roots()
                .into_iter()
                .filter(|r| !reported.contains(&r.registration_seq))
                .collect();
            if late.is_empty() {
                break;
            }
            late.sort_by_key(|r| std::cmp::Reverse(r.registration_seq));
            reported.extend(late.iter().map(|r| r.registration_seq));
            outcomes.extend(
                self.stop_roots(&late, policy.per_actor_timeout, deadline)
                    .await,
            );
        }
        #[cfg(test)]
        self.root_snapshots_done.store(true, Ordering::SeqCst);

        // Final sweep: anything still registered that wasn't already taken
        // on above. Normally empty - supervised children vanish via their
        // own guard drops as part of each root's cascade. A leftover can be
        // a supervisor whose own supervisor ended before it did and that is
        // stopping its children in order, or is about to: a killed parent's
        // child is raised its stop only when the runtime drops the runner
        // task its parent aborted, and a grandchild only when its own
        // parent's teardown reaches it, so nothing read here tells a
        // leftover about to stop from one that nothing stops. Every leftover
        // is therefore waited on until the global deadline, since a `Kill`
        // would cut an orderly stop short; those still running then are
        // force-stopped concurrently. These are non-root leftovers, so they
        // are reported in `swept`, never `outcomes`.
        //
        // The deadline's `Kill` and abort do not depend on this future: for
        // each leftover the system schedules, at the snapshot, a two-stage
        // entry on the reaper of the leftover's runtime, built from the
        // registration found (its own lane, task and kill grace, so a later
        // registration under the same id is never touched): `Kill` at the
        // global deadline, the abort one kill grace after it. An entry whose
        // task has ended by a stage does nothing at that stage. The force
        // sweep below still runs at the deadline and reports the outcomes.
        //
        // The deadline belongs to the system: this sweep folds its own into
        // the system's (see `fold_sweep_deadline`), arms and waits by the
        // result, and its waits follow the system's deadline while they run,
        // so a sweep that starts later with an earlier deadline shortens them
        // too. Every ended leftover is classified against the system's
        // deadline by the instant its end became final, which no call's
        // timing changes: one end gets one outcome whichever call reports
        // it.
        let deadline = self.fold_sweep_deadline(deadline);
        let mut deadline_kills: Vec<(Handle, ReaperEntry)> = Vec::new();
        let leftover: Vec<SweepTarget> = self
            .by_id
            .iter()
            .filter(|e| !reported.contains(&e.registration_seq))
            .map(|e| {
                if let Some(abort) = e.abort.clone() {
                    deadline_kills.push((
                        e.runtime.clone(),
                        ReaperEntry::two_stage(
                            deadline,
                            e.stop_lane.clone(),
                            saturating_deadline(deadline, e.kill_grace),
                            abort,
                        ),
                    ));
                }
                SweepTarget {
                    id: e.key().clone(),
                    snapshot: RootSnapshot {
                        id: e.key().clone(),
                        registration_seq: e.registration_seq,
                        status_rx: e.status_rx.clone(),
                        kill_grace: e.kill_grace,
                        record: e.stop_lane.abort_record().clone(),
                    },
                    kill_raised: false,
                }
            })
            .collect();
        for (runtime, entry) in deadline_kills {
            self.feed_reaper(&runtime, entry);
        }
        // Every leftover is waited on at once, each wait bounded by the
        // system's deadline. A wait task that fails (it panics or is
        // cancelled) has its leftover read as not seen to end, so it goes to
        // the force: every leftover is reported once.
        let mut waits = JoinSet::new();
        let mut by_task: HashMap<tokio::task::Id, (usize, SweepTarget)> = HashMap::new();
        for (index, target) in leftover.into_iter().enumerate() {
            let mut rx = target.snapshot.status_rx.clone();
            let mut system_deadline = self.sweep_deadline.subscribe();
            let task = waits.spawn(async move {
                wait_end_by_the_system_deadline(&mut rx, &mut system_deadline, deadline).await
            });
            by_task.insert(task.id(), (index, target));
        }
        let mut waited = Vec::new();
        while let Some(joined) = waits.join_next_with_id().await {
            let (task, ended) = match joined {
                Ok((task, ended)) => (task, ended),
                Err(err) => (err.id(), false),
            };
            if let Some((index, target)) = by_task.remove(&task) {
                waited.push((index, target, ended));
            }
        }
        waited.sort_by_key(|(index, ..)| *index);
        let mut swept: Vec<(ActorId, StopReport)> = Vec::new();
        let mut stragglers: Vec<SweepTarget> = Vec::new();
        for (_, target, ended) in waited {
            // A leftover that ended is reported from the plane it was found
            // with, never looked up by id again (its registry entry may be
            // gone).
            if ended {
                let report = leftover_report(
                    &target.snapshot.status_rx,
                    false,
                    self.current_sweep_deadline(deadline),
                );
                swept.push((target.id, report));
            } else {
                stragglers.push(target);
            }
        }
        if !stragglers.is_empty() {
            swept.extend(
                self.concurrent_force_sweep(stragglers, Some(deadline))
                    .await,
            );
        }

        // Shutdown has fully completed: the last one in flight moves the
        // system to `Defunct` rather than leaving `ShuttingDown` set forever,
        // so `reactivate` can return this system's name to service, and
        // clears the system's sweep deadline in the same step.
        in_flight.completed = true;
        drop(in_flight);

        ShutdownReport { outcomes, swept }
    }

    /// Folds a sweep's own deadline into the system's and returns the result,
    /// the deadline this sweep arms and waits by. The system's deadline becomes
    /// the earlier of the stored one and this sweep's own, where this sweep's
    /// own is first raised to an instant just after now: a lowering by this
    /// call then cannot land on an end already final, so an end compared with
    /// the system's deadline before the lowering and after it reads one way. A
    /// stored deadline already past stays as it is.
    fn fold_sweep_deadline(&self, own: Instant) -> Instant {
        let floor = own.max(saturating_deadline(Instant::now(), Duration::from_nanos(1)));
        let mut folded = floor;
        self.sweep_deadline.send_modify(|stored| {
            folded = stored.map_or(floor, |earlier| earlier.min(floor));
            *stored = Some(folded);
        });
        folded
    }

    /// The system's sweep deadline as it stands now, or `own` when none is
    /// held. A sweep in flight keeps a shutdown counted, so its own folded
    /// value is held until it completes.
    fn current_sweep_deadline(&self, own: Instant) -> Instant {
        self.sweep_deadline
            .borrow()
            .map_or(own, |held| held.min(own))
    }

    /// Stops `roots` one at a time through the sequential ladder, in the
    /// order given. The moment the global deadline cuts a root's ladder, that
    /// root and every root after it go to one concurrent force sweep instead,
    /// the cut root marked with whether its ladder had raised `Kill`.
    async fn stop_roots(
        &self,
        roots: &[RootSnapshot],
        per_actor_timeout: Duration,
        deadline: Instant,
    ) -> Vec<(ActorId, StopReport)> {
        let mut outcomes = Vec::with_capacity(roots.len());
        for (idx, root) in roots.iter().enumerate() {
            match self
                .stop_root_sequential(root, per_actor_timeout, deadline)
                .await
            {
                Ok(outcome) => outcomes.push((root.id.clone(), outcome)),
                Err(raised_kill) => {
                    let targets = roots[idx..]
                        .iter()
                        .enumerate()
                        .map(|(n, root)| SweepTarget {
                            id: root.id.clone(),
                            snapshot: root.clone(),
                            kill_raised: n == 0 && raised_kill,
                        })
                        .collect();
                    outcomes.extend(self.concurrent_force_sweep(targets, None).await);
                    break;
                }
            }
        }
        outcomes
    }

    /// Snapshots every currently-registered ROOT actor's id, registration
    /// sequence, a clone of its status receiver and its kill grace. The
    /// snapshot is independent of the registry entry from this point on: the
    /// `status_rx` clone stays valid even after the entry is removed by a
    /// `RegistryGuard` drop, and it names exactly this incarnation.
    fn snapshot_roots(&self) -> Vec<RootSnapshot> {
        self.by_id
            .iter()
            .filter(|e| e.is_root)
            .map(|e| RootSnapshot {
                id: e.key().clone(),
                registration_seq: e.registration_seq,
                status_rx: e.status_rx.clone(),
                kill_grace: e.kill_grace,
                record: e.stop_lane.abort_record().clone(),
            })
            .collect()
    }

    /// Runs one root through the escalation ladder (`ParentRequest` -> `Kill`
    /// -> `abort()`), each wait bounded by whichever is tighter: the step's
    /// own bound, or the time left before the global deadline. Every step
    /// acts only on the incarnation `root` snapshotted: a later registration
    /// under the same id is never signalled or aborted. The verdict is
    /// `Killed` only once this ladder raised `Kill` on the incarnation; an
    /// incarnation that left the registry before the `Kill` step ends with
    /// the outcome it reaches by itself. Returns `Err(kill_raised)` the
    /// instant the global deadline is hit without having reached a terminal
    /// status, saying whether this ladder had raised `Kill` by then - the
    /// caller then hands this root (and every root after it) to the
    /// concurrent force sweep instead of continuing the sequential ladder.
    async fn stop_root_sequential(
        &self,
        root: &RootSnapshot,
        per_actor_timeout: Duration,
        deadline: Instant,
    ) -> Result<StopReport, bool> {
        let id = &root.id;
        let mut status_rx = root.status_rx.clone();

        // Step 1: ParentRequest (vetoable via pre_stop).
        #[cfg(test)]
        self.run_ladder_hook(LadderPoint::Stop, id);
        self.send_stop(id, &status_rx, StopReason::ParentRequest);
        if wait_terminal(&mut status_rx, time_left(deadline).min(per_actor_timeout)).await {
            return Ok(terminal_report(&status_rx, StopOutcome::Graceful));
        }
        if Instant::now() >= deadline {
            return Err(false);
        }

        // Step 2: Kill (unvetoable, but still only observed cooperatively -
        // an actor stuck inside a callback never reaches the select! that
        // would see it). The system schedules the abort when it raises the
        // Kill, so a shutdown future dropped from here on still ends in one.
        #[cfg(test)]
        self.run_ladder_hook(LadderPoint::Kill, id);
        let kill_raised = self.raise_kill(id, &status_rx);
        let requested = if kill_raised {
            StopOutcome::Killed
        } else {
            StopOutcome::Graceful
        };
        if wait_terminal(&mut status_rx, time_left(deadline).min(root.kill_grace)).await {
            return Ok(terminal_report(&status_rx, requested));
        }
        if Instant::now() >= deadline {
            return Err(kill_raised);
        }

        // Step 3: abort() - the last resort for an actor that never yielded
        // back to the run loop (so Kill was never even observed).
        #[cfg(test)]
        self.run_ladder_hook(LadderPoint::Abort, id);
        self.abort_by_id(id, &status_rx);
        if wait_terminal(&mut status_rx, time_left(deadline).min(ABORT_SETTLE)).await {
            return Ok(terminal_report(&status_rx, requested));
        }

        // Nothing left to hand off to: the concurrent sweep would do exactly
        // what steps 2-3 just did. A genuinely non-yielding task.
        Ok(unresponsive())
    }

    /// Force-stops a batch CONCURRENTLY: `Kill` -> grace -> `abort()` ->
    /// grace, skipping the vetoable `ParentRequest` step entirely. Used once
    /// the global deadline is breached (remaining roots) and for the
    /// leftovers the final sweep still finds running at the global deadline:
    /// in both cases the time for a graceful stop has run out. Every id is
    /// reported exactly once, and no target's wait holds up another's.
    ///
    /// A target is looked up as the incarnation its snapshot kept. One the
    /// registry does not hold as that incarnation is waited on through the
    /// status receiver its snapshot kept, bounded as a found target is (its
    /// kill grace, then the wait after an abort), because the runtime frees
    /// an actor's registry entry before it publishes `Stopped`. A root's
    /// terminal end goes through the one terminal classifier: `Stopped`
    /// holds as `Killed` when this shutdown had raised `Kill` on it and as
    /// `Graceful` otherwise, a sender closed without `Stopped` is `Aborted`.
    /// A leftover's (`sweep_deadline` is the sweep's deadline) goes through
    /// the leftover classifier, [`leftover_report`], against the system's
    /// deadline read when its force ends. One still open after the bound is
    /// `Unresponsive`. A force task that fails (it panics or is cancelled)
    /// still has its target reported, from the target's plane: as its
    /// classifier reads it once the plane is terminal, `Unresponsive` while
    /// it still runs.
    async fn concurrent_force_sweep(
        &self,
        targets: Vec<SweepTarget>,
        sweep_deadline: Option<Instant>,
    ) -> Vec<(ActorId, StopReport)> {
        let mut set = JoinSet::new();
        let count = targets.len();
        // Each task's target, its plane, the outcome its classifier asks for
        // and whether this shutdown raised `Kill` on it, fixed here before
        // the task is spawned.
        let mut by_task: HashMap<
            tokio::task::Id,
            (ActorId, watch::Receiver<StatusValue>, StopOutcome, bool),
        > = HashMap::new();
        for target in targets {
            let SweepTarget {
                id,
                snapshot,
                kill_raised,
            } = target;
            #[cfg(test)]
            self.run_ladder_hook(LadderPoint::SweepLookup, &id);
            let found = self
                .by_id
                .get(&id)
                .filter(|e| e.status_rx.same_channel(&snapshot.status_rx))
                .map(|e| (e.status_rx.clone(), e.stop_lane.clone(), e.kill_grace));
            let Some((status_rx, lane, grace)) = found else {
                // The registry does not hold the incarnation the sweep is after.
                let requested = if kill_raised {
                    StopOutcome::Killed
                } else {
                    StopOutcome::Graceful
                };
                let rx = snapshot.status_rx.clone();
                let record = snapshot.record.clone();
                let grace = snapshot.kill_grace;
                let task =
                    set.spawn(async move { force_stop(rx, None, record, grace, requested).await });
                by_task.insert(task.id(), (id, snapshot.status_rx, requested, kill_raised));
                continue;
            };
            #[cfg(test)]
            self.run_after_kill_lookup();
            // Raise first, then read the abort and arm (see `kill`). A lane
            // that refuses the `Kill` belongs to an incarnation already past
            // its last read of it: nothing is armed, it ends with its own
            // reason, and the verdict is `Killed` only if this shutdown raised
            // `Kill` on it earlier. The wait is the incarnation's own grace,
            // read with the lookup, so it holds after the entry is gone.
            let took = lane.try_raise(StopReason::Kill) != RaiseResult::Refused;
            let raised = took || kill_raised;
            let requested = if raised {
                StopOutcome::Killed
            } else {
                StopOutcome::Graceful
            };
            let record = lane.abort_record().clone();
            let abort = if took {
                self.arm_after_raise(&id, &status_rx, &record)
            } else {
                self.kill_target_for(&id, &status_rx)
                    .and_then(|(abort, _, _)| abort)
            };
            let rx = status_rx.clone();
            let task =
                set.spawn(async move { force_stop(rx, abort, record, grace, requested).await });
            by_task.insert(task.id(), (id, status_rx, requested, raised));
        }

        let mut results = Vec::with_capacity(count);
        while let Some(joined) = set.join_next_with_id().await {
            let (task, report) = match joined {
                Ok((task, report)) => (task, Some(report)),
                Err(err) => (err.id(), None),
            };
            let Some((id, rx, requested, raised)) = by_task.remove(&task) else {
                continue;
            };
            let terminal = match &report {
                Some(report) => report.outcome != StopOutcome::Unresponsive,
                None => read_status(&rx) == ActorStatus::Stopped,
            };
            let report = if !terminal {
                report.unwrap_or_else(unresponsive)
            } else if let Some(deadline) = sweep_deadline {
                leftover_report(&rx, raised, self.current_sweep_deadline(deadline))
            } else {
                report.unwrap_or_else(|| terminal_report(&rx, requested))
            };
            results.push((id, report));
        }
        results
    }

    /// Clones the lane of the incarnation whose status plane is `status_rx`
    /// out of the map guard and drops the guard before raising it. A missing
    /// entry (the actor already died on its own), or one that is a later
    /// incarnation under the same id, is a silent no-op: the caller's
    /// subsequent status wait finds the incarnation already terminal.
    fn send_stop(
        &self,
        id: &ActorId,
        status_rx: &watch::Receiver<StatusValue>,
        reason: StopReason,
    ) {
        let lane = self
            .by_id
            .get(id)
            .filter(|e| e.status_rx.same_channel(status_rx))
            .map(|e| e.stop_lane.clone());
        if let Some(lane) = lane {
            lane.raise(reason);
        }
    }

    /// Raises `Kill` on the incarnation whose status plane is `status_rx` and
    /// schedules the abort of its task once its kill grace has passed.
    /// Returns whether the `Kill` was raised: it is not when the registry does
    /// not hold that incarnation under `id` (it left, or a later incarnation
    /// holds the id) or its lane refuses the request.
    fn raise_kill(&self, id: &ActorId, status_rx: &watch::Receiver<StatusValue>) -> bool {
        let Some(lane) = self
            .by_id
            .get(id)
            .filter(|e| e.status_rx.same_channel(status_rx))
            .map(|e| e.stop_lane.clone())
        else {
            return false;
        };
        #[cfg(test)]
        self.run_after_kill_lookup();
        // Raise first, then read the abort and arm (see `kill`).
        if lane.try_raise(StopReason::Kill) == RaiseResult::Refused {
            return false;
        }
        self.arm_after_raise(id, status_rx, lane.abort_record());
        true
    }

    /// Aborts the task of the incarnation whose status plane is `status_rx`.
    fn abort_by_id(&self, id: &ActorId, status_rx: &watch::Receiver<StatusValue>) {
        let target = self
            .by_id
            .get(id)
            .filter(|e| e.status_rx.same_channel(status_rx))
            .map(|e| (e.abort.clone(), e.stop_lane.abort_record().clone()));
        if let Some((Some(abort), record)) = target {
            record.abort(&abort);
        }
    }
}

// ---------------------------------------------------------------------------
// Stuck-turn watchdog: threshold encoding and the sweep task
// ---------------------------------------------------------------------------

/// Encodes a threshold for storage in `turn_warning_threshold_nanos`: `0`
/// means off. `Some(Duration::ZERO)` is treated the same as `None` (see
/// [`ActorSystem::set_turn_warning_threshold`]), and a `Duration` too large
/// to fit in a `u64` nanosecond count saturates to `u64::MAX` instead of
/// overflowing.
fn threshold_to_nanos(threshold: Option<Duration>) -> u64 {
    match threshold {
        None => 0,
        Some(d) if d.is_zero() => 0,
        Some(d) => u64::try_from(d.as_nanos()).unwrap_or(u64::MAX),
    }
}

/// Decodes a `turn_warning_threshold_nanos` value back into the `Option`
/// surface: `0` is `None`.
fn nanos_to_threshold(nanos: u64) -> Option<Duration> {
    (nanos != 0).then(|| Duration::from_nanos(nanos))
}

/// Clears a sweep task's shared spawn latch back to `false` when dropped:
/// whether the task it travels with returns normally, is aborted mid-flight,
/// or is dropped without ever being polled at all, because the runtime it
/// was spawned on shuts down first. Without this, a sweep task that never
/// clears the latch would leave `sweep_spawned` stuck at `true` forever, and
/// no later runtime could ever spawn a replacement for it.
#[cfg(feature = "tracing")]
struct SweepLatchGuard(Arc<AtomicBool>);

#[cfg(feature = "tracing")]
impl Drop for SweepLatchGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Background task, spawned by [`ActorSystem::ensure_sweep_spawned_with`]
/// whenever none is currently alive for this system, that periodically logs
/// actors whose current turn has run longer than their system's turn
/// warning threshold. Re-resolves its system by name every tick instead of
/// holding a strong reference, and exits the moment that lookup comes back
/// empty - so this task never keeps an otherwise-unreferenced system alive,
/// and never needs a separate shutdown signal. The caller spawns this
/// alongside a [`SweepLatchGuard`] built from the owning system's
/// `sweep_spawned`, so that latch clears on the way out by any exit path,
/// including this task being dropped before it is ever polled, or being
/// dropped outright by its runtime shutting down.
#[cfg(feature = "tracing")]
async fn turn_watchdog_sweep(name: String) {
    let mut last_warned: std::collections::HashMap<ActorId, u64> = std::collections::HashMap::new();

    loop {
        let Some(system) = ActorSystem::get_named(&name) else {
            return;
        };

        let threshold_nanos = system.turn_warning_threshold_nanos.load(Ordering::Relaxed);
        if threshold_nanos == 0 {
            drop(system);
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let threshold = Duration::from_nanos(threshold_nanos);
        let tick = (threshold / 4).clamp(Duration::from_millis(50), Duration::from_secs(1));

        // Collected with no `.await` in the loop body: never hold a
        // `DashMap` guard across one. `snapshot()` reads the token and the
        // elapsed time from a single atomic load, so the token recorded
        // below always corresponds to the exact turn `elapsed` was measured
        // against - unlike calling `elapsed()` and `token()` separately,
        // which can straddle the actor ending one turn and starting the
        // next between the two loads.
        let stuck_now: Vec<(ActorId, u64, Duration)> = system
            .by_id
            .iter()
            .filter_map(|e| {
                let (token, elapsed) = e.turn_clock.snapshot()?;
                (elapsed > threshold).then(|| (e.key().clone(), token, elapsed))
            })
            .collect();
        drop(system);

        let mut still_stuck: HashSet<ActorId> = HashSet::with_capacity(stuck_now.len());
        for (id, token, elapsed) in &stuck_now {
            still_stuck.insert(id.clone());
            if last_warned.get(id) != Some(token) {
                let elapsed_ms = elapsed.as_millis() as u64;
                let threshold_ms = threshold.as_millis() as u64;
                caught_log!(warn,
                    actor_id = %id,
                    elapsed_ms,
                    threshold_ms,
                    "actor turn exceeds the turn warning threshold"
                );
                last_warned.insert(id.clone(), *token);
            }
        }
        last_warned.retain(|id, _| still_stuck.contains(id));

        tokio::time::sleep(tick).await;
    }
}

/// The points at which a test hook runs: before each step of a root's
/// sequential shutdown ladder, before the force sweep looks a target up, and
/// before `ActorHandle::stop_within` raises its own `Kill`.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LadderPoint {
    Stop,
    Kill,
    Abort,
    SweepLookup,
    StopWithinKill,
}

#[cfg(test)]
type LadderHook = Arc<dyn Fn(LadderPoint, &ActorId) + Send + Sync>;

/// Counts one shutdown in flight (see `ActorSystem::registration_gate`):
/// entered with the phase change out of `Active`, and left when the shutdown
/// ends, completed or dropped. Only a completed shutdown that was the last in
/// flight stores `Defunct`; an abandoned one leaves the system refusing
/// registrations until a later shutdown completes.
struct ShutdownInFlight<'a> {
    system: &'a ActorSystem,
    completed: bool,
}

impl<'a> ShutdownInFlight<'a> {
    fn enter(system: &'a ActorSystem) -> Self {
        let mut in_flight = system
            .registration_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *in_flight += 1;
        system
            .phase
            .store(SystemPhase::ShuttingDown.as_u8(), Ordering::Release);
        Self {
            system,
            completed: false,
        }
    }
}

impl Drop for ShutdownInFlight<'_> {
    fn drop(&mut self) {
        let mut in_flight = self
            .system
            .registration_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *in_flight -= 1;
        if self.completed && *in_flight == 0 {
            // No sweep of this system is in flight any more: the deadline it
            // held for them goes with the move to `Defunct`, under the same
            // lock, so a shutdown after a reactivation folds only its own.
            self.system.sweep_deadline.send_replace(None);
            self.system
                .phase
                .store(SystemPhase::Defunct.as_u8(), Ordering::Release);
        }
    }
}

/// One target of the concurrent force sweep: its id, the snapshot of the
/// incarnation it is (a root's from the start of the shutdown, a non-root
/// leftover's from the listing that found it), and whether this shutdown had
/// already raised `Kill` on it.
struct SweepTarget {
    id: ActorId,
    snapshot: RootSnapshot,
    kill_raised: bool,
}

/// Snapshot of a root actor taken at the start of shutdown: independent of
/// the registry entry (the `status_rx` clone stays valid even after
/// `RegistryGuard` removes the entry), and exact to one incarnation (its
/// status channel is that incarnation's own).
#[derive(Clone)]
struct RootSnapshot {
    id: ActorId,
    registration_seq: u64,
    status_rx: watch::Receiver<StatusValue>,
    kill_grace: Duration,
    /// The incarnation's abort record: a leftover whose registry entry is
    /// already freed is judged by the abort the reaper issues against it.
    record: AbortRecord,
}

/// Time remaining until `deadline`, floored at zero.
fn time_left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Reads an actor's status from its status channel, treating a closed
/// sender as terminal: an aborted task never writes a final status itself,
/// so the sender closing is itself the terminal signal, the same one
/// [`wait_terminal`] already treats as terminal.
pub(crate) fn read_status(rx: &watch::Receiver<StatusValue>) -> ActorStatus {
    if rx.has_changed().is_err() {
        ActorStatus::Stopped
    } else {
        rx.borrow().status
    }
}

/// The reason a terminal plane carries: the one written, or `Kill` for a
/// plane that closed without one. Meaningful only once the plane is
/// terminal.
pub(crate) fn terminal_reason(rx: &watch::Receiver<StatusValue>) -> StopReason {
    rx.borrow().reason_or_kill()
}

/// Waits, without a bound, until the plane is terminal and returns its
/// reason.
pub(crate) async fn wait_reason(rx: &mut watch::Receiver<StatusValue>) -> StopReason {
    loop {
        if rx.borrow().status == ActorStatus::Stopped {
            return rx.borrow().reason_or_kill();
        }
        if rx.changed().await.is_err() {
            return rx.borrow().reason_or_kill();
        }
    }
}

/// Waits, without a bound, until the plane carries the actor's final reason
/// (written at its lane seal; for a task that never reaches its seal, by its
/// abort guard once its name is free, or by its context when it is dropped
/// before its first poll) or closes, and returns that reason.
pub(crate) async fn wait_final_reason(rx: &mut watch::Receiver<StatusValue>) -> StopReason {
    loop {
        if let Some(reason) = rx.borrow().reason.clone() {
            return reason;
        }
        if rx.borrow().status == ActorStatus::Stopped {
            return rx.borrow().reason_or_kill();
        }
        if rx.changed().await.is_err() {
            return rx.borrow().reason_or_kill();
        }
    }
}

/// A final sweep's wait on one leftover: until its plane is terminal
/// (`Stopped` published, or closed) or the deadline passes. The deadline is
/// the earliest of `deadline` and every value the system's sweep deadline
/// takes while the wait runs, so a sweep that starts later with an earlier
/// deadline shortens it. Returns whether the plane was found terminal.
async fn wait_end_by_the_system_deadline(
    rx: &mut watch::Receiver<StatusValue>,
    system: &mut watch::Receiver<Option<Instant>>,
    deadline: Instant,
) -> bool {
    let mut deadline = deadline;
    let mut system_open = true;
    loop {
        if let Some(earlier) = *system.borrow_and_update() {
            deadline = deadline.min(earlier);
        }
        if read_status(rx) == ActorStatus::Stopped {
            return true;
        }
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return true;
                }
            }
            changed = system.changed(), if system_open => {
                if changed.is_err() {
                    system_open = false;
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                return read_status(rx) == ActorStatus::Stopped;
            }
        }
    }
}

/// The one classifier of a final-sweep leftover whose plane is terminal
/// (`Stopped` published, or closed): a status other than `Stopped` (its task
/// ended without publishing it) reads `Aborted`; `Stopped` reads `Killed`
/// when this shutdown's force took a `Kill` on it (`raised`), or when its
/// end became final at or after `deadline` (or carries no instant), and
/// `Graceful` when it became final before. The reason is the one the plane
/// carries. Every terminal leftover goes through here, whichever path
/// produced its report, so one end reads one way whichever call reports it.
fn leftover_outcome(
    rx: &watch::Receiver<StatusValue>,
    raised: bool,
    deadline: Instant,
) -> StopOutcome {
    let value = rx.borrow();
    if value.status != ActorStatus::Stopped {
        return StopOutcome::Aborted;
    }
    if raised || value.final_at.is_none_or(|at| at >= deadline) {
        StopOutcome::Killed
    } else {
        StopOutcome::Graceful
    }
}

/// The [`StopReport`] of a final-sweep leftover whose plane is terminal:
/// [`leftover_outcome`] and the reason the plane carries.
fn leftover_report(
    rx: &watch::Receiver<StatusValue>,
    raised: bool,
    deadline: Instant,
) -> StopReport {
    StopReport {
        outcome: leftover_outcome(rx, raised, deadline),
        reason: Some(terminal_reason(rx)),
    }
}

/// The [`StopReport`] of a terminal wait: the outcome from
/// [`terminal_outcome`] and the reason the plane carries.
pub(crate) fn terminal_report(
    rx: &watch::Receiver<StatusValue>,
    requested: StopOutcome,
) -> StopReport {
    StopReport {
        outcome: terminal_outcome(rx, requested),
        reason: Some(terminal_reason(rx)),
    }
}

/// The report of an actor that reached no terminal status within the
/// call's last bound.
fn unresponsive() -> StopReport {
    StopReport {
        outcome: StopOutcome::Unresponsive,
        reason: None,
    }
}

/// Waits, bounded by `bound`, for `status_rx` to reach a terminal state:
/// `ActorStatus::Stopped`, or the sender dropped without ever writing it (an
/// aborted task - terminal too, matching `ActorHandle::wait_stopped`).
/// Returns `true` if terminal was observed within `bound`.
pub(crate) async fn wait_terminal(
    status_rx: &mut watch::Receiver<StatusValue>,
    bound: Duration,
) -> bool {
    tokio::time::timeout(bounded_wait(bound), async {
        while status_rx.borrow().status != ActorStatus::Stopped {
            if status_rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await
    .is_ok()
}

/// The one classifier of a terminal wait. `requested` is the outcome the
/// call asked for (`Graceful` for a stop that raised a plain request, `Killed`
/// once a `Kill` was raised): it holds when the actor published `Stopped`
/// itself, and a status sender closed without that write means the task was
/// aborted, by the scheduled backstop or by the caller, and is `Aborted`.
/// Every terminal wait of `stop_within`, the shutdown ladder and the force
/// stops answers through here, so one condition gets one verdict on every
/// path, whichever of the deadlines fired first.
pub(crate) fn terminal_outcome(
    status_rx: &watch::Receiver<StatusValue>,
    requested: StopOutcome,
) -> StopOutcome {
    if status_rx.borrow().status == ActorStatus::Stopped {
        requested
    } else {
        StopOutcome::Aborted
    }
}

/// Force-stop wait shared by `kill_by_id` and the concurrent sweep: the
/// target's kill grace -> `abort` (when there is a task to abort) -> settle
/// wait. `requested` is what a `Stopped` the actor publishes itself reads as
/// (`Killed` once a `Kill` was raised on it).
async fn force_stop(
    mut status_rx: watch::Receiver<StatusValue>,
    abort: Option<AbortHandle>,
    record: AbortRecord,
    kill_grace: Duration,
    requested: StopOutcome,
) -> StopReport {
    if wait_terminal(&mut status_rx, kill_grace).await {
        return terminal_report(&status_rx, requested);
    }

    if let Some(abort) = &abort {
        record.abort(abort);
    }
    // Without an abort of its own the force follows the one the reaper
    // issues: no verdict before that abort plus the settle time. A wait for
    // an owed abort is capped by the force's whole budget once more.
    let cap = kill_grace.saturating_add(ABORT_SETTLE);
    if wait_end_or_floor(&mut status_rx, Some(&record), ABORT_SETTLE, cap).await {
        return terminal_report(&status_rx, requested);
    }

    unresponsive()
}

/// Waits for `status_rx` to reach a terminal state, counting `budget` from
/// now, and returns whether it did before the verdict floor of
/// [`rules::verdict_floor`]: the later of the budget's deadline and the
/// first abort recorded against the incarnation plus the settle time, held
/// while an abort is owed to it and not issued yet (up to `cap` past the
/// budget's deadline, plus the settle time of an abort issued inside the
/// cap). The record is read again at each of its changes and
/// once more when the floor read before is reached, so an abort that became
/// owed in the meantime still holds the verdict. With no record and nothing
/// owed, the verdict comes at the budget's deadline exactly.
pub(crate) async fn wait_end_or_floor(
    status_rx: &mut watch::Receiver<StatusValue>,
    record: Option<&AbortRecord>,
    budget: Duration,
    cap: Duration,
) -> bool {
    let budget = bounded_wait(budget);
    let cap = bounded_wait(cap);
    let budget_at = saturating_deadline(Instant::now(), budget);
    let floor = || {
        let (aborted_at, owed) = record.map_or((None, false), AbortRecord::view);
        Instant::from_std(rules::verdict_floor(
            budget_at.into_std(),
            cap,
            aborted_at.map(Instant::into_std),
            owed,
        ))
    };
    let mut changes = record.map(AbortRecord::subscribe);
    loop {
        let at = floor();
        let floor_passed = tokio::select! {
            biased;
            _ = status_rx.wait_for(|v| v.status == ActorStatus::Stopped) => return true,
            changed = async {
                match changes.as_mut() {
                    Some(rx) => rx.changed().await,
                    None => std::future::pending().await,
                }
            } => {
                if changed.is_err() {
                    changes = None;
                }
                false
            }
            () = tokio::time::sleep_until(at) => Instant::now() >= floor(),
        };
        if floor_passed {
            return read_status(status_rx) == ActorStatus::Stopped;
        }
    }
}

// ---------------------------------------------------------------------------
// Reaper - the abort-on-deadline backstop fed by every kill path
// ---------------------------------------------------------------------------

/// The sending half of a reaper's feed.
///
/// The feed is unbounded, so an entry is never refused for lack of room and a
/// sweep never cuts a target's grace short. An entry lives until its abort
/// stage, when the reaper pops it and aborts its task unless the task has
/// ended (then it does nothing), so the feed holds at most the entries armed
/// within the longest `Shutdown` window plus kill grace, `stop_within`
/// window, or shutdown timeout plus kill grace in use. An entry is a
/// deadline and an abort handle, and for a two-stage entry a lane: a few
/// words.
pub(crate) type ReaperFeed = mpsc::UnboundedSender<ReaperEntry>;

/// The live reaper of one runtime: its feed and the `AbortHandle` used to tell
/// whether its backing task (and, with it, the runtime it was spawned on) is
/// still around.
struct ReaperSlot {
    tx: ReaperFeed,
    task: AbortHandle,
}

/// One pending abort deadline, fed by a dying child's link guard or by a kill
/// path.
///
/// An entry has one stage or two. The abort stage is the entry's `deadline`.
/// A first stage, `kill_at`, raises `Kill` on a lane at its instant and
/// leaves the abort stage in place: it is the orderly stop of a supervisor
/// whose own supervisor ended, or a final sweep's deadline, which the abort
/// follows once the target's kill grace has passed after that `Kill`. The
/// grace is the span between the two stages as built, and it is counted
/// again from the moment the `Kill` stage actually fires: an entry whose
/// stages were both already past when it was queued still gives its target
/// the whole grace after its `Kill`.
///
/// An entry that is dropped without having fired its abort aborts its task:
/// every entry belongs to a task that was already sent `Kill` or an orderly
/// stop that ends in one, so the deadline is a courtesy that a reaper unable
/// to keep it (its runtime was shut down, its task never polled, its feed
/// closed) must not turn into a child left alive with nothing watching it.
/// Dropping an entry, before its first stage, between its stages or in the
/// queue, is therefore never a way to cancel an abort.
pub(crate) struct ReaperEntry {
    /// The first stage while it is pending: the instant to raise `Kill`, the
    /// lane to raise it on, and the kill grace the abort follows it by.
    kill_at: Option<(Instant, StopLane, Duration)>,
    /// The abort instant: as built, and for a two-stage entry no earlier
    /// than its kill grace after the moment its `Kill` stage fired.
    deadline: Instant,
    abort: AbortHandle,
    /// The target's abort record: owed from the moment the entry exists
    /// (nothing is owed to a task that already finished), written when the
    /// entry aborts the task.
    record: AbortRecord,
    fired: bool,
}

impl ReaperEntry {
    pub(crate) fn new(deadline: Instant, abort: AbortHandle, record: AbortRecord) -> Self {
        record.owe_for(&abort);
        Self {
            kill_at: None,
            deadline,
            abort,
            record,
            fired: false,
        }
    }

    /// An entry whose first stage raises `Kill` on `lane` at `kill_at`, and
    /// whose abort stage is due at `deadline`.
    pub(crate) fn two_stage(
        kill_at: Instant,
        lane: StopLane,
        deadline: Instant,
        abort: AbortHandle,
    ) -> Self {
        let record = lane.abort_record().clone();
        record.owe_for(&abort);
        Self {
            kill_at: Some((kill_at, lane, deadline.saturating_duration_since(kill_at))),
            deadline,
            abort,
            record,
            fired: false,
        }
    }

    /// The instant the reaper acts on this entry next: the pending first
    /// stage, then the abort.
    fn due(&self) -> Instant {
        self.kill_at
            .as_ref()
            .map_or(self.deadline, |(kill_at, _, _)| *kill_at)
    }

    /// The next instant is due. A pending first stage whose task has not
    /// finished raises `Kill` on its lane, moves the abort stage to no
    /// earlier than its kill grace from now, and hands the entry back, still
    /// unfired, for that stage. Anything else ends the entry through the
    /// record: the abort of a task that has not exited, and the release of
    /// the owed mark of one that has.
    fn fire(mut self) -> Option<Self> {
        if !self.abort.is_finished() {
            if let Some((_, lane, grace)) = self.kill_at.take() {
                lane.raise(StopReason::Kill);
                self.deadline = self
                    .deadline
                    .max(saturating_deadline(Instant::now(), grace));
                return Some(self);
            }
        }
        self.fired = true;
        self.record.abort(&self.abort);
        None
    }
}

impl Drop for ReaperEntry {
    fn drop(&mut self) {
        if !self.fired {
            self.record.abort(&self.abort);
        }
    }
}

impl PartialEq for ReaperEntry {
    fn eq(&self, other: &Self) -> bool {
        self.due() == other.due()
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
        // SOONEST instant first, turning it into a min-heap by the next
        // instant the entry is due.
        other.due().cmp(&self.due())
    }
}

/// The reaper task's state: the feed and the entries waiting for their
/// deadlines. It is built before the task is spawned, so a task dropped before
/// its first poll still owns the feed and the entries queued in it, which
/// abort when they drop.
struct ReaperState {
    rx: mpsc::UnboundedReceiver<ReaperEntry>,
    heap: BinaryHeap<ReaperEntry>,
}

/// One task per [`ActorSystem`] and runtime, lazily spawned on the runtime's
/// first use: aborts a task if it has not exited by its scheduled deadline.
/// Entries arrive through `ActorSystem::feed_reaper` - a synchronous,
/// non-blocking send on an unbounded feed - and are never awaited by
/// anything: a closed feed makes the sender retry once on the reaper its
/// runtime has by then before it aborts.
fn reaper_loop(rx: mpsc::UnboundedReceiver<ReaperEntry>) -> impl std::future::Future<Output = ()> {
    let state = ReaperState {
        rx,
        heap: BinaryHeap::new(),
    };
    reaper_run(state)
}

/// Pops the entry that is due and fires it. An entry with a stage left comes
/// back and is queued again before anything else runs, so no entry is ever
/// held across an await.
fn fire_due(heap: &mut BinaryHeap<ReaperEntry>) {
    if let Some(due) = heap.pop() {
        if let Some(rest) = due.fire() {
            heap.push(rest);
        }
    }
}

async fn reaper_run(mut state: ReaperState) {
    let mut channel_open = true;

    loop {
        if !channel_open {
            match state.heap.peek().map(ReaperEntry::due) {
                Some(due) => {
                    tokio::time::sleep_until(due).await;
                    fire_due(&mut state.heap);
                }
                None => break,
            }
            continue;
        }

        match state.heap.peek().map(ReaperEntry::due) {
            Some(due) => {
                tokio::select! {
                    entry = state.rx.recv() => match entry {
                        Some(e) => state.heap.push(e),
                        None => channel_open = false,
                    },
                    _ = tokio::time::sleep_until(due) => {
                        fire_due(&mut state.heap);
                    }
                }
            }
            None => match state.rx.recv().await {
                Some(e) => state.heap.push(e),
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
    use std::sync::atomic::AtomicBool;
    use tokio::sync::mpsc;

    /// The outcomes of a report's entries, without their reasons.
    fn outcomes_of(entries: &[(ActorId, StopReport)]) -> Vec<(ActorId, StopOutcome)> {
        entries
            .iter()
            .map(|(id, report)| (id.clone(), report.outcome))
            .collect()
    }

    use crate::actor::supervision::{ChildLinkGuard, DEFAULT_KILL_GRACE};

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
        let (_status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, _stop_rx) = StopLane::new();
        ActorHandle::new(
            id.clone(),
            tx,
            system_tx,
            stop_lane,
            1,
            status_rx,
            system_name.into(),
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    // A guard whose registration was replaced (its entry removed by another
    // path and the id registered again) must not remove the newer entry when
    // it finally drops: removal is checked against the registration sequence
    // the guard owns.
    #[tokio::test]
    async fn stale_registry_guard_drop_never_removes_a_newer_registration() {
        let sys = ActorSystem::create(format!("seq-guard-{}", uuid::Uuid::new_v4())).unwrap();
        let id = ActorId::from("anon-child");

        let old_handle = dummy_handle(&id, sys.name());
        let old_seq = sys
            .register_actor::<Dummy>(
                &id,
                None,
                &old_handle,
                false,
                TurnClock::default(),
                DEFAULT_KILL_GRACE,
                Handle::current(),
            )
            .unwrap();
        let old_guard = RegistryGuard::new(sys.clone(), id.clone(), None, old_seq);

        // The old entry is removed by its own sequence and a newer
        // registration lands under the SAME id before the old guard drops.
        sys.unregister_by_id(&id, old_seq);
        let new_handle = dummy_handle(&id, sys.name());
        let new_seq = sys
            .register_actor::<Dummy>(
                &id,
                None,
                &new_handle,
                false,
                TurnClock::default(),
                DEFAULT_KILL_GRACE,
                Handle::current(),
            )
            .unwrap();
        assert_ne!(old_seq, new_seq);

        drop(old_guard);

        let entry = sys
            .by_id
            .get(&id)
            .expect("the stale guard's drop must not remove the newer registration");
        assert_eq!(entry.registration_seq, new_seq);
    }

    // A second live registration under one id is refused, for a root and for
    // a supervised child alike, and the refusal leaves neither a name nor a
    // second entry behind.
    #[tokio::test]
    async fn second_live_registration_under_one_id_is_refused_and_leaves_no_name_behind() {
        for is_root in [true, false] {
            let sys = ActorSystem::create(format!("dup-id-{}", uuid::Uuid::new_v4())).unwrap();
            let id = ActorId::from("one-id");

            let first = dummy_handle(&id, sys.name());
            let first_seq = sys
                .register_actor::<Dummy>(
                    &id,
                    None,
                    &first,
                    is_root,
                    TurnClock::default(),
                    DEFAULT_KILL_GRACE,
                    Handle::current(),
                )
                .unwrap();

            let second = dummy_handle(&id, sys.name());
            let err = sys
                .register_actor::<Dummy>(
                    &id,
                    Some("second-name"),
                    &second,
                    is_root,
                    TurnClock::default(),
                    DEFAULT_KILL_GRACE,
                    Handle::current(),
                )
                .unwrap_err();
            assert_eq!(err, SpawnError::DuplicateChild(id.clone()));
            assert!(
                err.to_string().contains("one-id"),
                "the message names the id, got {err}"
            );
            assert!(
                sys.registered().is_empty(),
                "the refused registration leaves no name behind"
            );
            let entry = sys.by_id.get(&id).expect("the first registration stays");
            assert_eq!(entry.registration_seq, first_seq);
        }
    }

    #[tokio::test]
    async fn matching_seq_guard_drop_removes_its_own_registration() {
        let sys = ActorSystem::create(format!("seq-guard-{}", uuid::Uuid::new_v4())).unwrap();
        let id = ActorId::from("solo-child");

        let handle = dummy_handle(&id, sys.name());
        let seq = sys
            .register_actor::<Dummy>(
                &id,
                None,
                &handle,
                false,
                TurnClock::default(),
                DEFAULT_KILL_GRACE,
                Handle::current(),
            )
            .unwrap();
        let guard = RegistryGuard::new(sys.clone(), id.clone(), None, seq);

        drop(guard);

        assert!(
            sys.by_id.get(&id).is_none(),
            "a guard whose seq still matches the live registration must remove it"
        );
    }

    // - Reaper ----------------------------------------------------------------

    /// A host that never ran its reaper task: the entries are still in the
    /// feed when the host ends, and the task is dropped before its first poll.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reaper_aborts_queued_entries_when_its_task_never_ran() {
        let sys = ActorSystem::create(format!("reaper-idle-{}", uuid::Uuid::new_v4())).unwrap();
        let host = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // The host is never driven, so the reaper task it was given never
        // polls.
        let tx = sys.reaper_handle(host.handle());
        let children: Vec<_> = (0..2)
            .map(|_| tokio::spawn(std::future::pending::<()>()))
            .collect();
        for child in &children {
            tx.send(ReaperEntry::new(
                Instant::now() + Duration::from_secs(30),
                child.abort_handle(),
                AbortRecord::new(),
            ))
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(children.iter().all(|child| !child.is_finished()));

        host.shutdown_background();
        for child in children {
            let ended = tokio::time::timeout(Duration::from_secs(5), child)
                .await
                .expect("a queued entry is aborted when its reaper never ran");
            assert!(ended.unwrap_err().is_cancelled());
        }
    }

    /// An entry that is dropped without having fired aborts its task; one that
    /// fired, or whose task already ended, does nothing more.
    #[tokio::test]
    async fn reaper_entry_dropped_before_its_deadline_aborts_its_task() {
        // A task that is not aborted fails the test at this bound instead of
        // hanging it.
        async fn ended(child: tokio::task::JoinHandle<()>) -> Result<(), tokio::task::JoinError> {
            tokio::time::timeout(Duration::from_secs(5), child)
                .await
                .expect("the task was aborted when its entry was dropped")
        }

        let child = tokio::spawn(std::future::pending::<()>());
        let entry = ReaperEntry::new(
            Instant::now() + Duration::from_secs(30),
            child.abort_handle(),
            AbortRecord::new(),
        );
        drop(entry);
        assert!(ended(child).await.unwrap_err().is_cancelled());

        let child = tokio::spawn(std::future::pending::<()>());
        let entry = ReaperEntry::new(Instant::now(), child.abort_handle(), AbortRecord::new());
        entry.fire();
        assert!(ended(child).await.unwrap_err().is_cancelled());

        let done = tokio::spawn(async {});
        let abort = done.abort_handle();
        done.await.unwrap();
        drop(ReaperEntry::new(Instant::now(), abort, AbortRecord::new()));
    }

    /// The reaper runs on the runtime that first asked for it. When that
    /// runtime is shut down inside a child's grace, the deadline can no
    /// longer be kept by the reaper, so every entry it was keeping is
    /// aborted at once: the child, which lives on another runtime, still ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reaper_aborts_pending_entries_when_its_host_runtime_ends() {
        let sys = ActorSystem::create(format!("reaper-host-{}", uuid::Uuid::new_v4())).unwrap();
        let host = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        // The reaper is spawned on the host runtime.
        let tx = sys.reaper_handle(host.handle());
        let child = tokio::spawn(std::future::pending::<()>());
        let abort = child.abort_handle();
        // Two deadlines far beyond the host's life, both waiting in the
        // reaper's heap when the host ends (it had time to take them in).
        tx.send(ReaperEntry::new(
            Instant::now() + Duration::from_secs(30),
            abort.clone(),
            AbortRecord::new(),
        ))
        .unwrap();
        let second = tokio::spawn(std::future::pending::<()>());
        tx.send(ReaperEntry::new(
            Instant::now() + Duration::from_secs(30),
            second.abort_handle(),
            AbortRecord::new(),
        ))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!abort.is_finished());

        host.shutdown_background();
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            let first = child.await;
            let second = second.await;
            (first, second)
        })
        .await
        .expect("both children are aborted when the reaper's runtime ends");
        assert!(ended.0.unwrap_err().is_cancelled());
        assert!(ended.1.unwrap_err().is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn reaper_aborts_a_child_that_outlives_its_deadline() {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(reaper_loop(rx));

        let join = tokio::spawn(std::future::pending::<()>());
        let abort = join.abort_handle();
        tx.send(ReaperEntry::new(
            Instant::now() + Duration::from_millis(30),
            abort,
            AbortRecord::new(),
        ))
        .unwrap();

        // The clock is paused: it moves when the test advances it, and it
        // also jumps forward on its own to the next pending timer whenever
        // every task is idle, which is what lets the timeout below catch a
        // reaper that never fires without costing any wall time.
        tokio::time::advance(Duration::from_millis(30)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }

        let result = tokio::time::timeout(Duration::from_secs(2), join).await;
        match result {
            Ok(Err(err)) => assert!(err.is_cancelled(), "expected a cancelled join error"),
            other => {
                panic!("expected the reaper to abort the child by its deadline, got {other:?}")
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reaper_never_disturbs_a_child_that_already_exited() {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(reaper_loop(rx));

        let join = tokio::spawn(async { 7u32 });
        let abort = join.abort_handle();
        // Let the task actually finish before the reaper ever hears about it.
        assert_eq!(join.await.unwrap(), 7);

        tx.send(ReaperEntry::new(Instant::now(), abort, AbortRecord::new()))
            .unwrap();
        // Give the reaper a few turns to process the (already-moot)
        // deadline; an already-finished task's `abort()` is a no-op
        // either way, so there is nothing further to observe here beyond
        // "does not panic". The deadline is already due, so no clock
        // advance is needed, only scheduling turns.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }

    /// A task that never ends on its own, with a lane to raise on.
    fn parked_with_a_lane() -> (
        tokio::task::JoinHandle<()>,
        StopLane,
        watch::Receiver<crate::types::LaneState>,
    ) {
        let (lane, lane_rx) = StopLane::new();
        (tokio::spawn(std::future::pending::<()>()), lane, lane_rx)
    }

    async fn turns() {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reaper_raises_kill_at_the_first_stage_and_aborts_at_the_second() {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(reaper_loop(rx));
        let (child, lane, lane_rx) = parked_with_a_lane();
        let start = Instant::now();
        tx.send(ReaperEntry::two_stage(
            start + Duration::from_millis(100),
            lane,
            start + Duration::from_millis(300),
            child.abort_handle(),
        ))
        .unwrap();
        turns().await;

        tokio::time::advance(Duration::from_millis(99)).await;
        turns().await;
        assert!(
            lane_rx.borrow().reason.is_none(),
            "nothing is raised before the first stage"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        turns().await;
        assert!(
            matches!(lane_rx.borrow().reason, Some(StopReason::Kill)),
            "the first stage raises Kill"
        );
        assert!(!child.is_finished(), "the first stage does not abort");

        tokio::time::advance(Duration::from_millis(199)).await;
        turns().await;
        assert!(!child.is_finished(), "the abort waits for its own instant");
        tokio::time::advance(Duration::from_millis(1)).await;
        let ended = tokio::time::timeout(Duration::from_secs(2), child)
            .await
            .expect("the second stage aborts the task at its instant");
        assert!(ended.unwrap_err().is_cancelled());
    }

    /// A first stage that fires late (its instant and its abort's both
    /// past, as when the reaper's runtime was busy) still gives the task its
    /// kill grace after the `Kill`: the abort stage moves to one grace from
    /// when the `Kill` fired.
    #[tokio::test(start_paused = true)]
    async fn a_kill_stage_fired_late_still_gives_its_kill_grace() {
        let (child, lane, lane_rx) = parked_with_a_lane();
        let start = Instant::now();
        let entry = ReaperEntry::two_stage(
            start + Duration::from_millis(100),
            lane,
            start + Duration::from_millis(500),
            child.abort_handle(),
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        let fired_at = Instant::now();
        let rest = entry.fire().expect("the Kill stage hands its entry back");
        assert!(matches!(lane_rx.borrow().reason, Some(StopReason::Kill)));
        assert!(
            rest.due() >= fired_at + Duration::from_millis(400),
            "the abort is one kill grace after the late Kill"
        );
        assert!(!child.is_finished());
        child.abort();
    }

    /// A plane that holds `status`, `reason` and the instant `final_at` (a
    /// watch keeps its last value once its sender is gone).
    fn plane(
        status: ActorStatus,
        reason: StopReason,
        final_at: Option<Instant>,
    ) -> watch::Receiver<StatusValue> {
        watch::channel(StatusValue {
            status,
            reason: Some(reason),
            final_at,
        })
        .1
    }

    #[test]
    fn an_ended_stop_reads_what_was_requested_when_it_published_stopped() {
        use StopOutcome::{Aborted, Graceful, Killed};
        let stopped = plane(ActorStatus::Stopped, StopReason::Kill, None);
        assert_eq!(terminal_outcome(&stopped, Graceful), Graceful);
        assert_eq!(terminal_outcome(&stopped, Killed), Killed);
        let (tx, closed) = watch::channel(StatusValue::new(ActorStatus::Stopping));
        drop(tx);
        assert_eq!(terminal_outcome(&closed, Graceful), Aborted);
        assert_eq!(terminal_outcome(&closed, Killed), Aborted);
    }

    /// The leftover classifier, row by row: a plane that published `Stopped`
    /// reads by the instant its end became final against the deadline, and
    /// `Killed` whenever this shutdown took a `Kill` on it; a plane closed
    /// without `Stopped` reads `Aborted` whatever its instant.
    #[test]
    fn a_leftover_reads_by_the_instant_its_end_became_final() {
        use StopOutcome::{Aborted, Graceful, Killed};
        let deadline = Instant::now() + Duration::from_secs(60);
        let before = deadline - Duration::from_secs(1);
        let after = deadline + Duration::from_secs(1);
        let rows = [
            (Some(before), false, Graceful),
            (Some(before), true, Killed),
            (Some(deadline), false, Killed),
            (Some(after), false, Killed),
            (None, false, Killed),
        ];
        for (final_at, raised, want) in rows {
            let stopped = plane(ActorStatus::Stopped, StopReason::Graceful, final_at);
            let report = leftover_report(&stopped, raised, deadline);
            assert_eq!(
                report.outcome, want,
                "Stopped, final at {final_at:?}, raised {raised}"
            );
            assert_eq!(report.reason, Some(StopReason::Graceful));
        }
        // A plane that closed without `Stopped`, its reason final before the
        // deadline (an abort guard's write): aborted, never read by its
        // instant.
        let (tx, closed) = watch::channel(StatusValue::new(ActorStatus::Stopping));
        StatusValue::write_reason(&tx, StopReason::Kill, false);
        drop(tx);
        assert!(closed.borrow().final_at.is_some_and(|at| at < deadline));
        assert_eq!(leftover_outcome(&closed, false, deadline), Aborted);
    }

    /// A sweep's wait already parked follows the system's deadline when it
    /// is lowered: it gives up at the new deadline, not at its own.
    #[tokio::test(start_paused = true)]
    async fn a_parked_wait_follows_a_system_deadline_lowered_while_it_waits() {
        let (status_tx, mut rx) = watch::channel(StatusValue::new(ActorStatus::Stopping));
        let (system_tx, mut system) =
            watch::channel(Some(Instant::now() + Duration::from_secs(10)));
        let own = Instant::now() + Duration::from_secs(5);
        let mut wait = Box::pin(wait_end_by_the_system_deadline(&mut rx, &mut system, own));
        let parked = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(wait.as_mut(), cx).is_pending())
        })
        .await;
        assert!(parked, "the wait parks on its leftover and its deadlines");
        system_tx.send_replace(Some(Instant::now() + Duration::from_millis(100)));
        let seen = tokio::time::timeout(Duration::from_millis(200), wait).await;
        assert_eq!(
            seen,
            Ok(false),
            "the wait gave up at the lowered deadline, the leftover not seen to end"
        );
        drop(status_tx);
    }

    /// An incarnation that ended before an entry's stages are due is left
    /// alone at both: its lane is not raised, and nothing is aborted.
    #[tokio::test(start_paused = true)]
    async fn a_two_stage_entry_whose_task_ended_does_nothing_at_either_stage() {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(reaper_loop(rx));
        let (lane, lane_rx) = StopLane::new();
        let ended = tokio::spawn(async {});
        let abort = ended.abort_handle();
        ended.await.unwrap();
        let start = Instant::now();
        tx.send(ReaperEntry::two_stage(
            start + Duration::from_millis(100),
            lane,
            start + Duration::from_millis(300),
            abort,
        ))
        .unwrap();
        turns().await;
        tokio::time::advance(Duration::from_millis(400)).await;
        turns().await;
        assert!(
            lane_rx.borrow().reason.is_none(),
            "no Kill is raised for an incarnation that already ended"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reaper_orders_two_stage_and_single_stage_entries_by_their_next_due_instant() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let (staged, staged_lane, staged_rx) = parked_with_a_lane();
        let single = tokio::spawn(std::future::pending::<()>());
        let early = tokio::spawn(std::future::pending::<()>());
        let mut heap = BinaryHeap::new();
        // The two-stage entry's abort is the latest instant, its first stage
        // the middle one: it sorts by the stage that is next.
        heap.push(ReaperEntry::new(
            at(500),
            single.abort_handle(),
            AbortRecord::new(),
        ));
        heap.push(ReaperEntry::two_stage(
            at(100),
            staged_lane,
            at(1_000),
            staged.abort_handle(),
        ));
        heap.push(ReaperEntry::new(
            at(50),
            early.abort_handle(),
            AbortRecord::new(),
        ));

        fire_due(&mut heap);
        turns().await;
        assert!(early.is_finished(), "the earliest instant is due first");
        assert!(!staged.is_finished() && !single.is_finished());

        fire_due(&mut heap);
        turns().await;
        assert!(
            matches!(staged_rx.borrow().reason, Some(StopReason::Kill)),
            "the two-stage entry's first stage is due before the single abort"
        );
        assert!(!staged.is_finished() && !single.is_finished());
        assert_eq!(heap.len(), 2, "the entry is queued again for its abort");

        fire_due(&mut heap);
        turns().await;
        assert!(single.is_finished(), "the single entry is due next");
        assert!(!staged.is_finished());

        fire_due(&mut heap);
        turns().await;
        assert!(staged.is_finished(), "the abort stage is due last");
        assert!(heap.is_empty());
    }

    #[tokio::test]
    async fn a_two_stage_entry_dropped_between_its_stages_aborts_its_task() {
        let (child, lane, lane_rx) = parked_with_a_lane();
        let entry = ReaperEntry::two_stage(
            Instant::now(),
            lane,
            Instant::now() + Duration::from_secs(30),
            child.abort_handle(),
        );
        let rest = entry
            .fire()
            .expect("the first stage hands the entry back for its abort");
        assert!(matches!(lane_rx.borrow().reason, Some(StopReason::Kill)));
        assert!(!rest.fired, "only the abort stage marks an entry fired");
        assert!(!child.is_finished());

        drop(rest);
        let ended = tokio::time::timeout(Duration::from_secs(5), child)
            .await
            .expect("dropping the entry between its stages aborts the task");
        assert!(ended.unwrap_err().is_cancelled());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_two_stage_entry_in_the_queue_aborts_its_task_when_the_reaper_s_host_ends_before_the_first_stage(
    ) {
        let sys = ActorSystem::create(format!("reaper-stage-{}", uuid::Uuid::new_v4())).unwrap();
        let host = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let tx = sys.reaper_handle(host.handle());
        let (child, lane, lane_rx) = parked_with_a_lane();
        tx.send(ReaperEntry::two_stage(
            Instant::now() + Duration::from_secs(30),
            lane,
            Instant::now() + Duration::from_secs(60),
            child.abort_handle(),
        ))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!child.is_finished());

        host.shutdown_background();
        let ended = tokio::time::timeout(Duration::from_secs(5), child)
            .await
            .expect("an entry still waiting for its first stage aborts when its host ends");
        assert!(ended.unwrap_err().is_cancelled());
        assert!(
            lane_rx.borrow().reason.is_none(),
            "the first stage never ran"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_two_stage_entry_survives_a_closed_feed_until_its_abort_stage() {
        let (tx, rx) = mpsc::unbounded_channel();
        let reaper = tokio::spawn(reaper_loop(rx));
        let (child, lane, lane_rx) = parked_with_a_lane();
        let start = Instant::now();
        tx.send(ReaperEntry::two_stage(
            start + Duration::from_millis(100),
            lane,
            start + Duration::from_millis(300),
            child.abort_handle(),
        ))
        .unwrap();
        turns().await;
        // The feed closes after the entry is queued.
        drop(tx);
        turns().await;

        tokio::time::advance(Duration::from_millis(100)).await;
        turns().await;
        assert!(
            matches!(lane_rx.borrow().reason, Some(StopReason::Kill)),
            "the first stage runs although the feed is closed"
        );
        assert!(!child.is_finished(), "and the entry is still queued");
        assert!(!reaper.is_finished());

        tokio::time::advance(Duration::from_millis(200)).await;
        let ended = tokio::time::timeout(Duration::from_secs(2), child)
            .await
            .expect("the abort stage runs at its instant");
        assert!(ended.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), reaper)
            .await
            .expect("the reaper ends once its queue is empty")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn reaper_pops_the_soonest_deadline_first() {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(reaper_loop(rx));

        let late = tokio::spawn(std::future::pending::<()>());
        let soon = tokio::spawn(std::future::pending::<()>());

        // Fed out of order: the later deadline is enqueued first.
        tx.send(ReaperEntry::new(
            Instant::now() + Duration::from_millis(300),
            late.abort_handle(),
            AbortRecord::new(),
        ))
        .unwrap();
        tx.send(ReaperEntry::new(
            Instant::now() + Duration::from_millis(30),
            soon.abort_handle(),
            AbortRecord::new(),
        ))
        .unwrap();

        // Advance just past the earlier deadline: `soon` must already be
        // reaped while `late`'s deadline has not arrived yet.
        tokio::time::advance(Duration::from_millis(30)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert!(
            soon.is_finished(),
            "soon must be reaped by its own, earlier deadline"
        );
        assert!(
            !late.is_finished(),
            "late's deadline has not arrived yet; it must still be running"
        );

        // Advance past the later deadline too: `late` must now be reaped.
        tokio::time::advance(Duration::from_millis(300)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert!(
            late.is_finished(),
            "late must be reaped once its own deadline arrives"
        );

        tokio::time::timeout(Duration::from_secs(2), soon)
            .await
            .expect("soon must be reaped by its own, earlier deadline")
            .expect_err("soon must be cancelled, not completed");
        tokio::time::timeout(Duration::from_secs(2), late)
            .await
            .expect("late must be reaped once its own deadline arrives")
            .expect_err("late must be cancelled, not completed");
    }

    // - Turn-watchdog sweep latch ---------------------------------------------

    /// Builds a bare, unstarted single-threaded runtime for driving the
    /// sweep by hand - not a `#[tokio::test]` function, since the whole
    /// point is to control exactly when a runtime exists and when it is
    /// dropped.
    #[cfg(feature = "tracing")]
    #[test]
    fn sweep_latch_clears_when_its_runtime_drops_and_respawns_on_the_next_one() {
        let sys = ActorSystem::create(format!("sweep-latch-{}", uuid::Uuid::new_v4())).unwrap();

        let rt_a = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt_a.block_on(async {
            sys.set_turn_warning_threshold(Some(Duration::from_millis(50)));
            // Give the sweep task a chance to run at least one iteration
            // before its runtime disappears, so this phase exercises the
            // steady-state path: a task that is alive and has actually been
            // polled. The third phase below exercises the opposite case, a
            // task dropped before it is ever polled at all.
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        assert!(
            sys.sweep_spawned.load(Ordering::Acquire),
            "the sweep task should be alive after starting it inside runtime A"
        );

        drop(rt_a);
        assert!(
            !sys.sweep_spawned.load(Ordering::Acquire),
            "dropping the sweep's own runtime must clear the latch, not leave it stuck forever"
        );

        let rt_b = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt_b.block_on(async {
            // The threshold is already `Some` from runtime A; setting it
            // again inside runtime B must still respawn the sweep, since
            // the latch cleared when runtime A dropped.
            sys.set_turn_warning_threshold(Some(Duration::from_millis(50)));
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        assert!(
            sys.sweep_spawned.load(Ordering::Acquire),
            "a threshold set inside a fresh runtime must respawn the sweep"
        );

        rt_b.block_on(async {
            sys.set_turn_warning_threshold(None);
        });
        drop(rt_b);
        assert!(
            !sys.sweep_spawned.load(Ordering::Acquire),
            "dropping runtime B must clear the latch too, ready for the next phase"
        );

        // Phase 3: a sweep task spawned but never polled at all - its
        // runtime disappears before anything ever gives it a turn. The
        // compare-exchange in `ensure_sweep_spawned_with` sets the latch
        // synchronously, before the future is even spawned, so an unpolled
        // task still leaving it stuck would break the guarantee: the guard is
        // built and moved into the future BEFORE the spawn, not constructed
        // as the first line of the future's own body, so it drops - and
        // clears the latch - along with the future's other captures even
        // when the future body never runs.
        let rt_c = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        {
            let _enter = rt_c.enter();
            sys.set_turn_warning_threshold(Some(Duration::from_millis(50)));
        }
        assert!(
            sys.sweep_spawned.load(Ordering::Acquire),
            "the latch is set as soon as the sweep is spawned, whether or not it is ever polled"
        );
        drop(rt_c);
        assert!(
            !sys.sweep_spawned.load(Ordering::Acquire),
            "a sweep task dropped before its first poll must still clear the latch"
        );

        sys.set_turn_warning_threshold(None);
    }

    // - Reaper survives a runtime change --------------------------------------

    /// Mirrors the sweep-latch test's shape: two hand-built multi-thread
    /// runtimes on one system, driven by hand (not `#[tokio::test]`) so the
    /// test controls exactly when each runtime exists and when it drops.
    ///
    /// Runtime A hosts the system's first reaper, then is dropped out from
    /// under it: the stored sender's receiver goes away with it, so a
    /// send against it comes back closed. A guard holding that now-
    /// stale sender, dropped later inside runtime B, must still honor its
    /// child's full grace via a freshly resolved reaper on B - not degrade
    /// straight to an immediate abort. The child's future records the
    /// instant it is torn down, so the check reads when the abort actually
    /// happened: a late wake or a busy worker only makes that instant later,
    /// never earlier, and the bound holds on any host.
    #[test]
    fn guard_drop_across_a_runtime_change_still_honors_the_grace() {
        let sys =
            ActorSystem::create(format!("reaper-runtime-change-{}", uuid::Uuid::new_v4())).unwrap();

        // Host the FIRST reaper on runtime A, then drop A out from under it.
        let rt_a = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let stale_tx = sys.reaper_handle(rt_a.handle());
        drop(rt_a);

        let rt_b = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let grace = Duration::from_millis(100);
        let torn_down = Arc::new(Mutex::new(None::<Instant>));

        /// Records the instant the future holding it is dropped.
        struct TornDown(Arc<Mutex<Option<Instant>>>);
        impl Drop for TornDown {
            fn drop(&mut self) {
                *self.0.lock().unwrap() = Some(Instant::now());
            }
        }

        rt_b.block_on(async move {
            let stamp = TornDown(torn_down.clone());
            let join: tokio::task::JoinHandle<()> = tokio::spawn(async move {
                let _stamp = stamp;
                std::future::pending::<()>().await;
            });
            let abort = join.abort_handle();
            let (lane, _lane_rx) = StopLane::new();

            let guard = ChildLinkGuard::new(
                lane,
                abort,
                stale_tx,
                crate::actor::supervision::rules::InheritedStop::Kill { grace },
                sys.clone(),
                Handle::current(),
            );
            let dropped_at = Instant::now();
            drop(guard);

            // The fresh reaper on runtime B aborts the child once the
            // original grace elapses; the bound here only catches a hang.
            let result = tokio::time::timeout(Duration::from_secs(10), join).await;
            match result {
                Ok(Err(join_err)) => assert!(
                    join_err.is_cancelled(),
                    "the fresh reaper on the new runtime must still abort the child \
                     once its grace elapses"
                ),
                other => panic!("expected the child to be aborted by its grace, got {other:?}"),
            }
            let aborted_at = torn_down
                .lock()
                .unwrap()
                .expect("the abort dropped the child's future");
            let waited = aborted_at.saturating_duration_since(dropped_at);
            assert!(
                waited >= grace,
                "the child must live out its full {grace:?} grace, aborted after \
                 {waited:?}: an earlier abort means the runtime switch degraded \
                 instead of retrying on the current reaper"
            );
        });
    }

    // - The reaper follows the runtime -----------------------------------------

    /// Parks in `handle` once messaged, and says so.
    struct Parker {
        parked: Arc<AtomicBool>,
    }

    impl Parker {
        fn new() -> (Parker, Arc<AtomicBool>) {
            let parked = Arc::new(AtomicBool::new(false));
            (
                Parker {
                    parked: parked.clone(),
                },
                parked,
            )
        }
    }

    impl Actor for Parker {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            self.parked.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    fn undriven_host() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// A reaper on a runtime that is alive but never driven holds only its own
    /// runtime's entries: a guard built on a driven runtime still reaches a
    /// reaper that runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reaper_on_a_driven_runtime_is_not_replaced_by_one_on_an_undriven_host() {
        let sys = ActorSystem::create(format!("reaper-two-{}", uuid::Uuid::new_v4())).unwrap();
        let host = undriven_host();
        // The system's first reaper lives on the host, which never runs it.
        let _host_feed = sys.reaper_handle(host.handle());

        let child = tokio::spawn(std::future::pending::<()>());
        let (lane, _lane_rx) = StopLane::new();
        let grace = Duration::from_millis(100);
        let guard = ChildLinkGuard::new(
            lane,
            child.abort_handle(),
            sys.reaper_handle(&Handle::current()),
            crate::actor::supervision::rules::InheritedStop::Kill { grace },
            sys.clone(),
            Handle::current(),
        );
        drop(guard);
        let ended = tokio::time::timeout(grace + ABORT_SETTLE + Duration::from_millis(500), child)
            .await
            .expect("the reaper of the driven runtime aborts the child once its grace passed");
        assert!(ended.unwrap_err().is_cancelled());
        host.shutdown_background();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reaper_handle_keeps_one_reaper_per_runtime() {
        let sys = ActorSystem::create(format!("reaper-keyed-{}", uuid::Uuid::new_v4())).unwrap();

        // The same runtime answers the same feed; another driven runtime
        // answers its own.
        let here = Handle::current();
        let first = sys.reaper_handle(&here);
        assert!(first.same_channel(&sys.reaper_handle(&here)));
        let other = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let elsewhere = sys.reaper_handle(other.handle());
        assert!(!first.same_channel(&elsewhere));
        assert!(elsewhere.same_channel(&sys.reaper_handle(other.handle())));

        // A slot whose task finished is replaced when its runtime is asked
        // for the reaper again: a runtime identity may be reused, and a
        // finished slot must never be returned.
        let done = tokio::spawn(async {});
        let finished = done.abort_handle();
        done.await.unwrap();
        let (stale_tx, _stale_rx) = mpsc::unbounded_channel();
        sys.reaper.lock().unwrap().insert(
            here.id(),
            ReaperSlot {
                tx: stale_tx.clone(),
                task: finished,
            },
        );
        let replaced = sys.reaper_handle(&here);
        assert!(!replaced.same_channel(&stale_tx));
        assert!(!sys.reaper.lock().unwrap()[&here.id()].task.is_finished());

        // The finished slots of other runtimes are pruned when a fresh slot is
        // inserted, and not on a lookup.
        let gone = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let gone_handle = gone.handle().clone();
        let gone_id = gone_handle.id();
        let _gone_feed = sys.reaper_handle(&gone_handle);
        gone.shutdown_background();
        // Wait for the reaper task of the ended runtime to read as finished.
        for _ in 0..200 {
            if sys.reaper.lock().unwrap()[&gone_id].task.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(sys.reaper.lock().unwrap()[&gone_id].task.is_finished());
        let _lookup = sys.reaper_handle(&here);
        assert!(
            sys.reaper.lock().unwrap().contains_key(&gone_id),
            "a lookup prunes nothing"
        );
        let third = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let _fresh = sys.reaper_handle(third.handle());
        assert!(
            !sys.reaper.lock().unwrap().contains_key(&gone_id),
            "a fresh slot prunes the finished ones"
        );

        // A feed whose runtime has ended ends in the entry's drop, an abort,
        // and not in a hang.
        let child = tokio::spawn(std::future::pending::<()>());
        sys.feed_reaper(
            &gone_handle,
            ReaperEntry::new(
                Instant::now() + Duration::from_secs(30),
                child.abort_handle(),
                AbortRecord::new(),
            ),
        );
        let ended = tokio::time::timeout(Duration::from_secs(5), child)
            .await
            .expect("an entry that cannot be queued aborts its task");
        assert!(ended.unwrap_err().is_cancelled());
        other.shutdown_background();
        third.shutdown_background();
    }

    // - A Kill raised between an entry's lookup and its abort being stored ----

    /// A registered entry whose task is spawned but whose abort handle is not
    /// stored yet; the hook stores it at the point between a system-side
    /// kill's lookup and its raise. The returned task owns the status sender
    /// and the lane's receiver, so its end is observable.
    fn pre_attach_target(
        grace: Duration,
    ) -> (
        Arc<ActorSystem>,
        String,
        ActorId,
        tokio::task::JoinHandle<()>,
    ) {
        let sys = ActorSystem::create(format!("pre-attach-hook-{}", uuid::Uuid::new_v4())).unwrap();
        let name = format!("hook-target-{}", uuid::Uuid::new_v4());
        let id = ActorId::from(name.clone());
        let (tx, _rx) = mpsc::channel(1);
        let (system_tx, _system_rx) = mpsc::channel(1);
        let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, stop_rx) = StopLane::new();
        let handle = ActorHandle::<Dummy>::new(
            id.clone(),
            tx,
            system_tx,
            stop_lane,
            1,
            status_rx,
            sys.name().into(),
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let seq = sys
            .register_actor::<Dummy>(
                &id,
                Some(&name),
                &handle,
                true,
                TurnClock::default(),
                grace,
                Handle::current(),
            )
            .unwrap();
        let task = tokio::spawn(async move {
            let _owned = (status_tx, stop_rx);
            std::future::pending::<()>().await;
        });
        let abort = task.abort_handle();
        let hook_sys = sys.clone();
        let (hook_id, hook_name) = (id.clone(), name.clone());
        *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
            hook_sys.attach_abort(&hook_id, Some(&hook_name), abort.clone(), seq);
        }));
        (sys, name, id, task)
    }

    async fn ends_by_the_backstop(task: tokio::task::JoinHandle<()>, grace: Duration, what: &str) {
        let ended = tokio::time::timeout(grace + ABORT_SETTLE + Duration::from_millis(500), task)
            .await
            .unwrap_or_else(|_| panic!("{what}: no backstop was armed"));
        assert!(ended.unwrap_err().is_cancelled());
    }

    /// Registers a root whose task owns its status sender and lane receiver,
    /// with its abort handle stored, and returns the task.
    fn registered_root(
        sys: &Arc<ActorSystem>,
        n: usize,
        grace: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let id = ActorId::from(format!("sweep-{n}"));
        let (tx, _rx) = mpsc::channel(1);
        let (system_tx, _system_rx) = mpsc::channel(1);
        let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, stop_rx) = StopLane::new();
        let handle = ActorHandle::<Dummy>::new(
            id.clone(),
            tx,
            system_tx,
            stop_lane,
            1,
            status_rx,
            sys.name().into(),
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let seq = sys
            .register_actor::<Dummy>(
                &id,
                None,
                &handle,
                true,
                TurnClock::default(),
                grace,
                Handle::current(),
            )
            .unwrap();
        let task = tokio::spawn(async move {
            let _owned = (status_tx, stop_rx);
            std::future::pending::<()>().await;
        });
        sys.attach_abort(&id, None, task.abort_handle(), seq);
        task
    }

    /// One sweep over more targets than any fixed feed capacity could hold
    /// (1,200) gives every target its full grace: none is aborted before its
    /// deadline. On a single thread the sweep arms every entry before any
    /// reaper turn can drain the feed, so a bounded feed would drop entries.
    #[tokio::test(flavor = "current_thread")]
    async fn a_large_sweep_gives_every_target_its_full_grace() {
        let sys = ActorSystem::create(format!("sweep-big-{}", uuid::Uuid::new_v4())).unwrap();
        let grace = Duration::from_millis(600);
        let tasks: Vec<_> = (0..1200).map(|n| registered_root(&sys, n, grace)).collect();
        let started = Instant::now();
        let sweep = {
            let sys = sys.clone();
            // A zero global timeout sends every root to the concurrent sweep.
            tokio::spawn(async move {
                sys.shutdown_with(ShutdownPolicy {
                    timeout: Duration::ZERO,
                    per_actor_timeout: Duration::from_secs(5),
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        let aborted_early = tasks.iter().filter(|task| task.is_finished()).count();
        assert_eq!(
            aborted_early, 0,
            "{aborted_early} of 1200 targets were aborted before their {grace:?} grace"
        );
        for task in tasks {
            let ended = tokio::time::timeout(grace + ABORT_SETTLE + Duration::from_secs(3), task)
                .await
                .expect("every target is aborted once its grace passed");
            assert!(ended.unwrap_err().is_cancelled());
        }
        assert!(
            started.elapsed() >= grace,
            "no target ended before its grace"
        );
        let report = tokio::time::timeout(Duration::from_secs(10), sweep)
            .await
            .expect("the shutdown returns")
            .unwrap();
        assert_eq!(report.outcomes.len(), 1200);
    }

    const HOOK_GRACE: Duration = Duration::from_millis(100);

    /// A grace far longer than the shutdown futures of the dropped-call tests
    /// are polled for, so only the system's own backstop can end the target
    /// before it.
    const SLOW_GRACE: Duration = Duration::from_millis(1000);

    async fn kill_by_name_arms_when_the_abort_is_stored_after_the_lookup() {
        let (sys, name, _id, task) = pre_attach_target(HOOK_GRACE);
        sys.kill(&name).await.unwrap();
        ends_by_the_backstop(task, HOOK_GRACE, "kill(name)").await;
    }

    async fn kill_by_id_arms_when_the_abort_is_stored_after_the_lookup() {
        let (sys, _name, id, task) = pre_attach_target(HOOK_GRACE);
        // The call is dropped at once: only the system's backstop is left.
        let _ = tokio::time::timeout(Duration::ZERO, sys.kill_by_id(&id)).await;
        ends_by_the_backstop(task, HOOK_GRACE, "kill_by_id").await;
    }

    async fn shutdown_kill_step_arms_when_the_abort_is_stored_after_the_lookup() {
        let (sys, _name, _id, task) = pre_attach_target(SLOW_GRACE);
        let policy = ShutdownPolicy {
            timeout: Duration::from_secs(30),
            per_actor_timeout: Duration::from_millis(10),
        };
        let _ = tokio::time::timeout(Duration::from_millis(60), sys.shutdown_with(policy)).await;
        ends_by_the_backstop(task, SLOW_GRACE, "the shutdown's Kill step").await;
    }

    async fn force_sweep_arms_when_the_abort_is_stored_after_the_lookup() {
        let (sys, _name, _id, task) = pre_attach_target(SLOW_GRACE);
        let policy = ShutdownPolicy {
            timeout: Duration::from_millis(10),
            per_actor_timeout: Duration::from_secs(5),
        };
        let _ = tokio::time::timeout(Duration::from_millis(60), sys.shutdown_with(policy)).await;
        ends_by_the_backstop(task, SLOW_GRACE, "the concurrent force sweep").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_by_name_arms_when_the_abort_is_stored_after_the_lookup_multi_thread() {
        kill_by_name_arms_when_the_abort_is_stored_after_the_lookup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn kill_by_name_arms_when_the_abort_is_stored_after_the_lookup_current_thread() {
        kill_by_name_arms_when_the_abort_is_stored_after_the_lookup().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_by_id_arms_when_the_abort_is_stored_after_the_lookup_multi_thread() {
        kill_by_id_arms_when_the_abort_is_stored_after_the_lookup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn kill_by_id_arms_when_the_abort_is_stored_after_the_lookup_current_thread() {
        kill_by_id_arms_when_the_abort_is_stored_after_the_lookup().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_kill_step_arms_when_the_abort_is_stored_after_the_lookup_multi_thread() {
        shutdown_kill_step_arms_when_the_abort_is_stored_after_the_lookup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_kill_step_arms_when_the_abort_is_stored_after_the_lookup_current_thread() {
        shutdown_kill_step_arms_when_the_abort_is_stored_after_the_lookup().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_sweep_arms_when_the_abort_is_stored_after_the_lookup_multi_thread() {
        force_sweep_arms_when_the_abort_is_stored_after_the_lookup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn force_sweep_arms_when_the_abort_is_stored_after_the_lookup_current_thread() {
        force_sweep_arms_when_the_abort_is_stored_after_the_lookup().await;
    }

    // - A Kill arms the incarnation it was raised on, and only it -------------

    /// Registers an incarnation under `id` (and `name`): a live lane and
    /// status plane owned by a pending task that also holds the registration
    /// guard, so the task's end unregisters it. Its abort handle is stored.
    fn register_incarnation(
        sys: &Arc<ActorSystem>,
        id: &ActorId,
        name: Option<&str>,
        grace: Duration,
    ) -> (u64, tokio::task::JoinHandle<()>) {
        let (tx, _rx) = mpsc::channel(1);
        let (system_tx, _system_rx) = mpsc::channel(1);
        let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, stop_rx) = StopLane::new();
        let handle = ActorHandle::<Dummy>::new(
            id.clone(),
            tx,
            system_tx,
            stop_lane,
            1,
            status_rx,
            sys.name().into(),
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let seq = sys
            .register_actor::<Dummy>(
                id,
                name,
                &handle,
                true,
                TurnClock::default(),
                grace,
                Handle::current(),
            )
            .unwrap();
        let guard = RegistryGuard::new(sys.clone(), id.clone(), name.map(str::to_string), seq);
        let task = tokio::spawn(async move {
            let _owned = (status_tx, stop_rx, guard);
            std::future::pending::<()>().await;
        });
        sys.attach_abort(id, name, task.abort_handle(), seq);
        (seq, task)
    }

    #[derive(Clone, Copy, Debug)]
    enum KillSite {
        ByName,
        ById,
        ShutdownKillStep,
        ForceSweep,
    }

    /// Between a kill's raise and its re-read of the registry, the incarnation
    /// it raised on is replaced by a successor under the same id and name:
    /// the successor's abort is never armed.
    async fn a_kill_arms_only_the_incarnation_it_was_raised_on(site: KillSite) {
        let sys = ActorSystem::create(format!("successor-{}", uuid::Uuid::new_v4())).unwrap();
        let name = format!("successor-target-{}", uuid::Uuid::new_v4());
        let id = ActorId::from(name.clone());
        let (seq, old) = register_incarnation(&sys, &id, Some(&name), SLOW_GRACE);
        let successor: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
        {
            let (hook_sys, hook_id, hook_name, slot) =
                (sys.clone(), id.clone(), name.clone(), successor.clone());
            *sys.after_kill_raise.lock().unwrap() = Some(Arc::new(move || {
                hook_sys.unregister_by_id(&hook_id, seq);
                hook_sys.unregister_by_name(&hook_name, seq);
                // A shutdown refuses registrations; the successor stands for
                // the next incarnation of the same id.
                let phase = hook_sys
                    .phase
                    .swap(SystemPhase::Active.as_u8(), Ordering::SeqCst);
                let (_, task) =
                    register_incarnation(&hook_sys, &hook_id, Some(&hook_name), HOOK_GRACE);
                hook_sys.phase.store(phase, Ordering::SeqCst);
                *slot.lock().unwrap() = Some(task);
            }));
        }
        match site {
            KillSite::ByName => {
                sys.kill(&name).await.unwrap();
            }
            KillSite::ById => {
                let _ = tokio::time::timeout(Duration::ZERO, sys.kill_by_id(&id)).await;
            }
            KillSite::ShutdownKillStep => {
                let policy = ShutdownPolicy {
                    timeout: Duration::from_secs(30),
                    per_actor_timeout: Duration::from_millis(10),
                };
                let _ = tokio::time::timeout(Duration::from_millis(60), sys.shutdown_with(policy))
                    .await;
            }
            KillSite::ForceSweep => {
                let policy = ShutdownPolicy {
                    timeout: Duration::from_millis(10),
                    per_actor_timeout: Duration::from_secs(5),
                };
                let _ = tokio::time::timeout(Duration::from_millis(60), sys.shutdown_with(policy))
                    .await;
            }
        }
        let successor = successor
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| panic!("{site:?}: the incarnation was never replaced"));
        // Well past the successor's grace: nothing aborts it, because the
        // Kill was raised on the incarnation before it.
        tokio::time::sleep(HOOK_GRACE * 4).await;
        assert!(
            !successor.is_finished(),
            "{site:?}: the Kill armed the abort of the successor incarnation"
        );
        successor.abort();
        old.abort();
    }

    macro_rules! successor_tests {
        ($($site:ident => $multi:ident, $single:ident;)*) => {$(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $multi() {
                a_kill_arms_only_the_incarnation_it_was_raised_on(KillSite::$site).await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn $single() {
                a_kill_arms_only_the_incarnation_it_was_raised_on(KillSite::$site).await;
            }
        )*};
    }

    successor_tests! {
        ByName => kill_by_name_arms_only_the_incarnation_it_was_raised_on_multi_thread,
            kill_by_name_arms_only_the_incarnation_it_was_raised_on_current_thread;
        ById => kill_by_id_arms_only_the_incarnation_it_was_raised_on_multi_thread,
            kill_by_id_arms_only_the_incarnation_it_was_raised_on_current_thread;
        ShutdownKillStep => shutdown_kill_step_arms_only_the_incarnation_it_was_raised_on_multi_thread,
            shutdown_kill_step_arms_only_the_incarnation_it_was_raised_on_current_thread;
        ForceSweep => force_sweep_arms_only_the_incarnation_it_was_raised_on_multi_thread,
            force_sweep_arms_only_the_incarnation_it_was_raised_on_current_thread;
    }

    // - The force sweep reports a root gone before it looked ------------------

    /// Two roots; the sequential ladder is cut at the first (global timeout
    /// spent) so both reach the sweep. The second was aborted by another
    /// caller's Kill and left the registry before the sweep looked: its
    /// verdict comes from the status receiver its snapshot kept, as the
    /// sequential ladder's would.
    async fn the_sweep_classifies_a_root_gone_before_it_looked() {
        let sys = ActorSystem::create(format!("sweep-gone-{}", uuid::Uuid::new_v4())).unwrap();
        let (aborted_id, parked_id) = (
            ActorId::from("sweep-aborted"),
            ActorId::from("sweep-parked"),
        );
        // Registered first, so it is stopped last and swept as a straggler.
        let (_seq, aborted_task) = register_incarnation(
            &sys,
            &aborted_id,
            Some("sweep-aborted"),
            Duration::from_millis(40),
        );
        let (_seq, parked_task) = register_incarnation(
            &sys,
            &parked_id,
            Some("sweep-parked"),
            Duration::from_millis(100),
        );
        let shutdown = {
            let sys = sys.clone();
            tokio::spawn(async move {
                sys.shutdown_with(ShutdownPolicy {
                    timeout: Duration::from_millis(400),
                    per_actor_timeout: Duration::from_secs(5),
                })
                .await
            })
        };
        // Another caller kills the first-registered root while the ladder is
        // still waiting on the second.
        tokio::time::sleep(Duration::from_millis(20)).await;
        sys.kill("sweep-aborted").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), aborted_task)
            .await
            .expect("the kill's backstop aborts the root")
            .unwrap_err();
        assert!(sys.get_by_id::<Dummy>(&aborted_id).is_none(), "gone");
        let report = tokio::time::timeout(Duration::from_secs(10), shutdown)
            .await
            .expect("the shutdown returns")
            .unwrap();
        let outcome = |id: &ActorId| {
            report
                .outcomes
                .iter()
                .find(|(reported, _)| reported == id)
                .map(|(_, report)| report.outcome)
        };
        assert_eq!(
            outcome(&aborted_id),
            Some(StopOutcome::Aborted),
            "a root aborted before the sweep looked is Aborted, as in the sequential ladder"
        );
        assert!(outcome(&parked_id).is_some());
        parked_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sweep_classifies_a_root_gone_before_it_looked_multi_thread() {
        the_sweep_classifies_a_root_gone_before_it_looked().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_sweep_classifies_a_root_gone_before_it_looked_current_thread() {
        the_sweep_classifies_a_root_gone_before_it_looked().await;
    }

    /// A `Kill` raised in the window before the task's abort handle is stored
    /// finds no abort to schedule: the attach itself arms the backstop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attach_abort_arms_the_backstop_when_the_lane_already_holds_a_kill() {
        let sys = ActorSystem::create(format!("pre-attach-{}", uuid::Uuid::new_v4())).unwrap();
        let id = ActorId::from("pre-attach-id");
        let (tx, _rx) = mpsc::channel(1);
        let (system_tx, _system_rx) = mpsc::channel(1);
        let (_status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, _stop_rx) = StopLane::new();
        let handle = ActorHandle::<Dummy>::new(
            id.clone(),
            tx,
            system_tx,
            stop_lane.clone(),
            1,
            status_rx,
            sys.name().into(),
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let grace = Duration::from_millis(100);
        let seq = sys
            .register_actor::<Dummy>(
                &id,
                Some("pre-attach-name"),
                &handle,
                true,
                TurnClock::default(),
                grace,
                Handle::current(),
            )
            .unwrap();
        // The kill lands before the abort handle exists.
        stop_lane.raise(StopReason::Kill);
        let task = tokio::spawn(std::future::pending::<()>());
        sys.attach_abort(&id, Some("pre-attach-name"), task.abort_handle(), seq);
        let ended = tokio::time::timeout(grace + ABORT_SETTLE + Duration::from_millis(500), task)
            .await
            .expect("the attach armed the backstop, which aborted the task");
        assert!(ended.unwrap_err().is_cancelled());
    }

    /// An attach that comes after its incarnation's registration was
    /// replaced (the name and the id freed, and a later spawn registered
    /// them) leaves the new entries' abort handle as it is, and arms no
    /// backstop through them, even when the new entry's lane holds a `Kill`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_attach_leaves_the_new_registration_s_abort_its_own() {
        let sys = ActorSystem::create(format!("stale-attach-{}", uuid::Uuid::new_v4())).unwrap();
        let id = ActorId::from("stale-attach-id");
        let name = "stale-attach-name";
        let register = |sys: &Arc<ActorSystem>| {
            let (tx, _rx) = mpsc::channel(1);
            let (system_tx, _system_rx) = mpsc::channel(1);
            let (_status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
            let (stop_lane, _stop_rx) = StopLane::new();
            let handle = ActorHandle::<Dummy>::new(
                id.clone(),
                tx,
                system_tx,
                stop_lane.clone(),
                1,
                status_rx,
                sys.name().into(),
                None,
                Arc::new(AtomicBool::new(false)),
            );
            let seq = sys
                .register_actor::<Dummy>(
                    &id,
                    Some(name),
                    &handle,
                    true,
                    TurnClock::default(),
                    Duration::from_secs(60),
                    Handle::current(),
                )
                .unwrap();
            (seq, stop_lane)
        };
        // The first incarnation registers, and its entries are freed before
        // its attach runs.
        let (old_seq, _old_lane) = register(&sys);
        sys.unregister_by_id(&id, old_seq);
        sys.unregister_by_name(name, old_seq);
        // A later spawn registers the same id and name and attaches its own
        // abort; its lane then holds a `Kill`.
        let (new_seq, new_lane) = register(&sys);
        let new_task = tokio::spawn(std::future::pending::<()>());
        sys.attach_abort(&id, Some(name), new_task.abort_handle(), new_seq);
        new_lane.raise(StopReason::Kill);
        let arms = sys.backstop_arms.load(Ordering::SeqCst);
        let old_task = tokio::spawn(std::future::pending::<()>());
        sys.attach_abort(&id, Some(name), old_task.abort_handle(), old_seq);
        let by_id = sys
            .by_id
            .get(&id)
            .and_then(|entry| entry.abort.as_ref().map(|a| a.id()));
        let by_name = sys
            .by_name
            .get(name)
            .and_then(|entry| entry.abort.as_ref().map(|a| a.id()));
        assert_eq!(
            by_id,
            Some(new_task.id()),
            "the id's entry keeps the new abort"
        );
        assert_eq!(
            by_name,
            Some(new_task.id()),
            "the name's entry keeps the new abort"
        );
        assert_eq!(
            sys.backstop_arms.load(Ordering::SeqCst),
            arms,
            "the stale attach armed no backstop"
        );
        new_task.abort();
        old_task.abort();
    }

    /// Supervises one child that parks in `handle`, stopped under
    /// `Shutdown::Kill` on command.
    struct Boss {
        child: &'static str,
        parked: Arc<AtomicBool>,
    }

    impl Actor for Boss {
        type Message = ();
        type Response = ();

        async fn on_started(
            &mut self,
            ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            let parked = self.parked.clone();
            ctx.spawn_child(move || Parker {
                parked: parked.clone(),
            })
            .named(self.child)
            .shutdown(crate::types::Shutdown::Kill)
            .await?;
            Ok(())
        }

        async fn handle(
            &mut self,
            _msg: (),
            ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            ctx.terminate_child(self.child).await?;
            Ok(())
        }
    }

    /// Supervises one parked child and parks itself in `handle` on command,
    /// so a `Kill` raised on it is held by its lane and never observed: its
    /// registry entry outlives every lookup a kill path makes.
    struct ParkingBoss {
        parked: Arc<AtomicBool>,
        child_parked: Arc<AtomicBool>,
    }

    impl Actor for ParkingBoss {
        type Message = ();
        type Response = ();

        async fn on_started(
            &mut self,
            ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            let parked = self.child_parked.clone();
            ctx.spawn_child(move || Parker {
                parked: parked.clone(),
            })
            .named("guard-child")
            .await?;
            Ok(())
        }

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            self.parked.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    /// A supervised child that crashes on command; every incarnation after
    /// the first hangs in its initialization, so a restart attempt's start
    /// timeout kills it.
    struct HangsOnRestart {
        builds: Arc<AtomicU64>,
    }

    impl Actor for HangsOnRestart {
        type Message = ();
        type Response = ();

        async fn on_started(
            &mut self,
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            if self.builds.load(Ordering::SeqCst) > 1 {
                std::future::pending::<()>().await;
            }
            Ok(())
        }

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Err(crate::error::ActorError::user("crash on command"))
        }
    }

    struct RestartBoss {
        builds: Arc<AtomicU64>,
    }

    impl Actor for RestartBoss {
        type Message = ();
        type Response = ();

        async fn on_started(
            &mut self,
            ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            let builds = self.builds.clone();
            ctx.spawn_child(move || {
                builds.fetch_add(1, Ordering::SeqCst);
                HangsOnRestart {
                    builds: builds.clone(),
                }
            })
            .named("restart-child")
            .start_timeout(Duration::from_millis(50))
            .await?;
            Ok(())
        }

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    /// Counts the entries every kill path hands to this runtime's reaper.
    fn counting_reaper(sys: &ActorSystem) -> mpsc::UnboundedReceiver<ReaperEntry> {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(std::future::pending::<()>()).abort_handle();
        sys.reaper
            .lock()
            .unwrap()
            .insert(Handle::current().id(), ReaperSlot { tx, task });
        rx
    }

    async fn until(what: &str, mut ready: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    fn drained(entries: &mut mpsc::UnboundedReceiver<ReaperEntry>) -> usize {
        let mut n = 0;
        while entries.try_recv().is_ok() {
            n += 1;
        }
        n
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_by_id_arms_exactly_one_backstop_entry_and_the_supervision_raisers_arm_none() {
        backstop_entries_by_kill_path().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn kill_by_id_arms_exactly_one_backstop_entry_and_the_supervision_raisers_arm_none_current_thread(
    ) {
        backstop_entries_by_kill_path().await;
    }

    async fn backstop_entries_by_kill_path() {
        use crate::actor::ActorExt;

        let sys = ActorSystem::create(format!("reaper-count-{}", uuid::Uuid::new_v4())).unwrap();
        let mut entries = counting_reaper(&sys);
        let arms = || sys.backstop_arms.load(Ordering::SeqCst);

        // One `kill_by_id` of a parked root arms exactly once and queues
        // exactly one entry.
        let (parker, parked) = Parker::new();
        let root = parker
            .spawn()
            .named("count-root")
            .on_system(&sys)
            .await
            .unwrap();
        root.notify(()).await.unwrap();
        until("the root to park", || parked.load(Ordering::SeqCst)).await;
        assert!(sys.kill_by_id(root.id()).await.is_some());
        assert_eq!(arms(), 1, "one kill, one arming");
        assert_eq!(drained(&mut entries), 1, "one kill, one backstop entry");

        // The supervisor's own stop of a child under `Shutdown::Kill` aborts
        // through the runner's ladder, which arms nothing.
        let parked = Arc::new(AtomicBool::new(false));
        let boss = Boss {
            child: "ladder-child",
            parked: parked.clone(),
        }
        .spawn()
        .named("count-boss")
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        let child = loop {
            if let Some(child) = sys.get::<Parker>("ladder-child") {
                break child;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        child.notify(()).await.unwrap();
        until("the child to park", || parked.load(Ordering::SeqCst)).await;
        boss.notify(()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), child.wait_stopped())
            .await
            .expect("the child is stopped by its supervisor's ladder");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(arms(), 1, "the runner's ladder arms nothing");
        assert_eq!(drained(&mut entries), 0);

        // A killed supervisor arms its own backstop once (its handle's
        // `stop(Kill)`); its child's link guard schedules an entry of its own
        // and arms nothing through the system. The supervisor is parked in a
        // turn, so the `Kill` is held by its lane and the lookup `stop(Kill)`
        // makes always finds it registered: an idle one can observe the
        // `Kill` and leave the registry before that lookup, and a target
        // already out of the registry is past its last await.
        let parked = Arc::new(AtomicBool::new(false));
        let child_parked = Arc::new(AtomicBool::new(false));
        let boss = ParkingBoss {
            parked: parked.clone(),
            child_parked: child_parked.clone(),
        }
        .spawn()
        .named("count-guard-boss")
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        let child = loop {
            if let Some(child) = sys.get::<Parker>("guard-child") {
                break child;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        child.notify(()).await.unwrap();
        until("the child to park", || child_parked.load(Ordering::SeqCst)).await;
        boss.notify(()).await.unwrap();
        until("the supervisor to park", || parked.load(Ordering::SeqCst)).await;
        boss.stop(crate::types::StopReason::Kill).await.unwrap();
        assert_eq!(arms(), 2, "the supervisor's stop(Kill) arms once");
        // The reaper of this runtime is the counting one. The supervisor's
        // entry is queued by the time `stop` returns; dropping it aborts the
        // parked supervisor, whose dropped death plane fires the child's link
        // guard, which queues the second entry.
        let supervisor_entry = entries
            .try_recv()
            .expect("the supervisor's entry is queued");
        assert_eq!(drained(&mut entries), 0, "one entry per arming");
        drop(supervisor_entry);
        let guard_entry = tokio::time::timeout(Duration::from_secs(5), entries.recv())
            .await
            .expect("the guard's entry is scheduled");
        assert!(guard_entry.is_some());
        let _ = &child;
        assert_eq!(arms(), 2, "the guard arms nothing through the system");
        assert_eq!(drained(&mut entries), 0, "and nothing else was queued");

        // A restart attempt's start-timeout Kill arms nothing either.
        let builds = Arc::new(AtomicU64::new(0));
        let _restarter = RestartBoss {
            builds: builds.clone(),
        }
        .spawn()
        .named("count-restart-boss")
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        until("the child to be built", || {
            builds.load(Ordering::SeqCst) == 1
        })
        .await;
        let restart_child = loop {
            if let Some(child) = sys.get::<HangsOnRestart>("restart-child") {
                break child;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        restart_child.notify(()).await.unwrap();
        until("a restart attempt to be built", || {
            builds.load(Ordering::SeqCst) >= 2
        })
        .await;
        // The first attempt's incarnation hangs past its start timeout and is
        // killed, and the supervisor builds the next one: the Kill this case
        // counts has happened.
        until("the hung attempt to be killed and replaced", || {
            builds.load(Ordering::SeqCst) >= 3
        })
        .await;
        assert_eq!(
            arms(),
            2,
            "a restart attempt arms nothing through the system"
        );

        // The reaper's own Kill stage raises the Kill and queues nothing: its
        // entry already holds the abort.
        let (staged, lane, lane_rx) = parked_with_a_lane();
        let entry = ReaperEntry::two_stage(
            Instant::now(),
            lane,
            Instant::now() + Duration::from_secs(60),
            staged.abort_handle(),
        );
        drained(&mut entries);
        let rest = entry.fire().expect("the Kill stage hands its entry back");
        assert!(matches!(
            lane_rx.borrow().reason,
            Some(crate::types::StopReason::Kill)
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            entries.try_recv().is_err(),
            "the reaper's Kill stage queues no second entry"
        );
        assert_eq!(arms(), 2, "and arms nothing through the system");
        drop(rest);
    }

    // - Roots whose end a test drives step by step ----------------------------

    /// A root incarnation whose end a test drives in the runtime's order:
    /// `leave` frees its registry entry, and only afterwards does
    /// `publish_stopped` write `Stopped` and close its status plane. Its lane
    /// receiver belongs to a parked task whose abort handle is the entry's,
    /// so a raise on its lane is recorded and an abort of it is observable.
    struct Scripted {
        id: ActorId,
        handle: ActorHandle<Dummy>,
        lane: StopLane,
        guard: Mutex<Option<RegistryGuard>>,
        status_tx: Mutex<Option<watch::Sender<StatusValue>>>,
        task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    }

    impl Scripted {
        /// Registers a root incarnation under `id` (also while a shutdown
        /// refuses registrations, so a test can stand a later incarnation of
        /// the same id in for the one a shutdown snapshotted).
        fn register(sys: &Arc<ActorSystem>, id: &ActorId, grace: Duration) -> Arc<Scripted> {
            Scripted::register_as(sys, id, grace, true)
        }

        /// As `register`, as a root or as a non-root (a supervised child's
        /// registration).
        fn register_as(
            sys: &Arc<ActorSystem>,
            id: &ActorId,
            grace: Duration,
            is_root: bool,
        ) -> Arc<Scripted> {
            let (tx, _rx) = mpsc::channel(1);
            let (system_tx, _system_rx) = mpsc::channel(1);
            let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
            let (stop_lane, stop_rx) = StopLane::new();
            let handle = ActorHandle::<Dummy>::new(
                id.clone(),
                tx,
                system_tx,
                stop_lane.clone(),
                1,
                status_rx,
                sys.name().into(),
                None,
                Arc::new(AtomicBool::new(false)),
            );
            let phase = sys
                .phase
                .swap(SystemPhase::Active.as_u8(), Ordering::SeqCst);
            let seq = sys
                .register_actor::<Dummy>(
                    id,
                    None,
                    &handle,
                    is_root,
                    TurnClock::default(),
                    grace,
                    Handle::current(),
                )
                .unwrap();
            sys.phase.store(phase, Ordering::SeqCst);
            let guard = RegistryGuard::new(sys.clone(), id.clone(), None, seq);
            let task = tokio::spawn(async move {
                let _owned = stop_rx;
                std::future::pending::<()>().await;
            });
            sys.attach_abort(id, None, task.abort_handle(), seq);
            Arc::new(Scripted {
                id: id.clone(),
                handle,
                lane: stop_lane,
                guard: Mutex::new(Some(guard)),
                status_tx: Mutex::new(Some(status_tx)),
                task: Mutex::new(Some(task)),
            })
        }

        /// This incarnation leaves the registry and a later one is
        /// registered under the same id.
        fn replace(&self, sys: &Arc<ActorSystem>, grace: Duration) -> Arc<Scripted> {
            self.leave();
            Scripted::register(sys, &self.id, grace)
        }

        fn leave(&self) {
            drop(self.guard.lock().unwrap().take());
        }

        fn publish_stopped(&self) {
            if let Some(tx) = self.status_tx.lock().unwrap().take() {
                StatusValue::write_reason(&tx, crate::types::StopReason::Graceful, true);
            }
        }

        /// Ends as the runtime does: leaves the registry, seals the lane,
        /// then publishes `Stopped`.
        fn end(&self) {
            self.leave();
            let _ = self.lane.seal();
            self.publish_stopped();
        }

        fn task_ended(&self) -> bool {
            self.task
                .lock()
                .unwrap()
                .as_ref()
                .is_none_or(|task| task.is_finished())
        }

        fn task(&self) -> tokio::task::JoinHandle<()> {
            self.task
                .lock()
                .unwrap()
                .take()
                .expect("the task is taken once")
        }
    }

    fn set_ladder_hook(
        sys: &ActorSystem,
        hook: impl Fn(LadderPoint, &ActorId) + Send + Sync + 'static,
    ) {
        *sys.ladder_hook.lock().unwrap() = Some(Arc::new(hook));
    }

    fn scripted_system(what: &str) -> Arc<ActorSystem> {
        ActorSystem::create(format!("{what}-{}", uuid::Uuid::new_v4())).unwrap()
    }

    async fn shut_down(
        sys: &Arc<ActorSystem>,
        timeout: Duration,
        per_actor: Duration,
    ) -> ShutdownReport {
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            sys.shutdown_with(ShutdownPolicy {
                timeout,
                per_actor_timeout: per_actor,
            }),
        )
        .await
        .expect("the shutdown returns");
        // The hooks hold the system; clearing them ends the cycle.
        *sys.ladder_hook.lock().unwrap() = None;
        report
    }

    fn outcome_of(report: &ShutdownReport, id: &ActorId) -> StopOutcome {
        report
            .outcomes
            .iter()
            .find(|(reported, _)| reported == id)
            .map(|(_, report)| report.outcome)
            .unwrap_or_else(|| panic!("{id} is reported: {report:?}"))
    }

    /// A global timeout that cuts the ladder in its first step: every root
    /// still waiting goes to the force sweep, unasked for `Kill`.
    const CUT_IN_STEP_ONE: (Duration, Duration) =
        (Duration::from_millis(100), Duration::from_secs(30));

    /// A step 1 that ends at once and a global timeout that then cuts the
    /// ladder in its `Kill` step (the root's grace being longer).
    const CUT_IN_STEP_TWO: (Duration, Duration) =
        (Duration::from_millis(1000), Duration::from_millis(1));

    /// A grace no test here waits out.
    const LONG_GRACE: Duration = Duration::from_secs(30);

    /// Two roots cut at the first one's step 1. The first left the registry
    /// before the sweep looked and had not published `Stopped`; it publishes
    /// it only once the sweep has moved on to the second root. The sweep
    /// waits for it and reports what it reached by itself.
    async fn the_sweep_waits_for_a_root_gone_before_it_published_stopped() {
        let sys = scripted_system("sweep-window");
        let second = Scripted::register(&sys, &ActorId::from("window-second"), LONG_GRACE);
        // Registered last, so the ladder reaches it first and is cut there.
        // Its grace is short of the test's bounds: a sweep that waited for it
        // before looking at the next root would give it `Unresponsive`.
        let first = Scripted::register(
            &sys,
            &ActorId::from("window-first"),
            Duration::from_millis(600),
        );
        {
            let (first, second) = (first.clone(), second.clone());
            set_ladder_hook(&sys, move |point, id| {
                if point != LadderPoint::SweepLookup {
                    return;
                }
                if *id == first.id {
                    first.leave();
                } else if *id == second.id {
                    first.publish_stopped();
                    second.end();
                }
            });
        }
        let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
        assert_eq!(
            outcome_of(&report, &first.id),
            StopOutcome::Graceful,
            "a root that left the registry and then published Stopped itself"
        );
        assert_eq!(outcome_of(&report, &second.id), StopOutcome::Graceful);
        first.task().abort();
        second.task().abort();
    }

    /// The same window, held past the bound: a root that left the registry
    /// and has not published `Stopped` within its kill grace and the wait
    /// after an abort is `Unresponsive`, as on every other path.
    async fn a_root_gone_and_not_stopped_within_its_bound_is_unresponsive() {
        let sys = scripted_system("sweep-window-held");
        // Longer than the default grace, so the bound read is the root's own.
        let grace = Duration::from_millis(600);
        let second = Scripted::register(&sys, &ActorId::from("held-second"), LONG_GRACE);
        let first = Scripted::register(&sys, &ActorId::from("held-first"), grace);
        {
            let (first, second) = (first.clone(), second.clone());
            set_ladder_hook(&sys, move |point, id| {
                if point != LadderPoint::SweepLookup {
                    return;
                }
                if *id == first.id {
                    first.leave();
                } else if *id == second.id {
                    second.end();
                }
            });
        }
        let started = Instant::now();
        let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
        assert_eq!(outcome_of(&report, &first.id), StopOutcome::Unresponsive);
        assert!(
            started.elapsed() >= CUT_IN_STEP_ONE.0 + grace + ABORT_SETTLE,
            "the sweep waited the root's grace and the wait after an abort"
        );
        first.publish_stopped();
        first.task().abort();
        second.task().abort();
    }

    /// The ladder is cut in step 2 after its `Kill` step found the root
    /// already gone from the registry: it raised nothing, so the root,
    /// publishing `Stopped` itself, is `Graceful`.
    async fn a_root_gone_at_the_kill_step_reports_no_raised_kill() {
        let sys = scripted_system("gone-at-kill");
        let root = Scripted::register(&sys, &ActorId::from("gone-at-kill"), LONG_GRACE);
        let reached_kill = Arc::new(AtomicBool::new(false));
        {
            let (root, reached_kill) = (root.clone(), reached_kill.clone());
            set_ladder_hook(&sys, move |point, _id| match point {
                LadderPoint::Kill => {
                    reached_kill.store(true, Ordering::SeqCst);
                    root.leave();
                }
                LadderPoint::SweepLookup => root.end(),
                _ => {}
            });
        }
        let report = shut_down(&sys, CUT_IN_STEP_TWO.0, CUT_IN_STEP_TWO.1).await;
        assert!(
            reached_kill.load(Ordering::SeqCst),
            "the ladder reached step 2"
        );
        assert!(
            matches!(root.lane.reason(), Some(StopReason::ParentRequest)),
            "no Kill was raised"
        );
        assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
        root.task().abort();
    }

    /// Two non-root leftovers leave the registry when the final sweep's force
    /// looks them up, after the global deadline: one publishes `Stopped` with
    /// its reason, the other's status sender closes without it. Each is
    /// reported from its own plane, with the reason it holds: the first as a
    /// kill, since its end became final after the deadline, the second as
    /// aborted.
    async fn a_leftover_gone_at_the_sweep_lookup_reports_the_reason_its_plane_holds() {
        let sys = scripted_system("leftover-gone");
        let stopped = Scripted::register_as(&sys, &ActorId::from("stopped"), LONG_GRACE, false);
        let closed = Scripted::register_as(&sys, &ActorId::from("closed"), LONG_GRACE, false);
        {
            let (stopped, closed) = (stopped.clone(), closed.clone());
            set_ladder_hook(&sys, move |point, id| {
                if point != LadderPoint::SweepLookup {
                    return;
                }
                if *id == stopped.id {
                    stopped.end();
                } else if *id == closed.id {
                    closed.leave();
                    drop(closed.status_tx.lock().unwrap().take());
                }
            });
        }
        let report = shut_down(&sys, Duration::from_millis(300), Duration::from_secs(5)).await;
        let swept = |id: &ActorId| {
            report
                .swept
                .iter()
                .find(|(reported, _)| reported == id)
                .map(|(_, report)| report.clone())
                .unwrap_or_else(|| panic!("{id} is swept: {report:?}"))
        };
        assert_eq!(
            swept(&stopped.id),
            StopReport {
                outcome: StopOutcome::Killed,
                reason: Some(StopReason::Graceful),
            }
        );
        assert_eq!(
            swept(&closed.id),
            StopReport {
                outcome: StopOutcome::Aborted,
                reason: Some(StopReason::Kill),
            }
        );
        stopped.task().abort();
        closed.task().abort();
    }

    /// What a shutdown did to a later incarnation `S` standing in for a root
    /// under the same id: `S` is registered at `swap_at` in place of the
    /// snapshotted root (which then publishes `Stopped` when
    /// `root_publishes`). When the shutdown takes `S` on as a late root,
    /// before its own first ladder step, its lane, whether its task had ended
    /// and the system's arm count are recorded, and `S` ends there.
    struct SuccessorWatch {
        successor: Mutex<Option<Arc<Scripted>>>,
        before_its_ladder: Mutex<Option<(Option<StopReason>, bool, u64)>>,
    }

    impl SuccessorWatch {
        fn successor(&self) -> Arc<Scripted> {
            self.successor
                .lock()
                .unwrap()
                .clone()
                .expect("the successor was registered")
        }

        fn before_its_ladder(&self) -> (Option<StopReason>, bool, u64) {
            self.before_its_ladder
                .lock()
                .unwrap()
                .clone()
                .expect("the shutdown took the successor on as a late root")
        }
    }

    fn watch_successor(
        sys: &Arc<ActorSystem>,
        root: &Arc<Scripted>,
        swap_at: LadderPoint,
        root_publishes: bool,
    ) -> Arc<SuccessorWatch> {
        let watch = Arc::new(SuccessorWatch {
            successor: Mutex::new(None),
            before_its_ladder: Mutex::new(None),
        });
        let (hook_sys, root, hook_watch) = (sys.clone(), root.clone(), watch.clone());
        set_ladder_hook(sys, move |point, _id| {
            let successor = hook_watch.successor.lock().unwrap().clone();
            match successor {
                None if point == swap_at => {
                    let successor = root.replace(&hook_sys, HOOK_GRACE);
                    *hook_watch.successor.lock().unwrap() = Some(successor);
                    if root_publishes {
                        root.publish_stopped();
                    }
                }
                Some(successor)
                    if point == LadderPoint::Stop
                        && hook_watch.before_its_ladder.lock().unwrap().is_none() =>
                {
                    *hook_watch.before_its_ladder.lock().unwrap() = Some((
                        successor.lane.reason(),
                        successor.task_ended(),
                        hook_sys.backstop_arms.load(Ordering::SeqCst),
                    ));
                    successor.end();
                }
                _ => {}
            }
        });
        watch
    }

    /// Both incarnations under the id are reported, the snapshotted one first
    /// with `first`, the successor `Graceful` (it ended at its first step).
    fn reported_twice(report: &ShutdownReport, id: &ActorId, first: StopOutcome) {
        let under_id: Vec<StopOutcome> = report
            .outcomes
            .iter()
            .filter(|(reported, _)| reported == id)
            .map(|(_, report)| report.outcome)
            .collect();
        assert_eq!(under_id, vec![first, StopOutcome::Graceful], "{report:?}");
    }

    /// Before the ladder's `Kill` step, the incarnation it is stopping is
    /// replaced by a later one under the same id, and never publishes
    /// `Stopped`. The `Kill` step neither raises on nor arms the successor,
    /// the replaced incarnation is `Unresponsive`, and the shutdown then
    /// stops the successor as a late root, reported after it.
    async fn a_successor_registered_before_the_kill_step_is_never_raised_on() {
        let sys = scripted_system("successor-at-kill");
        let grace = Duration::from_millis(20);
        let root = Scripted::register(&sys, &ActorId::from("successor-at-kill"), grace);
        let watch = watch_successor(&sys, &root, LadderPoint::Kill, false);
        let report = shut_down(&sys, Duration::from_secs(30), Duration::from_millis(1)).await;
        let (lane, ended, arms) = watch.before_its_ladder();
        assert!(lane.is_none(), "nothing was raised on the successor");
        assert!(!ended);
        assert_eq!(arms, 0, "nothing was armed");
        reported_twice(&report, &root.id, StopOutcome::Unresponsive);
        watch.successor().task().abort();
        root.publish_stopped();
        root.task().abort();
    }

    /// Before the ladder begins, the incarnation it snapshotted is replaced
    /// by a later one under the same id: no step of that ladder stops, kills
    /// or aborts the successor, the snapshotted incarnation, which never
    /// publishes `Stopped`, is `Unresponsive`, and the successor is stopped
    /// as a late root.
    async fn the_ladder_never_acts_on_a_successor_registered_before_it_began() {
        let sys = scripted_system("successor-at-stop");
        let grace = Duration::from_millis(20);
        let root = Scripted::register(&sys, &ActorId::from("successor-at-stop"), grace);
        let watch = watch_successor(&sys, &root, LadderPoint::Stop, false);
        let report = shut_down(&sys, Duration::from_secs(30), grace).await;
        let (lane, ended, arms) = watch.before_its_ladder();
        assert!(lane.is_none(), "nothing was raised on the successor");
        assert!(!ended, "the successor was not aborted");
        assert_eq!(arms, 0, "nothing was armed");
        reported_twice(&report, &root.id, StopOutcome::Unresponsive);
        watch.successor().task().abort();
        root.publish_stopped();
        root.task().abort();
    }

    /// Before the ladder's abort step, the incarnation it raised `Kill` on is
    /// replaced by a later one under the same id: the abort step does not
    /// abort the successor, and the successor is stopped as a late root.
    async fn the_abort_step_never_aborts_a_successor_registered_before_it() {
        let sys = scripted_system("successor-at-abort");
        let grace = Duration::from_millis(20);
        let root = Scripted::register(&sys, &ActorId::from("successor-at-abort"), grace);
        let watch = watch_successor(&sys, &root, LadderPoint::Abort, false);
        let report = shut_down(&sys, Duration::from_secs(30), grace).await;
        assert!(matches!(root.lane.reason(), Some(StopReason::Kill)));
        let (lane, ended, _) = watch.before_its_ladder();
        assert!(lane.is_none(), "nothing was raised on the successor");
        assert!(!ended, "the successor was not aborted");
        reported_twice(&report, &root.id, StopOutcome::Unresponsive);
        watch.successor().task().abort();
        root.publish_stopped();
    }

    /// The ladder is cut in step 1; before the sweep looks, the root is
    /// replaced by a later incarnation under the same id and never publishes
    /// `Stopped`. The sweep reports the snapshotted root `Unresponsive` after
    /// its grace and never kills the successor, and the shutdown then stops
    /// the successor as a late root, reported after it.
    async fn the_sweep_never_kills_a_successor_registered_before_it_looked() {
        let sys = scripted_system("successor-at-sweep");
        let grace = Duration::from_millis(20);
        let root = Scripted::register(&sys, &ActorId::from("successor-at-sweep"), grace);
        let watch = watch_successor(&sys, &root, LadderPoint::SweepLookup, false);
        let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
        let (lane, ended, arms) = watch.before_its_ladder();
        assert!(lane.is_none(), "nothing was raised on the successor");
        assert!(!ended);
        assert_eq!(arms, 0, "nothing was armed");
        reported_twice(&report, &root.id, StopOutcome::Unresponsive);
        watch.successor().task().abort();
        root.publish_stopped();
        root.task().abort();
    }

    /// The ladder's `Kill` step finds the root, but between its lookup and
    /// its raise the root leaves, seals its lane and publishes `Stopped`: the
    /// lane refuses the `Kill`, nothing is armed, and the root, which ended
    /// with its own reason, is `Graceful`.
    async fn a_root_that_refuses_the_kill_step_keeps_its_own_outcome() {
        let sys = scripted_system("kill-step-refused");
        let root = Scripted::register(&sys, &ActorId::from("kill-step-refused"), LONG_GRACE);
        {
            let root = root.clone();
            *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
                root.leave();
                let _ = root.lane.seal();
                root.publish_stopped();
            }));
        }
        let report = shut_down(&sys, Duration::from_secs(30), Duration::from_millis(1)).await;
        assert!(
            sys.after_kill_lookup.lock().unwrap().is_none(),
            "the Kill step found the root"
        );
        assert!(matches!(
            root.lane.reason(),
            Some(StopReason::ParentRequest)
        ));
        assert_eq!(
            sys.backstop_arms.load(Ordering::SeqCst),
            0,
            "nothing was armed"
        );
        assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
        root.task().abort();
    }

    /// The ladder is cut in step 1; before the sweep looks, the root
    /// publishes `Stopped` and leaves: `Graceful`, no `Kill` having been
    /// raised on it.
    async fn a_root_cut_in_step_one_that_ends_before_the_sweep_is_graceful() {
        let sys = scripted_system("cut-step-one");
        let root = Scripted::register(&sys, &ActorId::from("cut-step-one"), LONG_GRACE);
        {
            let root = root.clone();
            set_ladder_hook(&sys, move |point, _id| {
                if point == LadderPoint::SweepLookup {
                    root.end();
                }
            });
        }
        let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
        assert!(matches!(
            root.lane.reason(),
            Some(StopReason::ParentRequest)
        ));
        assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
        root.task().abort();
    }

    /// The ladder is cut in step 2, after it raised `Kill`; before the sweep
    /// looks, the root publishes `Stopped` and leaves: `Killed`.
    async fn a_root_cut_in_step_two_that_ends_before_the_sweep_is_killed() {
        let sys = scripted_system("cut-step-two");
        let root = Scripted::register(&sys, &ActorId::from("cut-step-two"), LONG_GRACE);
        {
            let root = root.clone();
            set_ladder_hook(&sys, move |point, _id| {
                if point == LadderPoint::SweepLookup {
                    root.end();
                }
            });
        }
        let report = shut_down(&sys, CUT_IN_STEP_TWO.0, CUT_IN_STEP_TWO.1).await;
        assert!(
            matches!(root.lane.reason(), Some(StopReason::Kill)),
            "the ladder raised Kill before it was cut"
        );
        assert_eq!(outcome_of(&report, &root.id), StopOutcome::Killed);
        root.task().abort();
    }

    /// The ladder is cut in step 1 and the sweep finds the root, but between
    /// its lookup and its raise the root leaves, seals its lane and publishes
    /// `Stopped`: the lane refuses the sweep's `Kill`, and the root, which
    /// ended with its own reason, is `Graceful`.
    async fn a_sweep_target_that_refuses_the_kill_keeps_its_own_outcome() {
        let sys = scripted_system("sweep-refused");
        let root = Scripted::register(&sys, &ActorId::from("sweep-refused"), LONG_GRACE);
        {
            let root = root.clone();
            *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
                root.leave();
                let _ = root.lane.seal();
                root.publish_stopped();
            }));
        }
        let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
        assert!(
            sys.after_kill_lookup.lock().unwrap().is_none(),
            "the sweep found the root"
        );
        assert!(matches!(
            root.lane.reason(),
            Some(StopReason::ParentRequest)
        ));
        assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
        root.task().abort();
    }

    /// `stop_within(Graceful)` outlives its window; before it raises its own
    /// `Kill`, the actor leaves the registry, seals its lane and publishes
    /// `Stopped`. The lane refuses the `Kill`, nothing is armed, and the call
    /// reports `Graceful`, the outcome the actor reached by itself.
    async fn stop_within_reports_its_kill_only_when_the_lane_took_it() {
        let sys = scripted_system("stop-within-refused");
        let root = Scripted::register(&sys, &ActorId::from("stop-within-refused"), LONG_GRACE);
        let reached = Arc::new(AtomicBool::new(false));
        {
            let (root, reached) = (root.clone(), reached.clone());
            set_ladder_hook(&sys, move |point, _id| {
                if point == LadderPoint::StopWithinKill {
                    reached.store(true, Ordering::SeqCst);
                    root.leave();
                    let _ = root.lane.seal();
                    root.publish_stopped();
                }
            });
        }
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            root.handle
                .stop_within(StopReason::Graceful, Duration::from_millis(20)),
        )
        .await
        .expect("stop_within returns")
        .expect("the graceful request is accepted");
        *sys.ladder_hook.lock().unwrap() = None;
        assert!(
            reached.load(Ordering::SeqCst),
            "the call reached its Kill step"
        );
        assert!(matches!(root.lane.reason(), Some(StopReason::Graceful)));
        assert_eq!(
            sys.backstop_arms.load(Ordering::SeqCst),
            0,
            "nothing was armed"
        );
        assert_eq!(outcome, StopOutcome::Graceful);
        root.task().abort();
    }

    // - Every wait bounded by a kill grace is the incarnation's own ----------

    /// Two placements of one condition, a root with its own 600 ms grace that
    /// leaves the registry and never publishes `Stopped` while the shutdown
    /// runs: gone before the sweep's lookup, or gone between the lookup and
    /// the raise. Both are waited the root's grace and the wait after an
    /// abort, and both are `Unresponsive`.
    async fn a_root_leaving_around_the_sweep_s_lookup_is_waited_its_own_grace() {
        let grace = Duration::from_millis(600);
        for after_the_lookup in [false, true] {
            let sys = scripted_system("own-grace-sweep");
            let root = Scripted::register(&sys, &ActorId::from("own-grace-sweep"), grace);
            if after_the_lookup {
                let root = root.clone();
                *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || root.leave()));
            } else {
                let root = root.clone();
                set_ladder_hook(&sys, move |point, _id| {
                    if point == LadderPoint::SweepLookup {
                        root.leave();
                    }
                });
            }
            let started = Instant::now();
            let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
            let took = started.elapsed();
            assert_eq!(
                outcome_of(&report, &root.id),
                StopOutcome::Unresponsive,
                "after_the_lookup={after_the_lookup}"
            );
            assert!(
                took >= CUT_IN_STEP_ONE.0 + grace + ABORT_SETTLE,
                "after_the_lookup={after_the_lookup}: the root's own grace, took {took:?}"
            );
            root.publish_stopped();
            root.task().abort();
        }
    }

    /// `kill_by_id` on a root that leaves the registry between the lookup and
    /// the raise, and never publishes `Stopped` during the call: the call
    /// waits the root's own grace, not a default.
    async fn kill_by_id_waits_the_incarnation_s_own_grace_after_its_entry_is_gone() {
        let sys = scripted_system("own-grace-kill-by-id");
        let grace = Duration::from_millis(600);
        let root = Scripted::register(&sys, &ActorId::from("own-grace-kill-by-id"), grace);
        {
            let root = root.clone();
            *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || root.leave()));
        }
        let started = Instant::now();
        assert!(sys.kill_by_id(&root.id).await.is_some());
        let took = started.elapsed();
        assert!(
            took >= grace + ABORT_SETTLE,
            "the root's own grace, took {took:?}"
        );
        root.publish_stopped();
        root.task().abort();
    }

    /// `stop_within(Graceful)` outlives its window; before its `Kill` step the
    /// actor leaves the registry and never publishes `Stopped` during the
    /// call. The `Kill` step waits the actor's own grace, read while the
    /// entry held it.
    async fn stop_within_waits_the_incarnation_s_own_grace_after_its_entry_is_gone() {
        let sys = scripted_system("own-grace-stop-within");
        let grace = Duration::from_millis(600);
        let root = Scripted::register(&sys, &ActorId::from("own-grace-stop-within"), grace);
        {
            let root = root.clone();
            set_ladder_hook(&sys, move |point, _id| {
                if point == LadderPoint::StopWithinKill {
                    root.leave();
                }
            });
        }
        let within = Duration::from_millis(20);
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            root.handle.stop_within(StopReason::Graceful, within),
        )
        .await
        .expect("stop_within returns")
        .expect("the graceful request is accepted");
        let took = started.elapsed();
        *sys.ladder_hook.lock().unwrap() = None;
        assert_eq!(outcome, StopOutcome::Unresponsive);
        assert!(
            took >= within + grace + ABORT_SETTLE,
            "the actor's own grace, took {took:?}"
        );
        root.publish_stopped();
        root.task().abort();
    }

    // - A refused Kill arms nothing ------------------------------------------

    /// A registered root whose lane is sealed between a lookup and the raise
    /// (`kill_by_id`, or the sweep's found branch): the lane refuses the
    /// `Kill` and nothing is armed, though the entry still holds its abort;
    /// `kill_by_id` answers `false` and aborts nothing.
    async fn a_refused_kill_arms_nothing_while_the_entry_is_registered() {
        for through_the_sweep in [false, true] {
            let sys = scripted_system("refused-registered");
            let root = Scripted::register(&sys, &ActorId::from("refused-registered"), LONG_GRACE);
            {
                let root = root.clone();
                *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
                    let _ = root.lane.seal();
                    root.publish_stopped();
                }));
            }
            if through_the_sweep {
                let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
                assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
            } else {
                // Answered as `kill(name)` answers a sealed lane, and nothing
                // is aborted.
                assert!(sys.kill_by_id(&root.id).await.is_none());
                let mut task = root.task();
                assert!(
                    tokio::time::timeout(ABORT_SETTLE, &mut task).await.is_err(),
                    "the refused Kill aborted the task"
                );
                task.abort();
            }
            assert!(
                sys.after_kill_lookup.lock().unwrap().is_none(),
                "through_the_sweep={through_the_sweep}: the lookup found the root"
            );
            assert_eq!(
                sys.backstop_arms.load(Ordering::SeqCst),
                0,
                "through_the_sweep={through_the_sweep}: nothing was armed"
            );
            root.leave();
            if through_the_sweep {
                root.task().abort();
            }
        }
    }

    // - Late roots ------------------------------------------------------------

    /// Two roots registered while the first root's ladder runs, which then
    /// ends; the global timeout cuts the first late root's ladder, so both go
    /// to one sweep. The first left the registry before the sweep looked and
    /// publishes `Stopped` only once the sweep looks at the second: it is
    /// waited for, concurrently, and reported `Graceful`.
    async fn late_roots_are_swept_together() {
        let sys = scripted_system("late-together");
        let grace = Duration::from_millis(600);
        let root = Scripted::register(&sys, &ActorId::from("late-root"), LONG_GRACE);
        let late: Arc<Mutex<Vec<Arc<Scripted>>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let (hook_sys, root, late) = (sys.clone(), root.clone(), late.clone());
            set_ladder_hook(&sys, move |point, id| {
                let registered = late.lock().unwrap().clone();
                match (point, registered.as_slice()) {
                    (LadderPoint::Stop, []) if *id == root.id => {
                        let second =
                            Scripted::register(&hook_sys, &ActorId::from("late-second"), grace);
                        // Registered last: laddered first, and cut there.
                        let first =
                            Scripted::register(&hook_sys, &ActorId::from("late-first"), grace);
                        *late.lock().unwrap() = vec![first, second];
                        root.end();
                    }
                    (LadderPoint::SweepLookup, [first, second]) => {
                        if *id == first.id {
                            first.leave();
                        } else if *id == second.id {
                            first.publish_stopped();
                            second.end();
                        }
                    }
                    _ => {}
                }
            });
        }
        // Step 1 of every late root waits until the deadline.
        let report = shut_down(&sys, Duration::from_millis(300), Duration::from_secs(30)).await;
        let late = late.lock().unwrap().clone();
        assert_eq!(late.len(), 2, "the late roots were registered");
        assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
        assert_eq!(outcome_of(&report, &late[0].id), StopOutcome::Graceful);
        assert_eq!(outcome_of(&report, &late[1].id), StopOutcome::Graceful);
        for scripted in [&root, &late[0], &late[1]] {
            scripted.task().abort();
        }
    }

    /// A root registered while the first root's ladder runs, which then ends;
    /// the late root's ladder raises `Kill` and is cut in step 2, and before
    /// the sweep looks it ends: `Killed`.
    async fn a_late_root_cut_in_step_two_that_ends_before_the_sweep_is_killed() {
        let sys = scripted_system("late-killed");
        let root = Scripted::register(&sys, &ActorId::from("late-killed-root"), LONG_GRACE);
        let late: Arc<Mutex<Option<Arc<Scripted>>>> = Arc::new(Mutex::new(None));
        {
            let (hook_sys, root, late) = (sys.clone(), root.clone(), late.clone());
            set_ladder_hook(&sys, move |point, id| {
                let registered = late.lock().unwrap().clone();
                match (point, registered) {
                    (LadderPoint::Stop, None) if *id == root.id => {
                        let late_root = Scripted::register(
                            &hook_sys,
                            &ActorId::from("late-killed"),
                            LONG_GRACE,
                        );
                        *late.lock().unwrap() = Some(late_root);
                        root.end();
                    }
                    (LadderPoint::SweepLookup, Some(late_root)) if *id == late_root.id => {
                        late_root.end();
                    }
                    _ => {}
                }
            });
        }
        let report = shut_down(&sys, CUT_IN_STEP_TWO.0, CUT_IN_STEP_TWO.1).await;
        let late_root = late
            .lock()
            .unwrap()
            .clone()
            .expect("the late root was registered");
        assert!(
            matches!(late_root.lane.reason(), Some(StopReason::Kill)),
            "the late root's ladder raised Kill before it was cut"
        );
        assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
        assert_eq!(outcome_of(&report, &late_root.id), StopOutcome::Killed);
        root.task().abort();
        late_root.task().abort();
    }

    // - The verdict follows the Kill the call's lane took --------------------

    /// The same end, an actor that left the registry and then published
    /// `Stopped` by itself, read by two calls. `stop_within` raises its `Kill`
    /// on the actor's own lane, which takes it: `Killed`. The shutdown's
    /// ladder finds the actor out of the registry at its `Kill` step and
    /// raises nothing: `Graceful`.
    async fn a_kill_the_lane_took_is_killed_and_one_never_raised_is_graceful() {
        // stop_within: the actor leaves before the Kill step and publishes
        // `Stopped` once its lane holds the `Kill`.
        let sys = scripted_system("verdict-split-handle");
        let root = Scripted::register(&sys, &ActorId::from("verdict-split-handle"), LONG_GRACE);
        {
            let root = root.clone();
            set_ladder_hook(&sys, move |point, _id| {
                if point == LadderPoint::StopWithinKill {
                    root.leave();
                }
            });
        }
        let ends = {
            let root = root.clone();
            tokio::spawn(async move {
                let bound = Instant::now() + Duration::from_secs(10);
                while !matches!(root.lane.reason(), Some(StopReason::Kill))
                    && Instant::now() < bound
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                root.publish_stopped();
            })
        };
        let through_the_handle = tokio::time::timeout(
            Duration::from_secs(10),
            root.handle
                .stop_within(StopReason::Graceful, Duration::from_millis(20)),
        )
        .await
        .expect("stop_within returns")
        .expect("the graceful request is accepted");
        *sys.ladder_hook.lock().unwrap() = None;
        ends.await.unwrap();
        root.task().abort();

        // The shutdown's ladder: the actor leaves and publishes `Stopped`
        // before the Kill step, which finds no entry.
        let sys = scripted_system("verdict-split-ladder");
        let root = Scripted::register(&sys, &ActorId::from("verdict-split-ladder"), LONG_GRACE);
        {
            let root = root.clone();
            set_ladder_hook(&sys, move |point, _id| {
                if point == LadderPoint::Kill {
                    root.leave();
                    root.publish_stopped();
                }
            });
        }
        let report = shut_down(&sys, Duration::from_secs(30), Duration::from_millis(1)).await;
        root.task().abort();

        assert_eq!(
            (through_the_handle, outcome_of(&report, &root.id)),
            (StopOutcome::Killed, StopOutcome::Graceful)
        );
    }

    // - What the shutdown has taken on ----------------------------------------

    /// A root and a late root that both stay registered after their ladders
    /// (neither publishes `Stopped`): each is reported once, the late pass
    /// ends, and the leftover sweep takes on neither again.
    async fn a_root_left_registered_after_its_ladder_is_reported_once() {
        let sys = scripted_system("left-registered");
        let grace = Duration::from_millis(20);
        let root = Scripted::register(&sys, &ActorId::from("left-registered-root"), grace);
        let late: Arc<Mutex<Option<Arc<Scripted>>>> = Arc::new(Mutex::new(None));
        {
            let (hook_sys, root, late) = (sys.clone(), root.clone(), late.clone());
            set_ladder_hook(&sys, move |point, id| {
                if point == LadderPoint::Stop && *id == root.id && late.lock().unwrap().is_none() {
                    let late_root = Scripted::register(
                        &hook_sys,
                        &ActorId::from("left-registered-late"),
                        grace,
                    );
                    *late.lock().unwrap() = Some(late_root);
                }
            });
        }
        let report = shut_down(&sys, Duration::from_secs(30), grace).await;
        let late_root = late
            .lock()
            .unwrap()
            .clone()
            .expect("the late root was registered");
        assert_eq!(
            outcomes_of(&report.outcomes),
            vec![
                (root.id.clone(), StopOutcome::Unresponsive),
                (late_root.id.clone(), StopOutcome::Unresponsive),
            ]
        );
        assert!(report.swept.is_empty(), "{report:?}");
        for scripted in [&root, &late_root] {
            scripted.end();
            scripted.task().abort();
        }
    }

    /// The ladder raised `Kill` on a root and was cut in step 2; before the
    /// sweep looks, the root seals its lane and publishes `Stopped` while
    /// still registered. The sweep finds it and its own `Kill` is refused,
    /// but this shutdown had raised `Kill` on it: `Killed`.
    async fn a_root_the_ladder_raised_kill_on_is_killed_though_the_sweep_s_kill_is_refused() {
        let sys = scripted_system("carried-kill");
        let root = Scripted::register(&sys, &ActorId::from("carried-kill"), LONG_GRACE);
        {
            let root = root.clone();
            set_ladder_hook(&sys, move |point, _id| {
                if point == LadderPoint::SweepLookup {
                    let _ = root.lane.seal();
                    root.publish_stopped();
                }
            });
        }
        let report = shut_down(&sys, CUT_IN_STEP_TWO.0, CUT_IN_STEP_TWO.1).await;
        assert!(matches!(root.lane.reason(), Some(StopReason::Kill)));
        assert_eq!(outcome_of(&report, &root.id), StopOutcome::Killed);
        root.leave();
        root.task().abort();
    }

    // - Registration against a shutdown -------------------------------------

    /// Registers a root under `id` from a thread of its own. Once registered,
    /// the root's task, spawned on `rt`, ends on the first stop request its
    /// lane receives: it frees its registry entry, then publishes `Stopped`.
    fn register_a_root_that_ends_when_asked(
        sys: &Arc<ActorSystem>,
        id: &ActorId,
        rt: &Handle,
    ) -> Result<(), SpawnError> {
        let (tx, _rx) = mpsc::channel(1);
        let (system_tx, _system_rx) = mpsc::channel(1);
        let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, mut stop_rx) = StopLane::new();
        let handle = ActorHandle::<Dummy>::new(
            id.clone(),
            tx,
            system_tx,
            stop_lane,
            1,
            status_rx,
            sys.name().into(),
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let seq = sys.register_actor::<Dummy>(
            id,
            None,
            &handle,
            true,
            TurnClock::default(),
            LONG_GRACE,
            rt.clone(),
        )?;
        let guard = RegistryGuard::new(sys.clone(), id.clone(), None, seq);
        let task = rt.spawn(async move {
            let _ = stop_rx.changed().await;
            drop(guard);
            StatusValue::write_reason(&status_tx, crate::types::StopReason::Graceful, true);
        });
        sys.attach_abort(id, None, task.abort_handle(), seq);
        Ok(())
    }

    /// A registration has passed its phase check when the shutdown begins,
    /// and holds there until the shutdown has reached the registration gate
    /// and then until it has taken its last root snapshot (or a second has
    /// passed). The shutdown's phase change waits for the registration to
    /// complete, so the root is in its first snapshot: it is stopped and
    /// reported, and nothing is left registered or swept.
    async fn a_registration_in_progress_when_the_shutdown_begins_is_stopped_by_it() {
        let sys = scripted_system("registration-race");
        let id = ActorId::from("racing-root");
        let in_window = Arc::new(AtomicBool::new(false));
        let saw_the_shutdown = Arc::new(AtomicBool::new(false));
        {
            let (hook_sys, in_window, saw) =
                (sys.clone(), in_window.clone(), saw_the_shutdown.clone());
            *sys.after_phase_check.lock().unwrap() = Some(Arc::new(move || {
                in_window.store(true, Ordering::SeqCst);
                let bound = std::time::Instant::now() + Duration::from_secs(10);
                while !hook_sys.shutdown_at_the_gate.load(Ordering::SeqCst)
                    && std::time::Instant::now() < bound
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                saw.store(
                    hook_sys.shutdown_at_the_gate.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                );
                let bound = std::time::Instant::now() + Duration::from_secs(1);
                while !hook_sys.root_snapshots_done.load(Ordering::SeqCst)
                    && std::time::Instant::now() < bound
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }));
        }
        let registering = {
            let (sys, id, rt) = (sys.clone(), id.clone(), Handle::current());
            std::thread::spawn(move || register_a_root_that_ends_when_asked(&sys, &id, &rt))
        };
        until("the registration to pass its phase check", || {
            in_window.load(Ordering::SeqCst)
        })
        .await;
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(5),
                per_actor_timeout: Duration::from_secs(2),
            }),
        )
        .await
        .expect("the shutdown returns");
        let registered = tokio::task::spawn_blocking(move || registering.join())
            .await
            .unwrap()
            .expect("the registering thread ends");
        assert!(
            registered.is_ok(),
            "the registration passed its phase check"
        );
        assert!(
            saw_the_shutdown.load(Ordering::SeqCst),
            "the registration was in progress when the shutdown reached the gate"
        );
        assert_eq!(
            outcomes_of(&report.outcomes),
            vec![(id.clone(), StopOutcome::Graceful)]
        );
        assert!(report.swept.is_empty(), "{report:?}");
        assert!(sys.actor_ids().is_empty(), "nothing is left registered");
    }

    /// Two shutdowns overlap. The second is waiting on a non-root leftover
    /// that has already left the registry when the first, finding nothing,
    /// completes. While the second is in flight, `reactivate` is refused and
    /// a registration is refused; once the second completes, the system is
    /// `Defunct` with nothing registered. The leftover ends before the
    /// deadline with nothing raised on it, so it reads `Graceful`.
    async fn reactivate_waits_for_every_shutdown_in_flight() {
        let sys = scripted_system("overlapping-shutdowns");
        let mut entries = counting_reaper(&sys);
        let leftover =
            Scripted::register_as(&sys, &ActorId::from("overlap-leftover"), LONG_GRACE, false);
        let policy = ShutdownPolicy {
            timeout: Duration::from_secs(30),
            per_actor_timeout: Duration::from_secs(30),
        };
        let second = {
            let (sys, policy) = (sys.clone(), policy.clone());
            tokio::spawn(async move { sys.shutdown_with(policy).await })
        };
        // The second shutdown's sweep took the leftover on (it scheduled the
        // deadline entry for it) and waits for it; the leftover then leaves
        // the registry.
        until(
            "the second shutdown's deadline entry for the leftover",
            || entries.try_recv().is_ok(),
        )
        .await;
        assert!(
            leftover.lane.reason().is_none(),
            "nothing is raised before the deadline"
        );
        leftover.leave();
        let first = tokio::time::timeout(Duration::from_secs(10), sys.shutdown_with(policy))
            .await
            .expect("the first shutdown returns");
        assert!(
            first.outcomes.is_empty() && first.swept.is_empty(),
            "{first:?}"
        );
        assert!(!second.is_finished(), "the second shutdown is in flight");
        assert!(
            !sys.reactivate(),
            "reactivate is refused while a shutdown is in flight"
        );
        let root_id = ActorId::from("overlap-root");
        assert!(
            matches!(
                register_a_root_that_ends_when_asked(&sys, &root_id, &Handle::current()),
                Err(SpawnError::SystemShuttingDown(_))
            ),
            "a registration is refused while a shutdown is in flight"
        );
        let _ = leftover.lane.seal();
        leftover.publish_stopped();
        let second = tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .expect("the second shutdown returns")
            .unwrap();
        assert_eq!(
            outcomes_of(&second.swept),
            vec![(leftover.id.clone(), StopOutcome::Graceful)]
        );
        assert!(sys.actor_ids().is_empty(), "nothing is left registered");
        assert!(
            sys.reactivate(),
            "the last shutdown to complete leaves it Defunct"
        );
        leftover.task().abort();
    }

    /// Three shutdowns: B waits on a non-root leftover; A starts with an
    /// earlier deadline, lowers the system's to it, finds nothing and
    /// completes while B waits; C starts after A with a later deadline. The
    /// system holds A's deadline until the last shutdown in flight (B)
    /// completes: A's completion leaves it, C folds into it, and only B's
    /// completion clears it.
    async fn the_sweep_deadline_is_held_until_the_last_shutdown_completes() {
        let sys = scripted_system("three-shutdowns");
        let mut entries = counting_reaper(&sys);
        let leftover =
            Scripted::register_as(&sys, &ActorId::from("held-leftover"), LONG_GRACE, false);
        let policy = |secs| ShutdownPolicy {
            timeout: Duration::from_secs(secs),
            per_actor_timeout: Duration::from_secs(secs),
        };
        let b = {
            let sys = sys.clone();
            tokio::spawn(async move { sys.shutdown_with(policy(60)).await })
        };
        until("B's deadline entry for the leftover", || {
            entries.try_recv().is_ok()
        })
        .await;
        let b_deadline = sys
            .sweep_deadline
            .borrow()
            .expect("B's sweep holds its deadline");
        leftover.leave();
        let a = tokio::time::timeout(Duration::from_secs(10), sys.shutdown_with(policy(30)))
            .await
            .expect("A returns");
        assert!(a.outcomes.is_empty() && a.swept.is_empty(), "{a:?}");
        let held = sys
            .sweep_deadline
            .borrow()
            .expect("A's completion leaves the deadline while B is in flight");
        assert!(
            held < b_deadline,
            "A lowered the system's deadline to its own"
        );
        let c = tokio::time::timeout(Duration::from_secs(10), sys.shutdown_with(policy(90)))
            .await
            .expect("C returns");
        assert!(c.outcomes.is_empty() && c.swept.is_empty(), "{c:?}");
        assert_eq!(
            *sys.sweep_deadline.borrow(),
            Some(held),
            "C folded into the held deadline and its completion left it"
        );
        assert!(!b.is_finished(), "B is in flight");
        let _ = leftover.lane.seal();
        leftover.publish_stopped();
        let b = tokio::time::timeout(Duration::from_secs(10), b)
            .await
            .expect("B returns")
            .unwrap();
        assert_eq!(
            outcomes_of(&b.swept),
            vec![(leftover.id.clone(), StopOutcome::Graceful)]
        );
        assert_eq!(
            *sys.sweep_deadline.borrow(),
            None,
            "the last shutdown to complete clears it"
        );
        assert!(sys.reactivate());
        leftover.task().abort();
    }

    /// A leftover whose end became final before the deadline (its lane
    /// sealed, its reason written) publishes `Stopped` only between the
    /// force's lookup and its raise. The force's `Kill` is refused, so this
    /// shutdown raised nothing on it, and it reads `Graceful` with its own
    /// reason.
    async fn a_leftover_final_before_the_deadline_and_stopped_at_the_force_reads_graceful() {
        let sys = scripted_system("final-before-force");
        let leftover =
            Scripted::register_as(&sys, &ActorId::from("final-early"), LONG_GRACE, false);
        let _ = leftover.lane.seal();
        let tx = leftover
            .status_tx
            .lock()
            .unwrap()
            .take()
            .expect("the plane's sender");
        assert!(StatusValue::write_reason(&tx, StopReason::Graceful, false));
        *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
            tx.send_modify(|value| value.status = ActorStatus::Stopped);
        }));
        let report = shut_down(&sys, Duration::from_millis(200), Duration::from_secs(5)).await;
        assert_eq!(
            report.swept,
            vec![(
                leftover.id.clone(),
                StopReport {
                    outcome: StopOutcome::Graceful,
                    reason: Some(StopReason::Graceful),
                }
            )]
        );
        leftover.task().abort();
    }

    /// A sweep that lowers the system's deadline at the very instant an end
    /// became final (the normal case on a paused clock) lands just after
    /// that instant, so the end reads `Graceful` before the lowering and
    /// after it. An own deadline already past is raised to the same floor.
    #[tokio::test(start_paused = true)]
    async fn a_lowering_at_the_instant_of_an_end_lands_after_it() {
        let sys = scripted_system("lowering-tie");
        let first = sys.fold_sweep_deadline(Instant::now() + Duration::from_secs(10));
        let (tx, rx) = watch::channel(StatusValue::new(ActorStatus::Stopping));
        assert!(StatusValue::write_reason(&tx, StopReason::Graceful, true));
        let before = leftover_outcome(&rx, false, sys.current_sweep_deadline(first));
        let lowered = sys.fold_sweep_deadline(Instant::now());
        let final_at = rx.borrow().final_at.expect("the write stamped it");
        assert!(lowered > final_at, "the lowering lands after the end");
        let after = leftover_outcome(&rx, false, sys.current_sweep_deadline(lowered));
        assert_eq!(
            (before, after),
            (StopOutcome::Graceful, StopOutcome::Graceful)
        );
        let past = sys.fold_sweep_deadline(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            past, lowered,
            "an own deadline already past takes the floor"
        );
    }

    /// Two shutdown calls share one sweep deadline and both report one
    /// leftover whose end becomes final at offsets from 60 us before that
    /// deadline to 100 us after it: every round, both calls report it the
    /// same way, that way is the one its recorded final instant gives
    /// (`Graceful` before the deadline, `Killed` at or after it), and the
    /// rounds reach both sides of the deadline. The ending thread starts
    /// well ahead of its instant and spins to it on the monotonic clock, so
    /// a coarse or late timer only shortens the spin, and each round aims
    /// by the shortest time an end has taken so far. Multi-thread only: the
    /// two calls and the end must run in parallel for an end to fall
    /// between one call's look at the leftover and the other's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_shutdowns_report_one_end_the_same_way_around_the_deadline() {
        // Ahead of the end's instant by more than a coarse host's timer
        // granularity (about 15.6 ms on Windows).
        const LEAD: Duration = Duration::from_millis(30);
        let mut sides: Vec<StopOutcome> = Vec::new();
        let mut end_cost: Option<Duration> = None;
        let mut round = 0u64;
        while round < 41 || (sides.len() < 2 && round < 400) {
            let offset = Duration::from_micros((round % 41) * 4);
            let sys = scripted_system("one-end-two-calls");
            let mut entries = counting_reaper(&sys);
            let leftover =
                Scripted::register_as(&sys, &ActorId::from("shared-leftover"), LONG_GRACE, false);
            let policy = ShutdownPolicy {
                timeout: Duration::from_millis(100),
                per_actor_timeout: Duration::from_millis(100),
            };
            let calls: Vec<_> = (0..2)
                .map(|_| {
                    let (sys, policy) = (sys.clone(), policy.clone());
                    tokio::spawn(async move { sys.shutdown_with(policy).await })
                })
                .collect();
            let mut queued = 0;
            until("both calls' deadline entries", || {
                queued += drained(&mut entries);
                queued >= 2
            })
            .await;
            let deadline = sys
                .sweep_deadline
                .borrow()
                .expect("the sweeps hold one deadline");
            let at = deadline - Duration::from_micros(60) + offset - end_cost.unwrap_or_default();
            tokio::time::sleep_until(at - LEAD).await;
            let ending = leftover.clone();
            tokio::task::spawn_blocking(move || {
                while Instant::now() < at {
                    std::hint::spin_loop();
                }
                ending.end();
            })
            .await
            .unwrap();
            let final_at = leftover
                .handle
                .status_rx()
                .borrow()
                .final_at
                .expect("the end stamped its final instant");
            let cost = final_at.saturating_duration_since(at);
            end_cost = Some(end_cost.map_or(cost, |least| least.min(cost)));
            let mut seen = Vec::new();
            for call in calls {
                let report = tokio::time::timeout(Duration::from_secs(10), call)
                    .await
                    .expect("the call returns")
                    .unwrap();
                seen.push(outcomes_of(&report.swept));
            }
            assert_eq!(seen[0], seen[1], "round {round}: one end, one outcome");
            assert_eq!(seen[0].len(), 1, "round {round}: {seen:?}");
            let want = if final_at < deadline {
                StopOutcome::Graceful
            } else {
                StopOutcome::Killed
            };
            assert_eq!(
                seen[0][0].1,
                want,
                "round {round}: the end became final {:?} before the deadline, {:?} after it",
                deadline.saturating_duration_since(final_at),
                final_at.saturating_duration_since(deadline)
            );
            if !sides.contains(&want) {
                sides.push(want);
            }
            leftover.task().abort();
            round += 1;
        }
        assert!(
            sides.contains(&StopOutcome::Graceful) && sides.contains(&StopOutcome::Killed),
            "the rounds reach both sides of the deadline in {round} rounds: {sides:?}"
        );
    }

    macro_rules! both_flavors {
        ($($body:ident => $multi:ident, $single:ident;)*) => {$(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $multi() {
                $body().await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn $single() {
                $body().await;
            }
        )*};
    }

    both_flavors! {
        the_sweep_deadline_is_held_until_the_last_shutdown_completes =>
            the_sweep_deadline_is_held_until_the_last_shutdown_completes_multi_thread,
            the_sweep_deadline_is_held_until_the_last_shutdown_completes_current_thread;
        a_leftover_final_before_the_deadline_and_stopped_at_the_force_reads_graceful =>
            a_leftover_final_before_the_deadline_and_stopped_at_the_force_reads_graceful_multi_thread,
            a_leftover_final_before_the_deadline_and_stopped_at_the_force_reads_graceful_current_thread;
        the_sweep_waits_for_a_root_gone_before_it_published_stopped =>
            the_sweep_waits_for_a_root_gone_before_it_published_stopped_multi_thread,
            the_sweep_waits_for_a_root_gone_before_it_published_stopped_current_thread;
        a_root_gone_and_not_stopped_within_its_bound_is_unresponsive =>
            a_root_gone_and_not_stopped_within_its_bound_is_unresponsive_multi_thread,
            a_root_gone_and_not_stopped_within_its_bound_is_unresponsive_current_thread;
        a_root_gone_at_the_kill_step_reports_no_raised_kill =>
            a_root_gone_at_the_kill_step_reports_no_raised_kill_multi_thread,
            a_root_gone_at_the_kill_step_reports_no_raised_kill_current_thread;
        a_leftover_gone_at_the_sweep_lookup_reports_the_reason_its_plane_holds =>
            a_leftover_gone_at_the_sweep_lookup_reports_the_reason_its_plane_holds_multi_thread,
            a_leftover_gone_at_the_sweep_lookup_reports_the_reason_its_plane_holds_current_thread;
        a_successor_registered_before_the_kill_step_is_never_raised_on =>
            a_successor_registered_before_the_kill_step_is_never_raised_on_multi_thread,
            a_successor_registered_before_the_kill_step_is_never_raised_on_current_thread;
        a_root_that_refuses_the_kill_step_keeps_its_own_outcome =>
            a_root_that_refuses_the_kill_step_keeps_its_own_outcome_multi_thread,
            a_root_that_refuses_the_kill_step_keeps_its_own_outcome_current_thread;
        a_root_cut_in_step_one_that_ends_before_the_sweep_is_graceful =>
            a_root_cut_in_step_one_that_ends_before_the_sweep_is_graceful_multi_thread,
            a_root_cut_in_step_one_that_ends_before_the_sweep_is_graceful_current_thread;
        a_root_cut_in_step_two_that_ends_before_the_sweep_is_killed =>
            a_root_cut_in_step_two_that_ends_before_the_sweep_is_killed_multi_thread,
            a_root_cut_in_step_two_that_ends_before_the_sweep_is_killed_current_thread;
        the_ladder_never_acts_on_a_successor_registered_before_it_began =>
            the_ladder_never_acts_on_a_successor_registered_before_it_began_multi_thread,
            the_ladder_never_acts_on_a_successor_registered_before_it_began_current_thread;
        the_abort_step_never_aborts_a_successor_registered_before_it =>
            the_abort_step_never_aborts_a_successor_registered_before_it_multi_thread,
            the_abort_step_never_aborts_a_successor_registered_before_it_current_thread;
        the_sweep_never_kills_a_successor_registered_before_it_looked =>
            the_sweep_never_kills_a_successor_registered_before_it_looked_multi_thread,
            the_sweep_never_kills_a_successor_registered_before_it_looked_current_thread;
        a_root_leaving_around_the_sweep_s_lookup_is_waited_its_own_grace =>
            a_root_leaving_around_the_sweep_s_lookup_is_waited_its_own_grace_multi_thread,
            a_root_leaving_around_the_sweep_s_lookup_is_waited_its_own_grace_current_thread;
        kill_by_id_waits_the_incarnation_s_own_grace_after_its_entry_is_gone =>
            kill_by_id_waits_the_incarnation_s_own_grace_after_its_entry_is_gone_multi_thread,
            kill_by_id_waits_the_incarnation_s_own_grace_after_its_entry_is_gone_current_thread;
        stop_within_waits_the_incarnation_s_own_grace_after_its_entry_is_gone =>
            stop_within_waits_the_incarnation_s_own_grace_after_its_entry_is_gone_multi_thread,
            stop_within_waits_the_incarnation_s_own_grace_after_its_entry_is_gone_current_thread;
        a_refused_kill_arms_nothing_while_the_entry_is_registered =>
            a_refused_kill_arms_nothing_while_the_entry_is_registered_multi_thread,
            a_refused_kill_arms_nothing_while_the_entry_is_registered_current_thread;
        late_roots_are_swept_together =>
            late_roots_are_swept_together_multi_thread,
            late_roots_are_swept_together_current_thread;
        a_late_root_cut_in_step_two_that_ends_before_the_sweep_is_killed =>
            a_late_root_cut_in_step_two_that_ends_before_the_sweep_is_killed_multi_thread,
            a_late_root_cut_in_step_two_that_ends_before_the_sweep_is_killed_current_thread;
        a_kill_the_lane_took_is_killed_and_one_never_raised_is_graceful =>
            a_kill_the_lane_took_is_killed_and_one_never_raised_is_graceful_multi_thread,
            a_kill_the_lane_took_is_killed_and_one_never_raised_is_graceful_current_thread;
        a_root_left_registered_after_its_ladder_is_reported_once =>
            a_root_left_registered_after_its_ladder_is_reported_once_multi_thread,
            a_root_left_registered_after_its_ladder_is_reported_once_current_thread;
        a_root_the_ladder_raised_kill_on_is_killed_though_the_sweep_s_kill_is_refused =>
            a_root_the_ladder_raised_kill_on_is_killed_though_the_sweep_s_kill_is_refused_multi_thread,
            a_root_the_ladder_raised_kill_on_is_killed_though_the_sweep_s_kill_is_refused_current_thread;
        reactivate_waits_for_every_shutdown_in_flight =>
            reactivate_waits_for_every_shutdown_in_flight_multi_thread,
            reactivate_waits_for_every_shutdown_in_flight_current_thread;
        a_registration_in_progress_when_the_shutdown_begins_is_stopped_by_it =>
            a_registration_in_progress_when_the_shutdown_begins_is_stopped_by_it_multi_thread,
            a_registration_in_progress_when_the_shutdown_begins_is_stopped_by_it_current_thread;
        stop_within_reports_its_kill_only_when_the_lane_took_it =>
            stop_within_reports_its_kill_only_when_the_lane_took_it_multi_thread,
            stop_within_reports_its_kill_only_when_the_lane_took_it_current_thread;
        a_sweep_target_that_refuses_the_kill_keeps_its_own_outcome =>
            a_sweep_target_that_refuses_the_kill_keeps_its_own_outcome_multi_thread,
            a_sweep_target_that_refuses_the_kill_keeps_its_own_outcome_current_thread;
    }

    // - A kill never arms the incarnation a supervisor restarted under its id -

    /// Counts its starts; does nothing else.
    struct Idle {
        builds: Arc<AtomicU64>,
    }

    impl Actor for Idle {
        type Message = ();
        type Response = ();

        async fn on_started(
            &mut self,
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    /// Supervises one permanent `Idle` child under `child`.
    struct IdleBoss {
        child: String,
        builds: Arc<AtomicU64>,
    }

    impl Actor for IdleBoss {
        type Message = ();
        type Response = ();

        async fn on_started(
            &mut self,
            ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            let builds = self.builds.clone();
            ctx.spawn_child(move || Idle {
                builds: builds.clone(),
            })
            .named(self.child.clone())
            .restart_type(crate::types::RestartType::Permanent)
            .await?;
            Ok(())
        }

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum KillCall {
        ByName,
        ById,
    }

    /// A supervised permanent child is killed; between the kill's raise and
    /// its re-read of the registry, the caller waits until the supervisor has
    /// restarted the child under the same id and name. The kill arms nothing:
    /// the incarnation it raised on is gone, and the restarted one is not it.
    /// `actors` is the runtime the supervisor and its child live on (the
    /// caller's when `None`).
    async fn a_kill_never_arms_the_restarted_incarnation(call: KillCall, actors: Option<Handle>) {
        use crate::actor::ActorExt;

        let sys = ActorSystem::create(format!("restarted-{}", uuid::Uuid::new_v4())).unwrap();
        let builds = Arc::new(AtomicU64::new(0));
        let child_name = format!("restarted-child-{}", uuid::Uuid::new_v4());
        let spawn_boss = {
            let (sys, child, builds) = (sys.clone(), child_name.clone(), builds.clone());
            async move {
                IdleBoss { child, builds }
                    .spawn()
                    .named(format!("restarted-boss-{}", uuid::Uuid::new_v4()))
                    .on_system(&sys)
                    .supervisor()
                    .await
                    .unwrap();
            }
        };
        match &actors {
            Some(rt) => rt.spawn(spawn_boss).await.unwrap(),
            None => spawn_boss.await,
        }
        until("the first incarnation to start", || {
            builds.load(Ordering::SeqCst) == 1
        })
        .await;
        let child = loop {
            if let Some(child) = sys.get::<Idle>(&child_name) {
                break child;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        {
            // The caller is the test's own thread (never a worker of the
            // actors' runtime), so holding it here lets the supervisor run.
            let builds = builds.clone();
            *sys.after_kill_raise.lock().unwrap() = Some(Arc::new(move || {
                let bound = std::time::Instant::now() + Duration::from_secs(10);
                while builds.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < bound {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }));
        }
        match call {
            KillCall::ByName => sys.kill(&child_name).await.unwrap(),
            KillCall::ById => assert!(sys.kill_by_id(child.id()).await.is_some()),
        }
        assert_eq!(
            builds.load(Ordering::SeqCst),
            2,
            "{call:?}: the supervisor restarted the killed incarnation before the re-read"
        );
        assert_eq!(
            sys.backstop_arms.load(Ordering::SeqCst),
            0,
            "{call:?}: the kill armed the restarted incarnation"
        );
        // Past the restarted incarnation's grace: it was not aborted, so the
        // supervisor built nothing more.
        tokio::time::sleep(DEFAULT_KILL_GRACE + ABORT_SETTLE + Duration::from_millis(300)).await;
        assert_eq!(builds.load(Ordering::SeqCst), 2, "{call:?}: it was aborted");
    }

    /// The caller on a `current_thread` runtime, the supervisor and its child
    /// on a second runtime that keeps running while the caller is held.
    fn a_kill_from_a_current_thread_caller_never_arms_the_restarted_incarnation(call: KillCall) {
        let actors = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let caller = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let on = actors.handle().clone();
        caller.block_on(a_kill_never_arms_the_restarted_incarnation(call, Some(on)));
        caller.shutdown_background();
        actors.shutdown_background();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_by_name_never_arms_the_restarted_incarnation_multi_thread() {
        a_kill_never_arms_the_restarted_incarnation(KillCall::ByName, None).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_by_id_never_arms_the_restarted_incarnation_multi_thread() {
        a_kill_never_arms_the_restarted_incarnation(KillCall::ById, None).await;
    }

    #[test]
    fn kill_by_name_never_arms_the_restarted_incarnation_current_thread_caller() {
        a_kill_from_a_current_thread_caller_never_arms_the_restarted_incarnation(KillCall::ByName);
    }

    #[test]
    fn kill_by_id_never_arms_the_restarted_incarnation_current_thread_caller() {
        a_kill_from_a_current_thread_caller_never_arms_the_restarted_incarnation(KillCall::ById);
    }

    /// A root that parks in `on_started`: a `Kill` raised on it is never
    /// observed, and only an abort ends its task.
    struct Parked;

    impl Actor for Parked {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }

        async fn on_started(
            &mut self,
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    /// Spawns a parked root on `sys`. The returned status sender outlives the
    /// task: publishing `Stopped` through it after the abort stands for an
    /// actor that, still inside a synchronous `Drop` when its task was
    /// aborted, reached its end and published `Stopped` itself; dropping it
    /// stands for a task the abort ended.
    fn spawn_parked(
        sys: &Arc<ActorSystem>,
    ) -> (
        ActorHandle<Parked>,
        tokio::task::JoinHandle<crate::types::StopReason>,
        watch::Sender<StatusValue>,
    ) {
        let name = format!("parked-{}", uuid::Uuid::new_v4());
        let (handle, join, status_tx, _subtree) = crate::actor::runtime::spawn_actor_parts(
            ActorId::from(name.clone()),
            Parked,
            crate::actor::runtime::ActorConfig::default(),
            Some(name),
            Some(sys.clone()),
            None,
            true,
            DEFAULT_KILL_GRACE,
            None,
        )
        .expect("the actor spawns");
        (handle, join, status_tx)
    }

    /// Moves the paused clock forward until the aborted task has ended; the
    /// caller's settle wait has just begun.
    async fn advance_until_finished(join: &tokio::task::JoinHandle<crate::types::StopReason>) {
        while !join.is_finished() {
            tokio::time::advance(Duration::from_millis(1)).await;
        }
    }

    /// After the abort step, the plane says how the actor ended: `Stopped`
    /// published means it reached its end itself, `Killed`; a plane closed
    /// without it means the abort ended the task, `Aborted`.
    #[tokio::test(start_paused = true)]
    async fn stop_within_reads_the_plane_after_its_abort() {
        let sys = ActorSystem::create(format!("after-abort-{}", uuid::Uuid::new_v4())).unwrap();
        for published in [true, false] {
            let (handle, join, status_tx) = spawn_parked(&sys);
            let call = tokio::spawn(async move {
                handle
                    .stop_within(crate::types::StopReason::Kill, Duration::from_millis(10))
                    .await
            });
            advance_until_finished(&join).await;
            if published {
                crate::actor::runtime::publish_stopped(&status_tx);
            } else {
                drop(status_tx);
            }
            let outcome = call.await.unwrap().expect("accepted");
            let expected = if published {
                StopOutcome::Killed
            } else {
                StopOutcome::Aborted
            };
            assert_eq!(outcome, expected, "Stopped published: {published}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_reads_the_plane_after_its_abort() {
        for published in [true, false] {
            let sys = ActorSystem::create(format!("after-abort-{}", uuid::Uuid::new_v4())).unwrap();
            let (handle, join, status_tx) = spawn_parked(&sys);
            let id = handle.id().clone();
            let call = tokio::spawn({
                let sys = sys.clone();
                async move {
                    sys.shutdown_with(ShutdownPolicy {
                        timeout: Duration::from_secs(30),
                        per_actor_timeout: Duration::from_millis(10),
                    })
                    .await
                }
            });
            advance_until_finished(&join).await;
            if published {
                crate::actor::runtime::publish_stopped(&status_tx);
            } else {
                drop(status_tx);
            }
            let report = call.await.unwrap();
            let outcome = report
                .outcomes
                .iter()
                .find(|(actor, _)| *actor == id)
                .map(|(_, report)| report.outcome);
            let expected = if published {
                StopOutcome::Killed
            } else {
                StopOutcome::Aborted
            };
            assert_eq!(outcome, Some(expected), "Stopped published: {published}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn force_stop_reads_the_plane_after_its_abort() {
        for published in [true, false] {
            let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
            let target = tokio::spawn(std::future::pending::<crate::types::StopReason>());
            let abort = target.abort_handle();
            let call = tokio::spawn(force_stop(
                status_rx,
                Some(abort),
                AbortRecord::new(),
                Duration::from_millis(10),
                StopOutcome::Killed,
            ));
            advance_until_finished(&target).await;
            if published {
                crate::actor::runtime::publish_stopped(&status_tx);
            } else {
                drop(status_tx);
            }
            let expected = if published {
                StopOutcome::Killed
            } else {
                StopOutcome::Aborted
            };
            let report = call.await.unwrap();
            assert_eq!(report.outcome, expected, "Stopped published: {published}");
            // An aborted task's plane closes with no reason written here, as
            // a stand-in plane with no context: the report reads `Kill`.
            assert_eq!(report.reason, Some(crate::types::StopReason::Kill));
        }
    }

    /// The force of a final-sweep leftover whose registry entry is freed:
    /// the sweep raises nothing on it and has no abort of its own, so the
    /// only `Kill` and abort it gets are the two-stage entry the sweep fed at
    /// its snapshot. The reaper is held back by a gate: the entry is fed only
    /// `late` after its `Kill` instant, which is how an entry behaves whose
    /// `Kill` stage fires late. The force is judged by the reaper's record:
    /// `Aborted` when that abort ends the leftover, `Unresponsive` only the
    /// settle time after it.
    mod late_kill_sweep {
        use super::*;

        const GRACE: Duration = Duration::from_millis(100);
        const LATE: Duration = Duration::from_millis(150);

        /// A leftover in its teardown: no request reaches it, only an abort
        /// ends its task. With `stuck`, its plane stays open past its task.
        async fn force_leftover(late: Duration, stuck: bool) -> (StopReport, Instant, Instant) {
            let (lane, _lane_rx) = StopLane::new();
            let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
            let plane = stuck.then(|| status_tx.clone());
            let task = tokio::spawn(async move {
                let _status = status_tx;
                std::future::pending::<()>().await;
            });
            let deadline = Instant::now();
            let entry = ReaperEntry::two_stage(
                deadline,
                lane.clone(),
                deadline + GRACE,
                task.abort_handle(),
            );
            let system =
                ActorSystem::create(format!("late-kill-{}", uuid::Uuid::new_v4())).unwrap();
            let feeder = system.clone();
            tokio::spawn(async move {
                tokio::time::sleep_until(deadline + late).await;
                feeder.feed_reaper(&Handle::current(), entry);
            });
            let report = force_stop(
                status_rx,
                None,
                lane.abort_record().clone(),
                GRACE,
                StopOutcome::Graceful,
            )
            .await;
            let reported = Instant::now();
            drop(plane);
            (report, deadline, reported)
        }

        #[tokio::test(start_paused = true)]
        async fn a_leftover_the_reaper_aborts_late_is_reported_aborted() {
            let (report, deadline, at) = force_leftover(LATE, false).await;
            assert_eq!(report.outcome, StopOutcome::Aborted);
            assert_eq!(at, deadline + LATE + GRACE);
        }

        #[tokio::test(start_paused = true)]
        async fn a_stuck_leftover_is_unresponsive_the_settle_time_after_a_late_abort() {
            let (report, deadline, at) = force_leftover(LATE, true).await;
            assert_eq!(report.outcome, StopOutcome::Unresponsive);
            assert_eq!(at, deadline + LATE + GRACE + ABORT_SETTLE);
        }

        /// The force's own abort is recorded: a reader of the same
        /// incarnation's record sees it at the end of the force's grace.
        #[tokio::test(start_paused = true)]
        async fn a_forces_own_abort_is_recorded() {
            let (lane, _lane_rx) = StopLane::new();
            let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
            let task = tokio::spawn(async move {
                let _status = status_tx.clone();
                std::future::pending::<()>().await;
            });
            let started = Instant::now();
            let report = force_stop(
                status_rx,
                Some(task.abort_handle()),
                lane.abort_record().clone(),
                GRACE,
                StopOutcome::Killed,
            )
            .await;
            assert_eq!(report.outcome, StopOutcome::Aborted);
            assert_eq!(lane.abort_record().view(), (Some(started + GRACE), false));
        }

        /// The control: the abort on time leaves the force's timing as it
        /// was, its grace and then the settle time.
        #[tokio::test(start_paused = true)]
        async fn a_stuck_leftover_aborted_on_time_keeps_the_forces_own_timing() {
            let (report, deadline, at) = force_leftover(Duration::ZERO, true).await;
            assert_eq!(report.outcome, StopOutcome::Unresponsive);
            assert_eq!(at, deadline + GRACE + ABORT_SETTLE);
            let (report, deadline, at) = force_leftover(Duration::ZERO, false).await;
            assert_eq!(report.outcome, StopOutcome::Aborted);
            assert_eq!(at, deadline + GRACE);
        }
    }

    /// A mark never outlives its task. A hold of the actor can: a forwarder
    /// still ending holds it past the task's end. Every path that finds the
    /// task already finished releases the mark instead of skipping it, and
    /// the release counts as the subtree's latest abort, so a wait on the
    /// subtree gives its verdict after the settle time, never at the cap. An
    /// entry built over a task that already finished takes no mark at all.
    mod finished_task {
        use super::*;
        use crate::actor::subtree::SubtreeLink;
        use crate::types::StopLane;
        use tokio::task::JoinHandle;

        const WINDOW: Duration = Duration::from_millis(400);
        const GRACE: Duration = Duration::from_millis(100);

        /// A root and one child below it, the child's lane record linked to
        /// its token, the child's task spawned and not yet run: it finishes
        /// at its first poll, once the test awaits it.
        fn child_task() -> (Arc<SubtreeHold>, Arc<SubtreeHold>, StopLane, JoinHandle<()>) {
            let root = SubtreeHold::new(None);
            let (lane, _lane_rx) = StopLane::new();
            let link = SubtreeLink {
                parent: root.token(),
                own: WINDOW + GRACE,
                infinity: false,
            };
            let child = SubtreeHold::with_record(Some(link), lane.abort_record().clone());
            let task = tokio::spawn(async {});
            (root, child, lane, task)
        }

        /// A root and one child below it, the child's lane record linked to
        /// its token, the child's task finished while the hold lives on.
        async fn finished_child() -> (Arc<SubtreeHold>, Arc<SubtreeHold>, StopLane, AbortHandle) {
            let (root, child, lane, task) = child_task();
            let abort = task.abort_handle();
            task.await.unwrap();
            (root, child, lane, abort)
        }

        /// The mark is released, the end recorded on the subtree, nothing
        /// recorded on the lane, and no mark is taken again.
        fn released(root: &SubtreeHold, lane: &StopLane, at: Instant) {
            assert_eq!(root.watch().abort_view(), (Some(at), false));
            assert_eq!(lane.abort_record().view(), (None, false));
            lane.abort_record().owe();
            assert_eq!(
                root.watch().abort_view(),
                (Some(at), false),
                "nothing is owed to a task that finished"
            );
        }

        /// An entry built while the task runs owes its abort; the task's
        /// end leaves the mark to the entry, which releases it at its first
        /// stage.
        #[tokio::test(start_paused = true)]
        async fn the_reapers_fire_releases_the_mark_of_a_finished_task() {
            let (root, _child, lane, task) = child_task();
            let now = Instant::now();
            let entry = ReaperEntry::two_stage(
                now + WINDOW,
                lane.clone(),
                now + WINDOW + GRACE,
                task.abort_handle(),
            );
            assert_eq!(root.watch().abort_view(), (None, true));
            task.await.unwrap();
            assert_eq!(
                root.watch().abort_view(),
                (None, true),
                "the entry holds the mark until it finds the task finished"
            );
            tokio::time::advance(WINDOW).await;
            let fired = Instant::now();
            assert!(
                entry.fire().is_none(),
                "no stage is left for a finished task"
            );
            released(&root, &lane, fired);
            assert!(
                lane.reason().is_none(),
                "no Kill is raised on a finished task"
            );
        }

        /// An entry built while the task runs and dropped after its end
        /// releases the mark at its drop.
        #[tokio::test(start_paused = true)]
        async fn the_entrys_drop_releases_the_mark_of_a_finished_task() {
            let (root, _child, lane, task) = child_task();
            let now = Instant::now();
            let entry = ReaperEntry::two_stage(
                now + WINDOW,
                lane.clone(),
                now + WINDOW + GRACE,
                task.abort_handle(),
            );
            assert_eq!(root.watch().abort_view(), (None, true));
            task.await.unwrap();
            tokio::time::advance(Duration::from_millis(30)).await;
            let dropped = Instant::now();
            drop(entry);
            released(&root, &lane, dropped);
        }

        #[tokio::test(start_paused = true)]
        async fn the_records_abort_releases_the_mark_of_a_finished_task() {
            let (root, _child, lane, abort) = finished_child().await;
            lane.abort_record().owe();
            assert_eq!(root.watch().abort_view(), (None, true));
            tokio::time::advance(Duration::from_millis(30)).await;
            let aborted = Instant::now();
            lane.abort_record().abort(&abort);
            released(&root, &lane, aborted);
        }

        /// An entry built over a task that already finished, its hold alive
        /// (a forwarder still ending), takes no mark: the subtree reads
        /// nothing owed and nothing recorded for the entry's whole window,
        /// through its drop, and no mark is taken afterwards.
        #[tokio::test(start_paused = true)]
        async fn an_entry_built_over_a_finished_task_owes_nothing() {
            let (root, _child, lane, abort) = finished_child().await;
            assert!(abort.is_finished());
            let now = Instant::now();
            let entry = ReaperEntry::two_stage(
                now + WINDOW,
                lane.clone(),
                now + WINDOW + GRACE,
                abort.clone(),
            );
            assert_eq!(root.watch().abort_view(), (None, false));
            tokio::time::advance(WINDOW - Duration::from_millis(1)).await;
            assert_eq!(
                root.watch().abort_view(),
                (None, false),
                "a mark never outlives its task"
            );
            drop(entry);
            assert_eq!(root.watch().abort_view(), (None, false));
            assert_eq!(lane.abort_record().view(), (None, false));
            let backstop = ReaperEntry::new(now + WINDOW, abort, lane.abort_record().clone());
            assert_eq!(root.watch().abort_view(), (None, false));
            drop(backstop);
            lane.abort_record().owe();
            assert_eq!(
                root.watch().abort_view(),
                (None, false),
                "nothing is owed to a task that finished"
            );
        }
    }

    /// Every abort the system issues against an actor records its instant on
    /// the actor's lane. The reaper here is a stand-in that keeps every entry
    /// it is fed and fires none, so the only abort an actor gets is the one
    /// the path under test issues, and its record is read on the paused
    /// clock.
    mod abort_writers {
        use super::*;
        use crate::actor::ActorExt;
        use crate::error::SpawnError;
        use crate::types::StopReason;

        const GRACE: Duration = Duration::from_millis(100);

        /// The sequential shutdown's last step aborts a root that took
        /// neither its stop nor its `Kill`: the record holds that step's
        /// instant, the end of its stop window and its kill grace.
        #[tokio::test(start_paused = true)]
        async fn a_sequential_stops_abort_is_recorded_on_the_roots_lane() {
            let sys = scripted_system("abort-sequential");
            let _entries = counting_reaper(&sys);
            let root = Scripted::register(&sys, &ActorId::from("hung-root"), GRACE);
            let per_actor = Duration::from_millis(200);
            let step = Arc::new(Mutex::new(None));
            {
                let step = step.clone();
                set_ladder_hook(&sys, move |point, _| {
                    if point == LadderPoint::Abort {
                        *step.lock().unwrap() = Some(Instant::now());
                    }
                });
            }
            let started = Instant::now();
            let report = shut_down(&sys, Duration::from_secs(5), per_actor).await;
            assert_eq!(outcome_of(&report, &root.id), StopOutcome::Unresponsive);
            let step = step.lock().unwrap().expect("the ladder reached its abort");
            assert_eq!(step, started + per_actor + GRACE);
            assert_eq!(root.lane.abort_record().view(), (Some(step), false));
        }

        /// `stop_within(Kill, within)` on an actor that never reaches a turn
        /// boundary aborts it once `within` has passed: the record holds
        /// that instant.
        #[tokio::test(start_paused = true)]
        async fn a_stop_withins_abort_is_recorded_on_the_actors_lane() {
            let sys = scripted_system("abort-stop-within");
            let _entries = counting_reaper(&sys);
            let root = Scripted::register(&sys, &ActorId::from("parked"), GRACE);
            let within = Duration::from_millis(300);
            let started = Instant::now();
            let outcome = root
                .handle
                .stop_within(StopReason::Kill, within)
                .await
                .expect("the lane takes the Kill");
            assert_eq!(outcome, StopOutcome::Unresponsive);
            assert_eq!(
                root.lane.abort_record().view(),
                (Some(started + within), false)
            );
        }

        /// An actor whose `pre_start` never completes: it hands out its stop
        /// lane first.
        struct HangsInPreStart {
            lane: Option<tokio::sync::oneshot::Sender<StopLane>>,
        }

        impl Actor for HangsInPreStart {
            type Message = ();
            type Response = ();

            async fn pre_start(
                &mut self,
                ctx: &mut crate::actor::context::ActorContext<Self>,
            ) -> crate::error::ActorResult<()> {
                if let Some(lane) = self.lane.take() {
                    let _ = lane.send(ctx.self_handle().stop_lane());
                }
                std::future::pending::<()>().await;
                Ok(())
            }

            async fn handle(
                &mut self,
                _msg: (),
                _ctx: &mut crate::actor::context::ActorContext<Self>,
            ) -> crate::error::ActorResult<()> {
                Ok(())
            }
        }

        /// A root spawn's start timeout aborts the initialization: the
        /// record holds the instant the timeout ran out.
        #[tokio::test(start_paused = true)]
        async fn a_root_start_timeouts_abort_is_recorded_on_the_actors_lane() {
            let sys = scripted_system("abort-start-timeout");
            let (lane_tx, lane_rx) = tokio::sync::oneshot::channel();
            let timeout = Duration::from_millis(250);
            let started = Instant::now();
            let spawned = HangsInPreStart {
                lane: Some(lane_tx),
            }
            .spawn()
            .on_system(&sys)
            .start_timeout(timeout)
            .await;
            match spawned {
                Err(SpawnError::StartTimeout) => {}
                Err(other) => panic!("expected StartTimeout, got {other:?}"),
                Ok(_) => panic!("expected StartTimeout, the spawn succeeded"),
            }
            let lane = lane_rx.await.expect("pre_start ran");
            assert_eq!(lane.abort_record().view(), (Some(started + timeout), false));
        }
    }
}
