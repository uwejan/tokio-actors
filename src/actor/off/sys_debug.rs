//! The debug state of an actor in a build without the `sys` feature: none can
//! be made, because no debug option can be set and no `sys` call can be sent.

use crate::actor::sys::{DebugEntry, DebugOptions};
use crate::types::StopReason;

/// Stands where an actor's debug state stands in a build with the feature.
/// It has no value, so the code that holds one in an `Option` always holds
/// `None`, and nothing below can be reached.
#[derive(Debug)]
pub(crate) enum DebugState {}

impl DebugState {
    /// The debug state `options` ask for: none in this build.
    pub(crate) fn from_options(_options: DebugOptions) -> Option<Self> {
        None
    }

    pub(crate) fn ring_entries(&self) -> Option<&[DebugEntry]> {
        match *self {}
    }

    pub(crate) fn message_in(&mut self, _reply_expected: bool, _priority: bool) {
        match *self {}
    }

    pub(crate) fn message_out(&mut self, _delivered: bool) {
        match *self {}
    }

    pub(crate) fn stopping(&mut self, _reason: &StopReason) {
        match *self {}
    }

    pub(crate) fn is_tracing(&self) -> bool {
        match *self {}
    }

    pub(crate) fn take_traced(&mut self) -> [Option<DebugEntry>; 2] {
        match *self {}
    }
}
