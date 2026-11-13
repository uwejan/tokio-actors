//! The caller token of the wait graph: which actor's hook the current thread
//! is polling.

use std::cell::Cell;

thread_local! {
    /// The token of the actor whose hook the thread polls now, or 0.
    static CALLER: Cell<u64> = const { Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// The reads of the thread's token this thread has made.
    static READS: Cell<u64> = const { Cell::new(0) };
}

/// How many times this thread has read its token through [`current`], for
/// tests that assert a path reads none.
#[cfg(test)]
pub(crate) fn reads_this_thread() -> u64 {
    READS.with(Cell::get)
}

/// The token of the hook being polled on this thread, or 0 when none is.
pub(crate) fn current() -> u64 {
    #[cfg(test)]
    READS.with(|reads| reads.set(reads.get() + 1));
    CALLER.try_with(Cell::get).unwrap_or(0)
}

/// The token of the hook being polled on this thread, or 0 when none is: one
/// load of a plain thread-local, with no registration and no call.
#[inline(always)]
pub(crate) fn peek() -> u64 {
    CALLER.try_with(Cell::get).unwrap_or(0)
}

/// Sets the thread's token to `token` until the guard drops, which puts back
/// the value found, also on an unwind.
pub(crate) fn enter(token: u64) -> TokenGuard {
    let previous = CALLER.try_with(|caller| caller.replace(token)).unwrap_or(0);
    TokenGuard { previous }
}

/// Restores the token the thread held before [`enter`].
#[derive(Debug)]
pub(crate) struct TokenGuard {
    previous: u64,
}

impl Drop for TokenGuard {
    fn drop(&mut self) {
        let _ = CALLER.try_with(|caller| caller.set(self.previous));
    }
}

/// Runs `f` with the thread's token set to `token`.
#[cfg(test)]
pub(crate) fn with<R>(token: u64, f: impl FnOnce() -> R) -> R {
    let _guard = enter(token);
    f()
}
