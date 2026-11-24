//! The `sys` bundle: the debug and state-access calls of an actor, reached
//! through [`ActorHandle::sys`](crate::actor::handle::ActorHandle::sys).
//!
//! Every call is bounded by [`DEFAULT_SYS_TIMEOUT`] unless
//! [`Sys::timeout`] sets another bound, and answers [`SysError`] when the
//! actor cannot serve it.
//!
//! The calls need the `sys` feature, which is on by default. Without it this
//! module keeps its option and entry types, and no debug option can be set.

use std::num::NonZeroU16;
use std::time::Duration;

#[cfg(feature = "sys")]
use thiserror::Error;

#[cfg(feature = "sys")]
use super::handle::ActorHandle;
use super::Actor;
use crate::types::StopReason;

/// The bound of a `sys` call when none is set: five seconds.
///
/// Needs the `sys` feature, which is on by default.
#[cfg(feature = "sys")]
pub const DEFAULT_SYS_TIMEOUT: Duration = Duration::from_millis(5000);

/// A log capacity for callers that have no preference: ten entries. Pass it to
/// [`DebugOptions::log`] or [`Sys::set_log`]; no call applies it by itself.
pub const DEFAULT_LOG_CAPACITY: usize = 10;

/// The most entries the log keeps.
pub(crate) const MAX_LOG_CAPACITY: usize = 1024;

/// Why a `sys` call was not served.
///
/// Needs the `sys` feature, which is on by default.
#[cfg(feature = "sys")]
#[derive(Debug, Error, Clone, PartialEq)]
pub enum SysError {
    /// The actor was not spawned with state access.
    #[error("the actor does not allow state access")]
    NotAllowed,
    /// The actor is sensitive: its state is never reachable from outside.
    #[error("the actor is sensitive")]
    Sensitive,
    /// The actor ended before it served the call, with this reason.
    #[error("the actor ended: {0}")]
    ActorEnded(StopReason),
    /// The call was not served within its bound. A call the actor has not
    /// begun to serve by then is dropped unrun; one it has begun to serve is
    /// not undone, so a call that answers `Timeout` may have been applied.
    #[error("the call timed out")]
    Timeout,
    /// The closure of the call panicked, with this message.
    #[error("the closure panicked: {0}")]
    Callback(String),
    /// The caller is not on a runtime with a time driver, which the bound of
    /// a call needs.
    #[error("the runtime has no time driver")]
    NoTimeDriver,
    /// The call was made from the actor's own task, which cannot serve it
    /// while it waits for the answer. Nothing is queued.
    #[error("the call was made from the actor's own task")]
    CallingSelf,
}

/// What an actor's statistics counted since they were turned on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Statistics {
    /// The time since the statistics were turned on.
    pub elapsed: Duration,
    /// The messages taken from the mailbox or the priority lane.
    pub messages_in: u64,
    /// The replies sent to callers: the messages taken with a caller waiting
    /// whose handler returned a value. A message sent without a caller adds
    /// to `messages_in` and `turns` only.
    pub messages_out: u64,
    /// The handler turns begun, one for each message taken.
    pub turns: u64,
}

/// What the actor did, as the log records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugEvent {
    /// A message was taken.
    MessageIn {
        /// Whether a caller waits for the reply.
        reply_expected: bool,
        /// Whether the message came on the priority lane.
        priority: bool,
    },
    /// A handler returned a value to a caller that waits for the reply.
    MessageOut {
        /// Whether the reply reached a caller.
        delivered: bool,
    },
    /// The actor began to stop, for this reason.
    Stopping(StopReason),
}

/// One entry of the log: a number and what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugEntry {
    /// The entry's number, counting from one each time the debug state starts:
    /// at spawn, or at the first call that turns an option on after all were
    /// off. A restart starts at one.
    pub seq: u64,
    /// What happened.
    pub event: DebugEvent,
}

/// What an installed function asks to happen to itself after a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// Keep the function installed.
    Keep,
    /// Remove the function.
    Remove,
}

/// What an actor starts with turned on, set on a spawn builder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DebugOptions {
    pub(crate) statistics: bool,
    /// The log capacity, clamped to `1..=MAX_LOG_CAPACITY`; `None` is off.
    pub(crate) log: Option<NonZeroU16>,
    pub(crate) trace: bool,
}

impl DebugOptions {
    /// Turns the statistics on or off at spawn.
    #[must_use]
    pub fn statistics(mut self, on: bool) -> Self {
        self.statistics = on;
        self
    }

    /// Turns the log on at spawn with room for `capacity` entries, at least
    /// one and at most 1024.
    #[must_use]
    pub fn log(mut self, capacity: usize) -> Self {
        self.log = NonZeroU16::new(clamp_log_capacity(capacity));
        self
    }

    /// Turns the trace on or off at spawn.
    #[must_use]
    pub fn trace(mut self, on: bool) -> Self {
        self.trace = on;
        self
    }

    /// Whether any option is on.
    #[inline]
    pub(crate) fn any(self) -> bool {
        self.statistics || self.trace || self.log.is_some()
    }
}

/// A log capacity brought into `1..=MAX_LOG_CAPACITY`.
pub(crate) fn clamp_log_capacity(capacity: usize) -> u16 {
    // The clamp keeps the value inside `u16`.
    capacity.clamp(1, MAX_LOG_CAPACITY) as u16
}

/// What a spawn passes on for the `sys` bundle: the same value at the first
/// spawn of a supervised child and at every restart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SysOptions {
    pub(crate) debug: DebugOptions,
    pub(crate) allow_state_access: bool,
    pub(crate) sensitive: bool,
}

impl SysOptions {
    /// Whether the actor is sensitive, by its own constant or by the spawn.
    pub(crate) fn effective_sensitive<A: Actor>(self) -> bool {
        A::SENSITIVE || self.sensitive
    }
}

/// The `sys` calls of one actor, with the bound they share.
///
/// A call is served by the actor between two turns, ahead of the messages in
/// its mailbox, and never interrupts a handler that is running. Every call
/// is bounded: a call that is not answered within the bound answers
/// [`SysError::Timeout`], and a call whose caller has given up is dropped
/// unrun if the actor has not begun to serve it; one it has begun is not
/// undone. A call to an actor that has ended, or that ends before it serves
/// the call, answers [`SysError::ActorEnded`] with the reason the actor
/// ended with. A call made from the actor's own task answers
/// [`SysError::CallingSelf`] at once, since the actor serves a call only
/// between two turns and this one would wait for itself.
///
/// Created by [`ActorHandle::sys`](crate::actor::handle::ActorHandle::sys).
///
/// Needs the `sys` feature, which is on by default.
#[cfg(feature = "sys")]
pub struct Sys<'a, A: Actor> {
    pub(super) handle: &'a ActorHandle<A>,
    pub(super) timeout: Duration,
}
// The parts of this module, in the order the generated docs list their methods.

#[cfg(feature = "sys")]
mod client;
