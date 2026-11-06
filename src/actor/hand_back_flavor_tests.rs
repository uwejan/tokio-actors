//! Which status requesters the run loop hands the thread back for. A
//! requester that is a runtime task is handed back for on any runtime. A
//! requester that is not a task (a body driven by `block_on`) is handed back
//! for on a current-thread runtime, where it runs on the actor's own thread,
//! and not on a multi-thread runtime, where it is woken from another thread.
