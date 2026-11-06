//! The actor's task answers the ack of its spawn from its own run. The
//! caller's waker belongs to the caller's task: when the ack is a failed
//! initialization and the wake panics, the panic stays in the answer, so the
//! task ends by its own return with the reason the initialization failed
//! with, and not by an unwind past every catch. The same holds when a `Kill`
//! cuts the initialization and the ack is dropped unanswered: the task ends as
//! `Kill`.
//!
//! It holds too when the task's own poll unwinds from a panic of the crate's
//! own code, which drops the start ack, the answer held for a failed request
//! and the sender of the request in hand as part of the unwind. A panic of a
//! caller's waker raised there would be a second panic in an unwind and would
//! end the process, so each sender is released inside a catch, and the task
//! ends with the reason an unwind carries. Those scenarios run in a child
//! process of the test binary, where the process ending fails one test.
