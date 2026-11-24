//! A system's phase: reading it and bringing a shut-down system back.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::{systems, ActorSystem, SystemPhase, DEFAULT_SYSTEM};

impl ActorSystem {
    /// Moves a `Defunct` system (one with no `shutdown`/`shutdown_with` in
    /// flight, after a shutdown completed) back to `Active`, so its name can
    /// be registered into again instead of staying poisoned forever.
    ///
    /// Returns `false` without effect if the system is not currently
    /// `Defunct` - either still `Active` (nothing to reactivate), or
    /// `ShuttingDown` - or if any shutdown is still in flight: reactivating
    /// out from under one would let a registration land after that
    /// shutdown's last snapshot, neither stopped nor refused. A system left
    /// `ShuttingDown` by shutdowns that were all cut short reaches `Defunct`
    /// by calling [`shutdown`](Self::shutdown) again and letting it complete;
    /// once a shutdown has completed since the system was last `Active`, the
    /// system is `Defunct` whenever no shutdown is in flight, however later
    /// calls ended. A reactivated system is held by the process map of named
    /// systems again, as a new system is, until a shutdown of it completes
    /// (see [`create`](Self::create)).
    pub fn reactivate(&self) -> bool {
        // Taken before the gate and the map's shard: the slot takes this
        // reference if it needs it, and what it does not take is dropped
        // after both are released.
        let mut me = self.me.upgrade();
        {
            let in_flight = self
                .registration_gate
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *in_flight != 0
                || SystemPhase::from_u8(self.phase.load(Ordering::Acquire)) != SystemPhase::Defunct
            {
                return false;
            }
            self.completed_since_active.store(false, Ordering::Relaxed);
            self.phase
                .store(SystemPhase::Active.as_u8(), Ordering::Release);
            // The map holds the system strongly again, in the same section
            // that clears the flag.
            self.promote_slot(&mut me);
        }
        drop(me);
        true
    }

    /// Puts the strong reference back into this system's slot of the process
    /// map, taking it from `me`. Called under the registration gate's
    /// exclusive side, in the section that clears `completed_since_active`.
    /// A system whose slot the map does not hold (another generation holds the
    /// name, or none) leaves `me` untouched. The caller drops what is left in
    /// `me` after the gate.
    fn promote_slot(&self, me: &mut Option<Arc<ActorSystem>>) {
        if let Some(mut slot) = systems().get_mut(&self.name) {
            if slot.generation == self.generation && slot.strong.is_none() {
                slot.strong = me.take();
            }
        }
    }

    /// Takes the strong reference out of this system's slot of the process
    /// map, so the map holds the system only weakly, and returns it for the
    /// caller to drop after the gate and the map's shard are released. Called
    /// under the registration gate's exclusive side, in the section that
    /// stores `Defunct`. The `"default"` system is never demoted, and neither
    /// is a system whose slot another generation holds.
    pub(super) fn demote_slot(&self) -> Option<Arc<ActorSystem>> {
        if self.name == DEFAULT_SYSTEM {
            return None;
        }
        let mut slot = systems().get_mut(&self.name)?;
        if slot.generation != self.generation {
            return None;
        }
        slot.strong.take()
    }

    /// Whether no actor is registered in the system.
    pub(crate) fn holds_no_actor(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Whether the system is `Active`, the one phase that takes registrations.
    pub(crate) fn is_active(&self) -> bool {
        SystemPhase::from_u8(self.phase.load(Ordering::Acquire)) == SystemPhase::Active
    }

    /// Whether the system is `Defunct`: a shutdown completed and none is in
    /// flight.
    pub(crate) fn is_defunct(&self) -> bool {
        SystemPhase::from_u8(self.phase.load(Ordering::Acquire)) == SystemPhase::Defunct
    }
}
