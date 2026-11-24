//! Handlers in a build without the `telemetry` feature: none can be attached.

use super::mode::Mode;

/// Whether the calling thread is running a handler of `mode`'s system: no
/// handler exists in this build.
pub(crate) fn inside_handler_of(_mode: &Mode) -> bool {
    false
}
