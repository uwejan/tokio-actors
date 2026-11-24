//! The event pump in a build without the `telemetry` feature: there is no
//! queue and no helper, because no sink can be attached to take an event.

use std::sync::Arc;

use super::event::{EventKind, TelemetryEvent};
use super::identity::Identity;
use super::mode::Mode;

/// Stands where the pump stands in a build with the feature. No emit site is
/// ever armed in this build, so none of these is reached with an event.
#[derive(Debug, Default)]
pub(crate) struct Pump(());

impl Pump {
    pub(crate) fn enqueue(
        &self,
        _mode: &Arc<Mode>,
        _kind: EventKind,
        _event: TelemetryEvent<'static>,
    ) {
    }

    pub(crate) fn enqueue_for(
        &self,
        _mode: &Arc<Mode>,
        _kind: EventKind,
        _event: TelemetryEvent<'static>,
        _owner: Option<Arc<Identity>>,
    ) {
    }

    pub(crate) fn drop_event(
        &self,
        _mode: &Mode,
        _kind: EventKind,
        _event: &TelemetryEvent<'static>,
    ) {
    }

    pub(crate) fn discard_queued(&self, _mode: &Mode) {}

    pub(crate) fn count_dropped(&self) {}

    pub(crate) fn close(&self) {}
}
