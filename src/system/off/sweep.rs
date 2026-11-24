//! The sweep in a build without the `telemetry` feature: there is no turn
//! watchdog and no system monitor, so no sweep task is ever started.

use super::ActorSystem;

impl ActorSystem {
    /// Starts the sweep task if something needs it: nothing does.
    pub(crate) fn ensure_sweep(&self) {}
}
