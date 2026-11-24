//! Actor system providing named actor registration and coordinated shutdown.
//!
//! The actor registry and coordinated shutdown; the public types are also
//! re-exported at the crate root.

#[cfg(test)]
use std::any::{Any, TypeId};
use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use dashmap::DashMap;
use tokio::runtime::Handle;
use tokio::sync::watch;
#[cfg(test)]
use tokio::task::AbortHandle;
use tokio::time::Instant;

#[cfg(test)]
use crate::actor::handle::ActorHandle;
#[cfg(test)]
use crate::actor::incarnation::{
    IncarnationRef, StatusRx, StatusTx, StopLane, SubtreeHold, TurnClock,
};
#[cfg(test)]
use crate::actor::supervision::ABORT_SETTLE;
#[cfg(test)]
use crate::actor::Actor;
#[cfg(test)]
use crate::error::SpawnError;
use crate::helper::HelperKind;
use crate::types::ActorId;
#[cfg(test)]
use crate::types::{ActorStatus, ShutdownReport, StopOutcome, StopReason, StopReport};

// ---------------------------------------------------------------------------
// ShutdownPolicy
// ---------------------------------------------------------------------------

/// Policy controlling how [`ActorSystem::shutdown`] behaves.
#[derive(Debug, Clone)]
pub struct ShutdownPolicy {
    /// The shutdown's global deadline, counted from the call's first poll.
    /// After it, all remaining actors are force-stopped: a shutdown can run on
    /// for the longest kill grace among the roots it forces plus the abort
    /// settle time, then the longest kill grace among the leftovers it forces
    /// plus the abort settle time, and, for either of those two steps, that
    /// time once more plus the abort settle time when the abort of an actor it
    /// forces, scheduled with the system, is issued late. The system holds one
    /// sweep deadline, the earliest of the deadlines of the sweeps that have
    /// started, each taken as no earlier than the moment its sweep begins, and
    /// clears it when the last shutdown in flight ends, provided a shutdown has
    /// completed since the system was last `Active`. Until then, a held
    /// deadline that is earlier than this call's own bounds this call's
    /// leftovers too, whether the sweep that set it is still running, was cut
    /// short or has completed. A timeout beyond 30 years is treated as 30
    /// years.
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
    /// a killed supervisor's children: workers that do not trap exit signals
    /// receive `Kill`, and a child that is itself a supervisor, or a worker
    /// that traps, stops in order within its own `Shutdown`. It
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
    /// default) leaves the watchdog off: the watchdog times turns only while a
    /// threshold is set, so with this left `None`,
    /// [`ActorSystem::turn_elapsed`] returns `None` for every actor and
    /// [`ActorSystem::stuck_actors`] is empty. Setting a threshold adds one
    /// monotonic clock read per callback of every actor in the
    /// system. A turn already in flight when the threshold is first applied
    /// is not timed; the next turn that actor starts is. See
    /// [`ActorSystem::set_turn_warning_threshold`] for what this controls.
    ///
    /// Has no effect without the `telemetry` feature, which is on by default.
    pub turn_warning_threshold: Option<Duration>,
    /// Whether turn tracking starts on (the default is off). See
    /// [`ActorSystem::set_turn_tracking`].
    ///
    /// Has no effect without the `telemetry` feature, which is on by default.
    pub turn_tracking: bool,
    /// Whether wait-cycle detection starts on (the default is off). See
    /// [`ActorSystem::set_wait_cycle_detection`].
    ///
    /// Has no effect without the `telemetry` feature, which is on by default.
    pub wait_cycle_detection: bool,
}

impl SystemConfig {
    /// Sets the initial stuck-turn watchdog threshold (see
    /// [`ActorSystem::set_turn_warning_threshold`]).
    ///
    /// Needs the `telemetry` feature, which is on by default.
    #[cfg(feature = "telemetry")]
    pub fn with_turn_warning_threshold(mut self, threshold: Duration) -> Self {
        self.turn_warning_threshold = Some(threshold);
        self
    }

    /// Starts the system with turn tracking on (see
    /// [`ActorSystem::set_turn_tracking`]).
    ///
    /// Needs the `telemetry` feature, which is on by default.
    #[cfg(feature = "telemetry")]
    pub fn with_turn_tracking(mut self) -> Self {
        self.turn_tracking = true;
        self
    }

    /// Starts the system with wait-cycle detection on (see
    /// [`ActorSystem::set_wait_cycle_detection`]).
    ///
    /// Needs the `telemetry` feature, which is on by default.
    #[cfg(feature = "telemetry")]
    pub fn with_wait_cycle_detection(mut self) -> Self {
        self.wait_cycle_detection = true;
        self
    }
}

#[cfg(test)]
thread_local! {
    static BEFORE_KILL_RAISE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// SystemPhase
// ---------------------------------------------------------------------------

/// Lifecycle phase of an [`ActorSystem`]'s registry.
///
/// `Active` accepts registrations. `shutdown`/`shutdown_with` moves the
/// system to `ShuttingDown` immediately and to `Defunct` once the last
/// shutdown in flight has ended, if a shutdown has completed since the system
/// was last `Active`; both reject new registrations with
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
/// An actor reaches only the actors whose handles it holds, except through its
/// system, which knows every registered actor by id; among its calls, `get` and
/// `get_by_id` return a typed handle to a registered actor of the type asked
/// for (`get` only for a named one), `actor_ids`, `registered` and
/// `actor_status` read the registry, `stop` and `kill` act on a named actor,
/// and `kill_by_id` on any registered id.
///
/// A system lives in the process map of named systems ([`create`](Self::create),
/// [`get_named`](Self::get_named), [`all`](Self::all)) until a shutdown of it
/// has completed, and from then on only as long as something else holds it:
/// its user's `Arc`, or an actor of it that has not ended. When the last holder
/// lets go the system is freed and its name is free; its reaper tasks end once
/// their pending entries have fired.
/// [`reactivate`](Self::reactivate) puts a shut-down system back to staying in
/// the map, and the system named `"default"` stays for the life of the
/// process.
///
/// `ActorSystem` is a phone book, not a runtime. It does not create or own a
/// Tokio runtime. Actors spawn on whatever runtime is current via
/// `Handle::try_current()`, and a supervised child on the runtime its
/// builder names with
/// [`on_runtime`](crate::actor::context::ChildSpawnBuilder::on_runtime), which
/// must have the time driver unless it has already ended.
/// A runtime built without the Tokio time driver
/// (`Builder::enable_time` or `enable_all`) runs plain actors (actors with no
/// timer and no supervised child), but timers, the kill grace and every bound
/// on a wait sleep on the driver: see
/// [`SpawnBuilder`](crate::actor::SpawnBuilder) for what each does without
/// it. A spawn with `start_timeout` panics when first polled on a calling
/// runtime without the driver, and `stop_within`, `kill_by_id` and
/// `kill_by_id_with_observer` panic when first polled on one without it or
/// with none current, in each case before anything changes; a call that, when
/// first looked at, has nothing to wait for (a spawn refused for its mailbox
/// capacity, its system or its name, a target that does not take the request,
/// an id nothing holds) answers first,
/// on any runtime, and a name taken between that look and the registration
/// still panics on a runtime without the driver.
/// `shutdown_with` panics at its first wait on a root, after the system has
/// entered shutdown.
/// [`SubtreeObserver::wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended)
/// changes nothing, and on a calling runtime without the driver or with none
/// current it panics at the first moment it needs a timer: the observed actor
/// has ended and let go of everything it held, and an actor below it has not
/// ended. It waits without a timer while the actor still runs or is still
/// letting go of what it held, and answers a subtree that has ended without
/// one. With `panic = "abort"` the entry checks cannot tell whether a runtime
/// has the driver, so the first timer a call uses on a runtime without it ends
/// the process.
pub struct ActorSystem {
    name: String,
    /// One entry per registered actor, keyed by its id. A named actor's id is
    /// its name, so a lookup by name reads this map too and finds only an
    /// entry marked named.
    by_id: DashMap<RegKey, AnyActorHandle>,
    shutdown_policy: ShutdownPolicy,
    /// The system's telemetry gate and sinks.
    telemetry: crate::telemetry::ModeOwner,
    /// `Active` until `shutdown`/`shutdown_with` is called (moves to
    /// `ShuttingDown`, then to `Defunct` once the last shutdown in flight has
    /// ended, if a shutdown has completed since the system was last `Active`).
    /// Every change happens under `registration_gate`'s exclusive side.
    /// `register_actor` rejects every registration attempt in either
    /// non-`Active` phase with `SpawnError::SystemShuttingDown`. Not a one-way
    /// flag:
    /// [`ActorSystem::reactivate`] moves a `Defunct` system back to `Active`.
    phase: AtomicU8,
    /// Makes a registration atomic with every phase change, and counts the
    /// shutdowns in flight: `register_actor` holds the shared side from its
    /// phase check through its inserts, and every phase change takes the
    /// exclusive side. A shutdown raises the count as it leaves `Active` and
    /// lowers it when it ends; the last one to end stores `Defunct` if a
    /// shutdown has completed since the system was last `Active`
    /// (`completed_since_active`), whichever order they ended in.
    /// `reactivate` returns to `Active` only from `Defunct` with no shutdown
    /// in flight. A registration therefore either lands before a shutdown's
    /// phase change, and is in that shutdown's first snapshot, or sees a
    /// phase other than `Active` and is refused, for as long as any shutdown
    /// is in flight.
    registration_gate: std::sync::RwLock<usize>,
    /// Whether a shutdown has completed since the system was last `Active`.
    /// Set by each shutdown that completes, cleared only by `reactivate`, and
    /// read by the shutdown that lowers the count to zero, which then stores
    /// `Defunct`. While it is set the system is never `Active` again without
    /// a `reactivate`, so every root registered before the first of those
    /// shutdowns was stopped by the one that completed. Read and written only
    /// under `registration_gate`'s exclusive side.
    completed_since_active: AtomicBool,
    /// The process-group scopes of this system by name, each made on first
    /// use (see [`pg_scope`](Self::pg_scope)). Never cleared by a shutdown.
    pub(crate) pg_scopes: Mutex<HashMap<Arc<str>, Arc<crate::actor::pg::PgScopeInner>>>,
    /// How many times a run's end looked for its process-group memberships.
    #[cfg(test)]
    pub(crate) pg_sweeps: AtomicU64,
    /// This system's reaper feeds, one per runtime, each spawned lazily on
    /// its runtime's first use and replaced when its task has finished (see
    /// [`reaper_handle`](Self::reaper_handle)).
    reaper: Mutex<HashMap<tokio::runtime::Id, ReaperSlot>>,
    /// The slots every helper task of this system is spawned through (the
    /// reaper, the turn-watchdog sweep, the drain of a closed queue's late
    /// values), one per runtime: each helper's end is recorded there.
    helpers: crate::helper::HelperSlots,
    /// Stuck-turn watchdog threshold, in nanoseconds; `0` means off. The
    /// watchdog bit of the mode word (read by every actor's [`TurnClock`])
    /// follows it, so a change here takes effect for already-spawned actors
    /// without any extra signaling. See
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    #[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
    turn_warning_threshold_nanos: Arc<AtomicU64>,
    /// Runs once between a system-side kill's lookup and its raise, so a test
    /// can store the target's abort handle at that exact point.
    #[cfg(test)]
    after_kill_lookup: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Runs once between a system-side kill's raise and its re-read of the
    /// registry, so a test can delay that re-read.
    #[cfg(test)]
    after_kill_raise: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Runs once right after a registry `Kill`'s deadline is declared, so a
    /// test can read the entry and the actor's lane at that exact point.
    #[cfg(test)]
    after_kill_declaration: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
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
    /// Runs once inside `is_quiescent`, between its two reads.
    #[cfg(test)]
    between_quiet_passes: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once inside a snapshot, after the registry walk and before the
    /// rows are built.
    #[cfg(test)]
    after_snapshot_walk: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// How many identity passes walked the registry.
    #[cfg(test)]
    identity_walks: std::sync::atomic::AtomicUsize,
    /// Runs once in the sweep task, after it decided to end and before it
    /// clears its latch, so a test can land a want in that window.
    #[cfg(test)]
    before_sweep_latch_clear: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
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
    /// a sweep is still running.
    #[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
    sweep_spawned: Arc<AtomicBool>,
    /// The runtimes the sweep's spawn found without the time driver, so a
    /// spawn on such a runtime does not probe it again. Bounded: cleared when
    /// full.
    #[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
    timerless_runtimes: Mutex<std::collections::HashSet<tokio::runtime::Id>>,
    /// The earliest global deadline of a final sweep that started, held while
    /// that sweep runs and after its shutdown completed or was cut short: its
    /// deadline entries stand, so every sweep, one already waiting included,
    /// bounds and reports its leftovers by it. A watch, so a sweep's waits see
    /// it lowered while they run. Cleared when the last shutdown in flight
    /// ends, provided a shutdown has completed since the system was last
    /// `Active`.
    sweep_deadline: watch::Sender<Option<Instant>>,
    /// The instant `AnyActorHandle::kill_due` counts from.
    epoch: Instant,
    /// This system's identity for the life of the process, from
    /// `NEXT_GENERATION`: the slot in the process map, a handle's equality
    /// and the stop paths of a handle all name a system by it.
    generation: u64,
    /// A weak reference to this system's own allocation, for the code that
    /// holds `&self` and needs the shared pointer (`reactivate`'s slot, the
    /// turn-watchdog sweep).
    me: Weak<ActorSystem>,
    /// Runs at the top of `Drop`, before the system's slot is removed, so a
    /// test can claim the name at that exact point.
    #[cfg(test)]
    before_slot_removal: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once inside the registration gate, right after the move to
    /// `Defunct` took the map's strong reference out of the slot, so a test
    /// can act at that exact point.
    #[cfg(test)]
    after_demotion: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl std::fmt::Debug for ActorSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorSystem")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Drop for ActorSystem {
    /// Removes this system's own slot from the process map, exact to its
    /// generation: a later system that claimed the name after this one's last
    /// holder let go keeps its slot. The fields drop after this body: the
    /// reaper feeds close (each reaper fires what it holds and ends), the
    /// helper slots and both registries are freed.
    fn drop(&mut self) {
        #[cfg(test)]
        {
            let hook = self
                .before_slot_removal
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let generation = self.generation;
        let removed = systems().remove_if(&self.name, |_, slot| slot.generation == generation);
        // The guard is gone; the slot (a dead `Weak`) is freed here.
        drop(removed);
    }
}
impl ActorSystem {
    /// The system's telemetry gate and sinks.
    #[allow(dead_code)]
    pub(crate) fn telemetry_mode(&self) -> &Arc<crate::telemetry::Mode> {
        &self.telemetry.0
    }

    /// Draws the next incarnation number: roots, fresh child spawns and
    /// restarts alike, in every system of the process, come from the one
    /// process-wide counter behind [`Incarnation`](crate::types::Incarnation)
    /// (numbers start at 1, so `0` stands for "no incarnation"). A supervisor
    /// draws a child's number before the spawn, for its ledger; the spawn
    /// draws a root's. The same number is the record's incarnation, the
    /// registration's sequence, the turn clock's id and the wait graph's
    /// token, so no two incarnations of any child, under any name, in any
    /// system, are ever equal, and a stale runner completion (from a
    /// superseded instance, including one that shared its predecessor's name
    /// after a terminate/delete/respawn cycle) can never be mistaken for the
    /// fresh one's death.
    pub(crate) fn next_incarnation(&self) -> u64 {
        crate::types::Incarnation::mint().number()
    }

    /// Spawns a helper task of the crate on `rt` through this system's helper
    /// slot, which records the task's end.
    pub(crate) fn spawn_helper<F, Fut>(
        &self,
        rt: &Handle,
        kind: HelperKind,
        owner: Option<crate::helper::HelperOwner>,
        make: F,
    ) -> tokio::task::JoinHandle<()>
    where
        F: FnOnce(crate::helper::EndGuard) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.helpers.spawn(rt, kind, owner, make)
    }

    /// The helper tasks of this system that have started and not ended.
    #[cfg(test)]
    pub(crate) fn live_helpers(&self) -> usize {
        self.helpers.live()
    }

    /// The helper slot of `rt`, if this system has started a helper there.
    #[cfg(test)]
    pub(crate) fn helper_slot_of(&self, rt: &Handle) -> Option<Arc<crate::helper::HelperSlot>> {
        self.helpers.slot_of(rt)
    }

    /// The ends the helper slot of `rt` has recorded, in order.
    #[cfg(test)]
    pub(crate) fn helper_ends(&self, rt: &Handle) -> Vec<(HelperKind, crate::helper::HelperEnd)> {
        self.helpers.ends(rt)
    }
}

/// How many runtimes without the time driver a system remembers before it
/// starts its memo over.
#[cfg(feature = "telemetry")]
const TIMERLESS_MEMO_LIMIT: usize = 64;

/// The key a monitor target is looked up by.
pub(crate) enum MonitorKey<'a> {
    /// An actor id.
    Id(&'a ActorId),
    /// A registered name.
    Name(&'a str),
}

#[cfg(test)]
thread_local! {
    static AFTER_RAISE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}
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

// The parts of this module, in the order the generated docs list their methods.

mod process_map;
use self::process_map::*;

mod actors;
use self::actors::*;

mod lifecycle;

#[cfg(feature = "telemetry")]
mod monitoring;

mod shutdown;
pub(crate) use self::shutdown::*;

mod registry;
pub(crate) use self::registry::*;

mod kill;
pub(crate) use self::kill::*;

#[cfg_attr(not(feature = "telemetry"), path = "system/off/sweep.rs")]
mod sweep;
#[cfg(feature = "telemetry")]
use self::sweep::*;

mod reaper;
pub(crate) use self::reaper::*;

// ---------------------------------------------------------------------------
// Unit tests (pub(crate) internals, Rust Book Ch 11.3)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod cause_tests;

#[cfg(test)]
mod duplicate_id_registration_tests;

#[cfg(test)]
mod kill_raiser_tests;

#[cfg(test)]
mod observer_pairing_tests;

#[cfg(test)]
mod process_map_tests;

#[cfg(test)]
mod refused_raiser_tests;

#[cfg(test)]
mod shutdown_walk_tests;

#[cfg(all(test, feature = "tracing"))]
#[cfg(feature = "telemetry")]
mod sweep_tick_tests;

#[cfg(all(test, feature = "tracing"))]
#[cfg(feature = "telemetry")]
mod sweep_identity_tests;

#[cfg(test)]
mod tests;
