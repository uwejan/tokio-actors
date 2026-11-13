//! What the run loop reads of the `sys` bundle: the suspension bit of the
//! incarnation's block and, for a call being served, the debug state.
//!
//! The loop holds no field for the bundle. Suspension is a bit of the
//! block, read once per pass; the debug state sits in the context's extras,
//! made with the first use; a call is served through a [`LoopSys`] view that
//! borrows both for the length of the call.

use super::context::ActorContext;
use super::subtree::{AbortRecord, SYS_SENSITIVE, SYS_SUSPENDED};
use super::sys::DebugEntry;
use super::sys_debug::DebugState;
use super::Actor;

/// The bundle's state as one call sees it, borrowed from the context.
#[derive(Debug)]
pub(crate) struct LoopSys<'a> {
    record: &'a AbortRecord,
    debug: &'a mut Option<Box<DebugState>>,
}

impl<'a> LoopSys<'a> {
    pub(crate) fn new(record: &'a AbortRecord, debug: &'a mut Option<Box<DebugState>>) -> Self {
        Self { record, debug }
    }

    /// Whether the loop is suspended.
    #[inline]
    pub(crate) fn suspended(&self) -> bool {
        self.record.sys_bits() & SYS_SUSPENDED != 0
    }

    /// Sets the suspension bit.
    pub(crate) fn set_suspended(&mut self, on: bool) {
        self.record.set_sys_bit(SYS_SUSPENDED, on);
    }

    /// Whether the actor is sensitive.
    pub(crate) fn sensitive(&self) -> bool {
        self.record.sys_bits() & SYS_SENSITIVE != 0
    }

    /// The place of the debug state, which a call that turns an option on
    /// fills and a call that turns the last one off empties.
    pub(crate) fn debug_slot(&mut self) -> &mut Option<Box<DebugState>> {
        self.debug
    }

    /// The debug state, when any debug option is on.
    #[inline]
    pub(crate) fn debug_mut(&mut self) -> Option<&mut DebugState> {
        self.debug.as_deref_mut()
    }

    /// The log's entries, when the log is on.
    pub(crate) fn debug_ring(&self) -> Option<&[DebugEntry]> {
        self.debug.as_deref().and_then(DebugState::ring_entries)
    }
}

/// Whether the actor's loop is suspended: one relaxed read of the block.
#[inline]
pub(crate) fn is_suspended<A: Actor>(ctx: &ActorContext<A>) -> bool {
    ctx.stop_lane().abort_record().sys_bits() & SYS_SUSPENDED != 0
}

/// The `sys` options each run loop started with, by actor id, for the tests
/// of the options.
#[cfg(test)]
pub(crate) mod starts {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use crate::actor::sys::SysOptions;
    use crate::types::ActorId;

    static STARTS: OnceLock<Mutex<HashMap<ActorId, Vec<SysOptions>>>> = OnceLock::new();

    fn starts() -> &'static Mutex<HashMap<ActorId, Vec<SysOptions>>> {
        STARTS.get_or_init(Default::default)
    }

    pub(crate) fn record(id: &ActorId, options: SysOptions) {
        starts()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(id.clone())
            .or_default()
            .push(options);
    }

    /// The options of every run loop `id` has started, oldest first.
    pub(crate) fn of(id: &ActorId) -> Vec<SysOptions> {
        starts()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
            .unwrap_or_default()
    }
}
