//! The per-loop handler cache in a build without the `telemetry` feature: no
//! handler can be attached, so the cache holds nothing and an emit on the
//! path of an actor's own task does nothing.

use std::sync::Arc;

use super::event::{EventKind, TelemetryEvent};
use super::identity::Identity;
use super::mode::Mode;

/// Stands where the handler cache of a run loop stands in a build with the
/// feature.
#[derive(Debug)]
pub(crate) struct HotCache(());

impl HotCache {
    pub(crate) fn new() -> Self {
        Self(())
    }

    pub(super) fn take_identity(&mut self) -> Option<Arc<Identity>> {
        None
    }

    pub(super) fn keep_identity(&mut self, _identity: Arc<Identity>) {}
}

impl Drop for HotCache {
    /// The run loop drops its cache at a set point of its end: here that gives nothing back.
    fn drop(&mut self) {}
}

impl Mode {
    /// Delivers `event` to the handlers of the cache: there are none.
    pub(crate) fn emit_cached(
        self: &Arc<Self>,
        _cache: &mut HotCache,
        _kind: EventKind,
        _event: &TelemetryEvent<'_>,
    ) {
    }
}
