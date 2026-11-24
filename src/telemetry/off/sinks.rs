//! The sinks of a system in a build without the `telemetry` feature: there
//! are none, and none can be attached.

use super::event::{EventKind, TelemetryEvent};
use super::mode::Mode;

/// Stands where the handler and subscription tables stand in a build with
/// the feature.
#[derive(Debug, Default)]
pub(crate) struct Sinks(());

impl Sinks {
    /// Delivers `event` to the sinks: there are none.
    pub(crate) fn deliver(&self, _mode: &Mode, _kind: EventKind, _event: TelemetryEvent<'static>) {}
}
