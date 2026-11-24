//! Wait-cycle detection in a build without the `telemetry` feature: no send
//! is recorded, because the detection cannot be switched on.

use std::marker::PhantomData;

use crate::actor::incarnation::IncarnationRef;

/// Stands where the graph of waiting sends stands in a build with the
/// feature.
#[derive(Debug, Default)]
pub(crate) struct WaitGraph(());

/// Stands where the record of one waiting send stands in a build with the
/// feature; none is ever made.
pub(crate) struct WaitEdge<'a>(PhantomData<&'a ()>);

/// Whether a send that waits on `record`'s actor is to be recorded: never.
#[inline(always)]
pub(crate) fn wait_wanted(_record: &IncarnationRef) -> bool {
    false
}

/// Records a send that waits on `record`'s actor: nothing is recorded.
#[inline(always)]
pub(crate) fn begin_wait(_record: &IncarnationRef) -> Option<WaitEdge<'_>> {
    None
}
