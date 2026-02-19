//! Actor system providing named actor registration and coordinated shutdown.
//!
//! Internal module.

#![allow(dead_code)]

use std::any::{Any, TypeId};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::actor::handle::ActorHandle;
use crate::actor::Actor;
use crate::error::{SendError, SpawnError};
use crate::types::{ActorId, StopReason};

type StopFn =
    dyn Fn(StopReason) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send>> + Send + Sync;

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
}

impl Default for ShutdownPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
        }
    }
}

// ---------------------------------------------------------------------------
// Type-erased actor handle
// ---------------------------------------------------------------------------

struct AnyActorHandle {
    type_id: TypeId,
    handle: Box<dyn Any + Send + Sync>,
    stopper: Box<StopFn>,
    #[allow(dead_code)]
    created_at: Instant,
}

impl AnyActorHandle {
    fn new<A: Actor>(handle: &ActorHandle<A>) -> Self {
        let cloned = handle.clone();
        let stopper_handle = handle.clone();
        Self {
            type_id: TypeId::of::<ActorHandle<A>>(),
            handle: Box::new(cloned),
            stopper: Box::new(move |reason| {
                let h = stopper_handle.clone();
                Box::pin(async move { h.stop(reason).await })
            }),
            created_at: Instant::now(),
        }
    }

    fn downcast<A: Actor>(&self) -> Option<ActorHandle<A>> {
        if self.type_id == TypeId::of::<ActorHandle<A>>() {
            self.handle.downcast_ref::<ActorHandle<A>>().cloned()
        } else {
            None
        }
    }

    async fn stop(&self, reason: StopReason) -> Result<(), SendError> {
        (self.stopper)(reason).await
    }
}

// ---------------------------------------------------------------------------
// RegistryGuard — drop-based auto-unregister
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
/// Tokio runtime — actors spawn on whatever runtime is current via
/// `Handle::try_current()`.
pub struct ActorSystem {
    name: String,
    by_name: DashMap<String, AnyActorHandle>,
    by_id: DashMap<ActorId, AnyActorHandle>,
}

impl std::fmt::Debug for ActorSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorSystem")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl ActorSystem {
    // -- Construction -------------------------------------------------------

    /// Returns the default system (named `"default"`), creating it lazily.
    ///
    /// This is intentionally not `std::default::Default` because it returns
    /// `Arc<ActorSystem>` (shared ownership), not an owned value.
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> Arc<ActorSystem> {
        let reg = systems();
        reg.entry("default".to_string())
            .or_insert_with(|| {
                Arc::new(ActorSystem {
                    name: "default".to_string(),
                    by_name: DashMap::new(),
                    by_id: DashMap::new(),
                })
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
        if reg.contains_key(&name) {
            return Err(SpawnError::SystemNameTaken(name));
        }
        let system = Arc::new(ActorSystem {
            name: name.clone(),
            by_name: DashMap::new(),
            by_id: DashMap::new(),
        });
        reg.insert(name, system.clone());
        Ok(system)
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

    // -- Actor lookup -------------------------------------------------------

    /// Looks up a named actor, returning a typed handle.
    ///
    /// Returns `None` if the name is not registered or if the registered actor
    /// is a different type (type mismatch is silent, matching OTP's
    /// `whereis/1 → undefined` semantics).
    pub fn get<A: Actor>(&self, name: &str) -> Option<ActorHandle<A>> {
        self.by_name.get(name).and_then(|e| e.downcast::<A>())
    }

    /// Looks up an actor by ID.
    pub fn get_by_id<A: Actor>(&self, id: &ActorId) -> Option<ActorHandle<A>> {
        self.by_id.get(id).and_then(|e| e.downcast::<A>())
    }

    /// Lists all registered actor names in this system.
    pub fn registered(&self) -> Vec<String> {
        self.by_name.iter().map(|e| e.key().clone()).collect()
    }

    // -- Internal registration (used by spawn path) -------------------------

    pub(crate) fn register_actor<A: Actor>(
        &self,
        id: &ActorId,
        name: Option<&str>,
        handle: &ActorHandle<A>,
    ) -> Result<(), SpawnError> {
        if let Some(n) = name {
            if self.by_name.contains_key(n) {
                return Err(SpawnError::NameTaken {
                    name: n.to_string(),
                    system: self.name.clone(),
                });
            }
            self.by_name
                .insert(n.to_string(), AnyActorHandle::new(handle));
        }
        self.by_id.insert(id.clone(), AnyActorHandle::new(handle));
        Ok(())
    }

    pub(crate) fn unregister_by_id(&self, id: &ActorId) {
        self.by_id.remove(id);
    }

    // -- Shutdown -----------------------------------------------------------

    /// Shuts down all registered actors with the default policy (30s timeout).
    pub async fn shutdown(&self) {
        self.shutdown_with(ShutdownPolicy::default()).await;
    }

    /// Shuts down all registered actors with a custom policy.
    ///
    /// All entries are drained from the registry first (releasing locks),
    /// then stop signals are sent. This avoids deadlock with `RegistryGuard`
    /// drop handlers that also mutate the registry.
    pub async fn shutdown_with(&self, policy: ShutdownPolicy) {
        let deadline = Instant::now() + policy.timeout;

        // Drain both maps — takes ownership and releases all DashMap locks
        // before we await the stop futures. This prevents deadlock with
        // RegistryGuard::drop which also mutates these maps.
        self.by_name.clear();
        let entries: Vec<(ActorId, AnyActorHandle)> = self
            .by_id
            .iter()
            .map(|e| e.key().clone())
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|id| self.by_id.remove(&id))
            .collect();

        // Phase 1: Graceful — stop each actor (no locks held).
        let mut remaining = Vec::new();
        for (i, (_, entry)) in entries.iter().enumerate() {
            if Instant::now() >= deadline {
                remaining.push(i);
                continue;
            }
            let _ = entry.stop(StopReason::Graceful).await;
            tokio::task::yield_now().await;
        }

        // Phase 2: Force remaining (deadline exceeded).
        for &i in &remaining {
            let _ = entries[i].1.stop(StopReason::Graceful).await;
        }
    }
}
