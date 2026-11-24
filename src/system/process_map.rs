//! The process-wide map of named systems and the constructors of a system.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::watch;
use tokio::time::Instant;

use super::{
    registry_map, threshold_to_nanos, ActorSystem, ShutdownPolicy, SystemConfig, SystemPhase,
};
use crate::error::SpawnError;

// ---------------------------------------------------------------------------
// Global systems registry
// ---------------------------------------------------------------------------

/// The name of the ambient system that bare spawns join. Its slot in the
/// process map is always strong.
pub(super) const DEFAULT_SYSTEM: &str = "default";

static SYSTEMS: OnceLock<DashMap<String, Slot>> = OnceLock::new();

pub(super) fn systems() -> &'static DashMap<String, Slot> {
    SYSTEMS.get_or_init(DashMap::new)
}

/// The source of every system's generation. Generations start at 1: `0`
/// stands for "no system" in a handle no spawn made.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// What the process map holds for one name.
///
/// The slot holds its system strongly from the system's creation until the
/// shutdown that completes it (the one that stores `Defunct`), and weakly from
/// then until [`ActorSystem::reactivate`] puts the strong reference back. The
/// ambient `"default"` system is never demoted. A weakly held system leaves
/// the map when its last holder lets go (see `Drop for ActorSystem`).
///
/// No `Arc` is created while a shard guard of the map is held unless it
/// escapes by return, and none is dropped under one: a system's drop removes
/// its own slot, which takes the same shard lock. Liveness under a guard is
/// read from the counts (`is_live`), and a `Weak` is upgraded only after the
/// guard has ended.
pub(super) struct Slot {
    /// The generation of the system that owns this slot.
    pub(super) generation: u64,
    weak: Weak<ActorSystem>,
    pub(super) strong: Option<Arc<ActorSystem>>,
}

impl Slot {
    fn new(system: &Arc<ActorSystem>) -> Self {
        Self {
            generation: system.generation,
            weak: Arc::downgrade(system),
            strong: Some(system.clone()),
        }
    }

    /// Whether the system can still be reached. A count of zero is final; a
    /// stale positive count only makes a caller see a system that has just
    /// died, which is a legal order.
    fn is_live(&self) -> bool {
        self.strong.is_some() || self.weak.strong_count() > 0
    }
}

impl ActorSystem {
    // - Construction -------------------------------------------------------

    fn empty(
        name: String,
        shutdown_policy: ShutdownPolicy,
        turn_warning_threshold: Option<Duration>,
        turn_tracking: bool,
        wait_cycle_detection: bool,
        me: Weak<ActorSystem>,
    ) -> Self {
        #[cfg(all(feature = "tracing", panic = "abort"))]
        {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                caught_log!(warn, target: "tokio_actors::system",
                    "panic = \"abort\": actor panics terminate the process; panic capture and crash-visible supervision are unavailable"
                );
            });
        }

        let telemetry = crate::telemetry::ModeOwner::bound(me.clone(), &name);
        telemetry.0.set_ctl(
            crate::telemetry::CTL_WATCHDOG,
            threshold_to_nanos(turn_warning_threshold) != 0,
        );
        telemetry
            .0
            .set_ctl(crate::telemetry::CTL_TRACK_TURNS, turn_tracking);
        telemetry
            .0
            .set_ctl(crate::telemetry::CTL_DEADLOCK, wait_cycle_detection);
        // The registry is empty: a switch turned on here owes no pass.
        telemetry.0.mark_bound();
        Self {
            name,
            by_id: registry_map(),
            shutdown_policy,
            helpers: crate::helper::HelperSlots::new(telemetry.0.clone()),
            telemetry,
            phase: AtomicU8::new(SystemPhase::Active.as_u8()),
            registration_gate: std::sync::RwLock::new(0),
            completed_since_active: AtomicBool::new(false),
            pg_scopes: Mutex::new(HashMap::new()),
            #[cfg(test)]
            pg_sweeps: AtomicU64::new(0),
            reaper: Mutex::new(HashMap::new()),
            turn_warning_threshold_nanos: Arc::new(AtomicU64::new(threshold_to_nanos(
                turn_warning_threshold,
            ))),
            #[cfg(test)]
            after_kill_lookup: Mutex::new(None),
            #[cfg(test)]
            after_kill_raise: Mutex::new(None),
            #[cfg(test)]
            after_kill_declaration: Mutex::new(None),
            #[cfg(test)]
            ladder_hook: Mutex::new(None),
            #[cfg(test)]
            backstop_arms: AtomicU64::new(0),
            #[cfg(test)]
            after_phase_check: Mutex::new(None),
            #[cfg(test)]
            between_quiet_passes: Mutex::new(None),
            #[cfg(test)]
            after_snapshot_walk: Mutex::new(None),
            #[cfg(test)]
            identity_walks: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            before_sweep_latch_clear: Mutex::new(None),
            #[cfg(test)]
            root_snapshots_done: AtomicBool::new(false),
            #[cfg(test)]
            shutdown_at_the_gate: AtomicBool::new(false),
            sweep_spawned: Arc::new(AtomicBool::new(false)),
            timerless_runtimes: Mutex::new(std::collections::HashSet::new()),
            sweep_deadline: watch::channel(None).0,
            epoch: Instant::now(),
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            me,
            #[cfg(test)]
            before_slot_removal: Mutex::new(None),
            #[cfg(test)]
            after_demotion: Mutex::new(None),
        }
    }

    /// A new system in its own allocation, which knows its own `Weak`.
    fn build(
        name: String,
        shutdown_policy: ShutdownPolicy,
        turn_warning_threshold: Option<Duration>,
        turn_tracking: bool,
        wait_cycle_detection: bool,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| {
            Self::empty(
                name,
                shutdown_policy,
                turn_warning_threshold,
                turn_tracking,
                wait_cycle_detection,
                me.clone(),
            )
        })
    }

    /// Puts a new system under `name` in the process map, if the name is free
    /// or its slot is dead. `Err` carries the live occupant when the map holds
    /// it strongly (and `None` when it holds it only weakly), and nothing is
    /// built then.
    ///
    /// The new system is built under the entry's lock and escapes by return;
    /// a dead slot it replaces holds no live `Arc`, so dropping it under the
    /// lock drops nothing that takes it.
    fn claim(
        name: String,
        build: impl FnOnce(String) -> Arc<ActorSystem>,
    ) -> Result<Arc<ActorSystem>, Option<Arc<ActorSystem>>> {
        use dashmap::mapref::entry::Entry;
        match systems().entry(name.clone()) {
            Entry::Vacant(v) => {
                let system = build(name);
                v.insert(Slot::new(&system));
                Ok(system)
            }
            Entry::Occupied(mut o) => {
                if o.get().is_live() {
                    return Err(o.get().strong.clone());
                }
                let system = build(name);
                let dead = o.insert(Slot::new(&system));
                debug_assert!(dead.strong.is_none() && dead.weak.strong_count() == 0);
                drop(dead);
                Ok(system)
            }
        }
    }

    /// Returns the default system (named `"default"`), creating it lazily.
    ///
    /// The default system is the one ambient system bare spawns join, and it
    /// stays in the process map of named systems for the life of the process,
    /// shut down or not.
    ///
    /// This is intentionally not `std::default::Default` because it returns
    /// `Arc<ActorSystem>` (shared ownership), not an owned value.
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> Arc<ActorSystem> {
        // The ambient system never leaves the map of named systems, and its
        // slot is always strong, so the system the map answers once is the
        // answer for the life of the process. Kept here, a bare spawn skips
        // the hash of the name and the map's shard lock.
        static AMBIENT: OnceLock<Arc<ActorSystem>> = OnceLock::new();
        if let Some(system) = AMBIENT.get() {
            return system.clone();
        }
        let system = Self::ambient();
        AMBIENT.get_or_init(|| system.clone()).clone()
    }

    /// The ambient system as the map holds it, created on first use.
    fn ambient() -> Arc<ActorSystem> {
        loop {
            if let Some(found) = Self::get_named(DEFAULT_SYSTEM) {
                return found;
            }
            let made = Self::claim(DEFAULT_SYSTEM.to_string(), |name| {
                Self::build(name, ShutdownPolicy::default(), None, false, false)
            });
            match made {
                Ok(system) | Err(Some(system)) => return system,
                // The slot is weak and its system is still alive for another
                // thread only: look again.
                Err(None) => std::thread::yield_now(),
            }
        }
    }

    /// Creates a new named system and registers it in the global systems map.
    ///
    /// A system stays in the map from its creation until a shutdown of it
    /// has completed; from then on the map holds it only as long as someone
    /// else does, and when the last holder lets go (the caller's `Arc` and
    /// every actor of the system that has not ended) the system is freed, its
    /// name leaves the map and a later `create` can claim the name again.
    /// [`reactivate`](Self::reactivate) puts a shut-down system back to
    /// staying in the map. The system named `"default"` is the exception: it
    /// stays in the map for the life of the process.
    ///
    /// Returns [`SpawnError::SystemNameTaken`] if a system with this name
    /// already exists.
    pub fn create(name: impl Into<String>) -> Result<Arc<ActorSystem>, SpawnError> {
        let name = name.into();
        Self::claim(name.clone(), |name| {
            Self::build(name, ShutdownPolicy::default(), None, false, false)
        })
        .map_err(|_| SpawnError::SystemNameTaken(name))
    }

    /// Creates a new named system with the default configuration, as
    /// [`create`](Self::create) does, for an application of its own: the
    /// system counts the live subtree holds of its actors from its creation
    /// (see [`live_holds`](Self::live_holds)).
    pub(crate) fn create_owned(name: impl Into<String>) -> Result<Arc<ActorSystem>, SpawnError> {
        let name = name.into();
        Self::claim(name.clone(), |name| {
            let system = Self::build(name, ShutdownPolicy::default(), None, false, false);
            system.telemetry.0.holds.mark_counted();
            system
        })
        .map_err(|_| SpawnError::SystemNameTaken(name))
    }

    /// The subtree holds of this system's actors that are alive, for a system
    /// made by [`create_owned`](Self::create_owned); `0` for any other.
    pub(crate) fn live_holds(&self) -> usize {
        self.telemetry.0.holds.live_holds()
    }

    /// Resolves when [`live_holds`](Self::live_holds) reads zero. It reads no
    /// clock; the caller bounds it.
    pub(crate) async fn holds_released(&self) {
        self.telemetry.0.holds.holds_released().await;
    }

    /// Creates a new named system with custom configuration.
    ///
    /// The system's life in the map of named systems is the one
    /// [`create`](Self::create) describes.
    ///
    /// Returns [`SpawnError::SystemNameTaken`] if a system with this name
    /// already exists.
    pub fn create_with(
        name: impl Into<String>,
        config: SystemConfig,
    ) -> Result<Arc<ActorSystem>, SpawnError> {
        let name = name.into();
        Self::claim(name.clone(), |name| {
            Self::build(
                name,
                config.shutdown_policy,
                config.turn_warning_threshold,
                config.turn_tracking,
                config.wait_cycle_detection,
            )
        })
        .map_err(|_| SpawnError::SystemNameTaken(name))
    }

    /// Looks up a named system. Returns `None` if no system with this name
    /// exists, which includes a system that has completed a shutdown and has
    /// no holder left (see [`create`](Self::create)).
    pub fn get_named(name: &str) -> Option<Arc<ActorSystem>> {
        // The slot's own `Arc` escapes by return. A weakly held system is
        // upgraded only after the map's guard has ended, since the upgrade
        // can make this call the last holder and the system's drop takes the
        // same shard.
        let weak = {
            let slot = systems().get(name)?;
            if let Some(strong) = &slot.strong {
                return Some(strong.clone());
            }
            slot.weak.clone()
        };
        weak.upgrade()
    }

    /// Lists the names of all registered systems: the systems of the map that
    /// can still be reached (see [`get_named`](Self::get_named)).
    pub fn all() -> Vec<String> {
        systems()
            .iter()
            .filter(|e| e.value().is_live())
            .map(|e| e.key().clone())
            .collect()
    }

    /// Returns this system's name.
    pub fn name(&self) -> &str {
        &self.name
    }
}
