//! The key of the actor that is running on this thread.
//!
//! A call into an actor from the actor's own task can never be answered: the
//! task is busy running the callback that made the call. Erlang/OTP refuses
//! the same call with `calling_self` (`gen.erl:231`).
//!
//! The run loop of every actor records, at the start of each poll of its
//! task, two numbers in a thread-local: the key of the incarnation that is
//! polled (its public [`Incarnation`](crate::types::Incarnation), unique in
//! the process and never reused) and the id of the tokio task that is polling
//! it. The task never changes during an incarnation, so the future that
//! carries the incarnation reads the id once and copies it into every poll's
//! record. A guard puts the previous record back when the poll ends, whether it
//! returns, is cut short by an abort or unwinds, so a record is never left
//! behind and a poll nested inside another one (an actor driven from inside
//! another actor's callback) restores its outer record.
//!
//! A call refuses only when both numbers match the caller: the target's key
//! is the key being polled on this thread and the task the caller runs in is
//! the task that is polling it. Nothing is stored per spawn, and the record
//! costs the other callers one load of the thread-local.
//!
//! The task id is checked as well as the key because the record belongs to
//! the poll, not to the thread: a plain future that the callback drives to
//! completion on the same thread (`block_on` of a current-thread runtime
//! inside `block_in_place`) runs inside the actor's poll, and the tasks of
//! that runtime run there under ids of their own. Those are other tasks, so
//! they are never refused; if the actor is blocked on one of them, because its
//! thread is parked in the callback's `block_on`, the call still waits for an
//! answer, as the docs of `send` say. A future the
//! callback polls itself (a `join!`, a `select!`, `futures::executor::block_on`)
//! runs in the actor's task and is refused, as it must be: the actor is
//! blocked on it.
//!
//! A key is not a reused address, so a call made after the incarnation ended
//! (or after a later incarnation took its place) matches nothing: the record
//! is gone with the poll that set it, and an incarnation number is drawn once.
//! A task id that tokio gives to a later task cannot match either, since the
//! record that held it is gone before the task ends.
//!
//! The record is read and written by one thread, so no ordering is involved.

use std::cell::Cell;
use std::marker::PhantomData;

use tokio::task::Id;

use crate::types::Incarnation;

/// What one thread is polling right now: the key of the incarnation and the
/// id of the task that polls it. A key of 0 means none; keys start at 1.
#[derive(Clone, Copy)]
struct Running {
    key: u64,
    task: Option<Id>,
}

const NOTHING: Running = Running { key: 0, task: None };

thread_local! {
    static RUNNING: Cell<Running> = const { Cell::new(NOTHING) };
}

/// The record of one poll. Dropping it puts back the record that was in
/// place before it, on every path out of the poll.
///
/// The guard belongs to the thread that made it, so it is not `Send`: moved
/// to another thread it would put a record back where it was never taken.
pub(crate) struct PollScope {
    outer: Running,
    _thread: PhantomData<*const ()>,
}

impl PollScope {
    /// Records `incarnation` as the one polled on this thread, by `task`:
    /// the id of the task that runs this call (`None` outside any task).
    #[inline]
    pub(crate) fn enter(incarnation: Incarnation, task: Option<Id>) -> Self {
        let now = Running {
            key: incarnation.number(),
            task,
        };
        Self {
            outer: RUNNING.replace(now),
            _thread: PhantomData,
        }
    }
}

impl Drop for PollScope {
    #[inline]
    fn drop(&mut self) {
        RUNNING.set(self.outer);
    }
}

/// Whether the call is made from the task that is polling `incarnation`, so a
/// request it sends to that incarnation could never be answered.
#[inline]
pub(crate) fn is_self_call(incarnation: Incarnation) -> bool {
    let running = RUNNING.get();
    if running.key != incarnation.number() {
        return false;
    }
    match running.task {
        Some(polling) => tokio::task::try_id() == Some(polling),
        None => false,
    }
}

/// The record in place now, for the tests.
#[cfg(test)]
pub(crate) fn current_for_test() -> (u64, Option<Id>) {
    let running = RUNNING.get();
    (running.key, running.task)
}
