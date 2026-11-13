#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(missing_debug_implementations)]
#![warn(unreachable_pub)]
#![warn(rust_2018_idioms)]
#![warn(rustdoc::broken_intra_doc_links)]
#![cfg_attr(test, allow(dead_code))]
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
//! its actor takes the `Kill`), regardless of how it was spawned. Erlang/OTP
//! gives a process's lifetime structure through links, monitors and
//! `supervisor` trees (erlang.org/docs/28); this crate provides supervision
//! trees, links and monitors, and structuring actors under supervisors is still
//! the recommended way to manage their lifetimes here.
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
    // The span the actor task runs in, as a type.
    (@span_type) => {
        ::tracing::Span
    };
    // The guard of an entered span, as a type.
    (@entered_type $lt:lifetime) => {
        ::tracing::span::Entered<$lt>
    };
    // Whether the process-wide level checks every tracing macro makes first
    // pass for a level: the compile-time ceiling and the highest level any
    // installed subscriber takes. False when no subscriber takes the level.
    (@level $level:ident) => {
        ::tracing::Level::$level <= ::tracing::level_filters::STATIC_MAX_LEVEL
            && ::tracing::Level::$level <= ::tracing::level_filters::LevelFilter::current()
    };
    // Whether a subscriber takes error-level statements, the most severe
    // level, which every subscriber filtering on severity accepts; the
    // statements of the crate that carry the identity fields of an actor are
    // written at error, warn or info level.
    (@interest) => {
        ::tracing::enabled!(::tracing::Level::ERROR)
    };
    // Whether a subscriber takes statements of `$level` for the target
    // `$target`, asked inside a catch so a subscriber that panics reads as
    // not interested; false without the feature. A crate path that owes a
    // step asks this only to decide whether to format text it would
    // otherwise never build.
    (@enabled $level:ident, $target:expr) => {{
        #[cfg(feature = "tracing")]
        let interested = $crate::actor::panic::catch_sync(|| {
            ::tracing::enabled!(target: $target, ::tracing::Level::$level)
        })
        .unwrap_or_else(|payload| {
            std::mem::forget(payload);
            false
        });
        #[cfg(not(feature = "tracing"))]
        let interested = false;
        interested
    }};
    // A new root span for one incarnation; the fields are evaluated only
    // when the span's callsite is enabled.
    (@new_span $id:expr, $name:expr, $incarnation:expr, $system:expr) => {
        ::tracing::debug_span!(
            parent: None,
            "actor",
            actor.id = %$id,
            actor.name = $name,
            actor.incarnation = $incarnation,
            system = $system,
        )
    };
}

pub mod actor;
pub mod behavior;
pub mod error;
mod helper;
pub mod system;
pub mod telemetry;
mod time_probe;
#[cfg(test)]
#[cfg(feature = "tracing")]
mod tracing_isolation;
pub mod types;

pub use actor::sys;
pub use actor::{
    alias::{Alias, AliasOptions},
    context::{ActorContext, ChildSpawnBuilder, RecurringScheduleBuilder, ScheduleBuilder},
    handle::{ActorHandle, SubtreeObserver},
    monitor::IntoMonitorTarget,
    pg::PgScope,
    runtime::{ActorConfig, MailboxConfig},
    supervision::{AutoShutdown, SupervisionConfig},
    supervisor_ref::SupervisorRef,
    Actor, ActorExt, LinkedSpawn, MonitoredSpawn, SpawnBuilder,
};
pub use behavior::{Behavior, BehaviorActor, BehaviorContext, Next};
pub use error::{
    ActorError, ActorResult, AliasError, AskError, ExitError, LinkError, MonitorError, PgError,
    RequestError, SendError, SpawnError, StopError, StreamError, SupervisionError,
    SupervisorRefError, TimerError, TrySendError,
};
pub use system::{ActorSystem, ShutdownPolicy, SystemConfig};
pub use types::{
    ActorId, ActorStatus, ActorStatusInfo, ChildCounts, ChildEvent, ChildInfo, Down, DownReason,
    GroupChange, Incarnation, LinkExit, MissPolicy, MonitorId, PeerRef, RecurringId, RequestId,
    RequestReply, RestartStrategy, RestartType, Shutdown, ShutdownReport, StopChildOutcome,
    StopOutcome, StopReason, StopReport, StreamEvent, StreamId, SubtreeEnd, SupervisionAction,
};
