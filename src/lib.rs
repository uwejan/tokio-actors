#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(missing_debug_implementations)]
#![warn(unreachable_pub)]
#![warn(rust_2018_idioms)]
#![warn(rustdoc::broken_intra_doc_links)]
//! Tokio Actors is a Tokio-native actor framework for building
//! hierarchical systems with strongly-typed mailboxes, timers, and supervision.
//!
//! # Overview
//! - Thread-safe actors, each running as its own task on the Tokio runtime you provide, multi-threaded or current-thread.
//! - Typed request/response semantics through [`ActorHandle::send`](crate::actor::handle::ActorHandle::send).
//! - Recurring timers and supervision (both need a runtime with the Tokio time driver, which `#[tokio::main]` and `#[tokio::test]` enable) and bounded mailboxes, out of the box.
//!
//! # Liveness
//! An actor runs until it is stopped or killed (by a caller, by its supervisor
//! or by a system shutdown), until it fails (an `Err` or a panic from one of
//! its callbacks ends it), or until its runtime ends; dropping its handles
//! does not end it. While it is registered, every actor is enumerable
//! ([`ActorSystem::actor_ids`](crate::system::ActorSystem::actor_ids)) through
//! its system, and killable through it with
//! [`ActorSystem::kill_by_id`](crate::system::ActorSystem::kill_by_id) (which
//! needs a runtime with the Tokio time driver when the id is registered and
//! its actor takes the `Kill`), regardless of how it was spawned. Erlang/OTP gives a
//! process's lifetime structure through links, monitors and `supervisor` trees
//! (erlang.org/docs/28); this crate provides supervision trees, which is why
//! structuring actors under supervisors is the recommended way to manage their
//! lifetimes here.
//!
//! ```rust,no_run
//! use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult, StopReason};
//!
//! #[derive(Default)]
//! struct Counter(i64);
//!
//! impl Actor for Counter {
//!     type Message = i64;
//!     type Response = i64;
//!
//!     async fn handle(&mut self, msg: i64, _ctx: &mut ActorContext<Self>) -> ActorResult<i64> {
//!         self.0 += msg;
//!         Ok(self.0)
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let counter = Counter::default().spawn().named("counter").await?;
//!     counter.notify(5).await?;          // fire-and-forget
//!     let total = counter.send(3).await?; // request-response -> 8
//!     counter.stop(StopReason::Graceful).await?;
//!     Ok(())
//! }
//! ```

/// `true` when this build unwinds on panic, which is what callback-boundary
/// panic capture and crash-visible supervision depend on.
///
/// Under `panic = "abort"` a panic inside any actor callback terminates the
/// whole process: no `on_stopped`, no supervisor notification and no typed
/// error; the crate reports nothing. `Err`-based supervision is unaffected.
/// The process ends the same way for a stack overflow, a panic raised while
/// another unwinds, an allocation failure, `std::process::abort` and
/// `std::process::exit`: no actor runs `on_stopped` and no supervisor is told.
///
/// To refuse such a build, assert it in your own crate:
/// `const _: () = assert!(tokio_actors::PANIC_CAPTURE_AVAILABLE);`
pub const PANIC_CAPTURE_AVAILABLE: bool = cfg!(panic = "unwind");

/// Writes one log line through `tracing` (with the `tracing` feature; a
/// no-op without it), the subscriber's call inside a catch: a subscriber that
/// panics never unwinds into the crate code that logs, so it never changes an
/// actor's reason, never cuts a teardown short, and never aborts the process
/// from a drop. Two aborts are beyond any catch, and Rust turns both into an
/// abort: a panic hook that itself panics, and a subscriber that panics
/// again while its own panic unwinds. Every log line of the crate goes
/// through this macro. Its
/// uses sit in `tracing`-gated code, so a build without the feature uses none.
#[allow(unused_macros)]
macro_rules! caught_log {
    ($level:ident, $($arg:tt)+) => {{
        #[cfg(feature = "tracing")]
        $crate::actor::panic::contain_log(|| {
            ::tracing::$level!($($arg)+);
        });
    }};
}

pub mod actor;
pub mod error;
mod helper;
pub mod system;
mod time_probe;
pub mod types;

pub use actor::{
    context::{ActorContext, ChildSpawnBuilder, RecurringScheduleBuilder, ScheduleBuilder},
    handle::{ActorHandle, SubtreeObserver},
    runtime::{ActorConfig, MailboxConfig},
    supervision::SupervisionConfig,
    Actor, ActorExt, SpawnBuilder,
};
pub use error::{
    ActorError, ActorResult, AskError, SendError, SpawnError, StreamError, SupervisionError,
    TimerError, TrySendError,
};
pub use system::{ActorSystem, ShutdownPolicy, SystemConfig};
pub use types::{
    ActorId, ActorStatus, ActorStatusInfo, ChildEvent, ChildInfo, MissPolicy, RecurringId,
    RestartStrategy, RestartType, SchedulePolicy, Shutdown, ShutdownReport, StopChildOutcome,
    StopOutcome, StopReason, StopReport, StreamEvent, StreamId, SubtreeEnd, SupervisionAction,
};
