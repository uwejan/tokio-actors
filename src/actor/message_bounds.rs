//! The compile-time contract of an actor's message type: it must be
//! `Send + 'static`, because the runtime moves messages between worker
//! threads. These blocks are compiled by `cargo test --doc` only.
//!
//! A message type that is not `Send` does not compile. The `handle` here
//! returns a future that never touches the message, so the only error is the
//! one about the message type itself:
//!
//! ```compile_fail
//! // error[E0277]: `Rc<()>` cannot be sent between threads safely
//! # use std::future::Future;
//! # use tokio_actors::{actor::{Actor, context::ActorContext}, ActorResult};
//! #
//! struct RcMessage;
//!
//! impl Actor for RcMessage {
//!     type Message = std::rc::Rc<()>;
//!     type Response = ();
//!
//!     fn handle(
//!         &mut self,
//!         _msg: Self::Message,
//!         _ctx: &mut ActorContext<Self>,
//!     ) -> impl Future<Output = ActorResult<()>> + Send {
//!         async { Ok(()) }
//!     }
//! }
//! ```
//!
//! The same actor with a `Send` message type compiles:
//!
//! ```rust
//! # use std::future::Future;
//! # use tokio_actors::{actor::{Actor, context::ActorContext}, ActorResult};
//! #
//! struct ArcMessage;
//!
//! impl Actor for ArcMessage {
//!     type Message = std::sync::Arc<()>;
//!     type Response = ();
//!
//!     fn handle(
//!         &mut self,
//!         _msg: Self::Message,
//!         _ctx: &mut ActorContext<Self>,
//!     ) -> impl Future<Output = ActorResult<()>> + Send {
//!         async { Ok(()) }
//!     }
//! }
//! ```
//!
//! The trait itself promises the bound to generic code: a function over any
//! actor may require `Send` of its message type without saying so.
//!
//! ```rust
//! # use tokio_actors::actor::Actor;
//! #
//! fn the_message_type_is_send<A: Actor>() {
//!     fn is_send<T: Send>() {}
//!     is_send::<A::Message>();
//! }
//! ```
