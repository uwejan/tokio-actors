//! What a spawn tells the mailbox and the run loop about the runtime the
//! actor runs on: how many worker threads it has and whether it is a
//! current-thread runtime. Both are read once, at the spawn, from the handle
//! the task is spawned on, so a child placed on another runtime gets the facts
//! of that runtime and not of its parent's, and a restart lands with the same
//! facts. The two are separate facts: a multi-thread runtime with one worker
//! has one worker and is not a current-thread runtime.
