#![warn(missing_docs)]
//! Tokio Actors is a light-weight, Tokio-native actor framework for building
//! hierarchical systems with strongly-typed mailboxes, timers, and supervision.
//! Perfect for AI/LLM Applications.
//!
//! # Overview
//! - Thread-safe actors that run as exclusive tasks on Tokio's multi-threaded runtime.
//! - Typed request/response semantics through [`ActorHandle::send`](crate::actor::handle::ActorHandle::send).
//! - Recurring timers, supervision hooks, and bounded mailboxes out of the box.
//! - See `examples/simple_counter.rs` for a runnable end-to-end example.
//!
//! ```rust,no_run
//! use tokio_actors::{
//!     actor::{context::ActorContext, Actor, ActorExt},
//!     ActorConfig, ActorResult, StopReason, ActorId,
//! };
//! use async_trait::async_trait;
//!
//! #[derive(Default)]
//! struct Counter {
//!     value: i64,
//! }
//!
//! #[derive(Clone)]
//! enum Msg {
//!     Inc(i64),
//!     Get,
//! }
//!
//! #[derive(Clone)]
//! enum Resp {
//!     Ack,
//!     Value(i64),
//! }
//!
//! #[async_trait]
//! impl Actor for Counter {
//!     type Message = Msg;
//!     type Response = Resp;
//!
//!     async fn handle(
//!         &mut self,
//!         msg: Self::Message,
//!         ctx: &mut ActorContext<Self>,
//!     ) -> ActorResult<Self::Response> {
//!         match msg {
//!             Msg::Inc(delta) => {
//!                 self.value += delta;
//!                 Ok(Resp::Ack)
//!             }
//!             Msg::Get => Ok(Resp::Value(self.value)),
//!         }
//!     }
//! }
//!
//! #[tokio::main(flavor = "multi_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let handle = Counter::default().spawn_actor("counter", ActorConfig::default()).await?;
//!     handle.notify(Msg::Inc(1)).await?;
//!     let _value = handle.send(Msg::Get).await?;
//!     handle.stop(StopReason::Graceful).await?;
//!     Ok(())
//! }
//! ```

pub mod actor;
pub mod error;
pub mod types;

pub use actor::{
    context::ActorContext,
    handle::ActorHandle,
    runtime::{ActorConfig, MailboxConfig},
    Actor, ActorExt,
};
pub use error::{
    ActorError, ActorResult, AskError, SendError, SpawnError, TimerError, TrySendError,
};
pub use types::{ActorId, MissPolicy, RecurringId, SchedulePolicy, StopReason};
