//! Suspension: the loop stops taking everything but the stop lane and the
//! system channel.
//!
//! A suspended actor takes no message from its mailbox or its priority lane
//! and, when it supervises, handles no child death, restart outcome, verdict
//! or forwarder end. All of those wait in their queues and are taken in the
//! select's order after [`resume`]. It still reads its stop lane (a stop
//! request runs `pre_stop` and a `Kill` ends it) and its system channel
//! (`get_status` and every `sys` call), and the stop of a supervisor runs
//! from the stop path, not from a branch the suspension disables.
//!
//! The flag is one bit of the incarnation's block, written here and read
//! once per pass of the loop, so an incarnation that is restarted starts
//! running. No wait of the crate depends on the flag: the caller that
//! suspends owns the suspension.

use super::sys_loop::LoopSys;
use crate::actor::panic::send_contained;
use crate::types::Reply;

/// Sets the suspension and answers. A second call changes nothing.
pub(crate) fn suspend(sys: &mut LoopSys<'_>, reply: Reply<()>) {
    sys.set_suspended(true);
    let _ = send_contained(reply, ());
}

/// Clears the suspension and answers. A call on a running actor changes
/// nothing.
pub(crate) fn resume(sys: &mut LoopSys<'_>, reply: Reply<()>) {
    sys.set_suspended(false);
    let _ = send_contained(reply, ());
}
