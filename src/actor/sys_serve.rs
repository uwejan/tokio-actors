//! The one function that serves a `sys` command on the actor's task, at a
//! turn boundary.

use std::ops::ControlFlow;

use super::context::ActorContext;
use super::panic::contain_waker_panic;
#[cfg(test)]
use super::panic::send_contained;
use super::sys_debug;
use super::sys_state;
use super::sys_suspend;
use super::Actor;
use crate::types::{ActorId, StopReason, SysCommand, TurnClock};

/// Serves `command` on the actor's task, between two turns. The bundle's
/// state (the suspension bit and the debug state) is reached through
/// `ctx.sys_loop()`, which the loop does not hold between calls.
///
/// A command whose caller has gone (its reply receiver is dropped: the call
/// timed out or was cancelled) is dropped unrun if it has not begun. A
/// command the actor has begun to serve is not undone: a call that answers
/// `Timeout` may have been applied, and a state closure that is running when
/// the caller's bound passes runs to its end.
pub(crate) fn serve<A: Actor>(
    command: SysCommand,
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    clock: &TurnClock,
) -> ControlFlow<StopReason> {
    if caller_gone(&command) {
        drop_unrun(command, ctx.actor_id());
        return ControlFlow::Continue(());
    }
    #[cfg(test)]
    served::bump();
    match command {
        SysCommand::Suspend(reply) => sys_suspend::suspend(&mut ctx.sys_loop(), reply),
        SysCommand::Resume(reply) => sys_suspend::resume(&mut ctx.sys_loop(), reply),
        SysCommand::SetStatistics(on, reply) => {
            sys_debug::set_statistics(&mut ctx.sys_loop(), on, reply);
        }
        SysCommand::Statistics(reply) => sys_debug::statistics(&mut ctx.sys_loop(), reply),
        SysCommand::SetLog(capacity, reply) => {
            sys_debug::set_log(&mut ctx.sys_loop(), capacity, reply);
        }
        SysCommand::Log(reply) => sys_debug::log(&mut ctx.sys_loop(), reply),
        SysCommand::SetTrace(on, reply) => sys_debug::set_trace(&mut ctx.sys_loop(), on, reply),
        SysCommand::Install(id, f, reply) => {
            let owner = ctx.owner();
            sys_debug::install(&mut ctx.sys_loop(), owner, id, f, reply);
        }
        SysCommand::Remove(id, reply) => sys_debug::remove(&mut ctx.sys_loop(), id, reply),
        SysCommand::State(op) => return sys_state::run(op, actor, ctx, clock),
        #[cfg(test)]
        SysCommand::Probe(runs, reply) => {
            runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = send_contained(reply, ());
        }
    }
    ControlFlow::Continue(())
}

/// Whether nobody waits for the command's reply any more.
fn caller_gone(command: &SysCommand) -> bool {
    match command {
        SysCommand::Suspend(reply)
        | SysCommand::Resume(reply)
        | SysCommand::SetStatistics(_, reply)
        | SysCommand::SetLog(_, reply)
        | SysCommand::SetTrace(_, reply)
        | SysCommand::Install(_, _, reply) => reply.is_closed(),
        SysCommand::Statistics(reply) => reply.is_closed(),
        SysCommand::Log(reply) => reply.is_closed(),
        SysCommand::Remove(_, reply) => reply.is_closed(),
        SysCommand::State(op) => op.caller_gone(),
        #[cfg(test)]
        SysCommand::Probe(_, reply) => reply.is_closed(),
    }
}

/// Drops a command that is not run. It holds closures the caller gave
/// (an installed function, a state operation) whose captured values may
/// panic when dropped, so the drop is contained.
fn drop_unrun(command: SysCommand, owner: &ActorId) {
    contain_waker_panic("sys request", Some(owner.as_who()), move || drop(command));
}

/// How many commands the run loops have served, for the tests of the calls.
#[cfg(test)]
pub(crate) fn served_count() -> usize {
    served::count()
}

#[cfg(test)]
mod served {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SERVED: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn bump() {
        SERVED.fetch_add(1, Ordering::SeqCst);
    }

    pub(super) fn count() -> usize {
        SERVED.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
#[path = "sys_requests_tests.rs"]
mod requests;
