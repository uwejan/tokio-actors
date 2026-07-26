#![warn(missing_docs)]
//! Tokio Actors is a light-weight, Tokio-native actor framework for building
//! hierarchical systems with strongly-typed mailboxes, timers, and supervision.
//!
//! # Overview
//! - Thread-safe actors that run as exclusive tasks on Tokio's multi-threaded runtime.
//! - Typed request/response semantics through [`ActorHandle::send`](crate::actor::handle::ActorHandle::send).
//! - Recurring timers, supervision hooks, and bounded mailboxes out of the box.
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

pub mod actor;
pub mod error;
pub mod system;
pub mod types;

pub use actor::{
    context::{ActorContext, ChildSpawnBuilder, RecurringScheduleBuilder, ScheduleBuilder},
    handle::ActorHandle,
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
    RestartStrategy, RestartType, SchedulePolicy, Shutdown, ShutdownReport, StopOutcome,
    StopReason, StreamEvent, StreamId, SupervisionAction,
};
