//! Behaviors: an actor whose handler returns its next state.
//!
//! A [`Behavior`] is a state that handles one message and says what state
//! handles the next, the actor model's `become`. [`BehaviorActor`] runs one as
//! an ordinary [`Actor`], so a behavior is spawned, supervised, monitored and
//! stopped like any other actor.
//!
//! The handler is a plain `fn`, so it cannot await: it works on the state it
//! owns and hands any waiting to the actor model. To ask another actor, it
//! calls [`BehaviorContext::request`], which returns at once and delivers the
//! answer as a later message, and the next state decides what to do with it.
//!
//! The state moves in and out of the handler once per message, so a large state
//! belongs in a `Box`. Dispatch is static; no `Box<dyn Behavior>` is involved.

use std::time::Duration;

use crate::actor::alias::{Alias, AliasOptions};
use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::monitor::IntoMonitorTarget;
use crate::actor::Actor;
use crate::error::{ActorError, ActorResult, LinkError, MonitorError, RequestError, StopError};
use crate::types::{ActorId, Down, MonitorId, RequestId, RequestReply, StopReason};

/// A state that handles a message and names the state that handles the next.
///
/// `handle` takes the state by value and returns the next state with the
/// response to the message. It is a plain `fn`: it cannot await, which keeps
/// the actor model's rule that an actor never blocks on another, and it asks
/// other actors through [`BehaviorContext::request`], whose answer arrives as
/// a message.
///
/// A panic in `handle` fails the actor, as a panic in any handler does.
///
/// # Examples
/// A counter whose state changes with each message:
/// ```rust
/// use tokio_actors::{
///     actor::ActorExt, Behavior, BehaviorActor, BehaviorContext, Next, StopReason,
/// };
///
/// enum Step {
///     Add(u32),
///     Finish,
/// }
///
/// struct Counting(u32);
///
/// impl Behavior for Counting {
///     type Message = Step;
///     type Response = u32;
///
///     fn handle(self, msg: Step, cx: &mut BehaviorContext<'_, Self>) -> (Next<Self>, u32) {
///         let _ = cx.actor_id();
///         match msg {
///             Step::Add(n) => {
///                 let total = self.0 + n;
///                 (Next::Become(Counting(total)), total)
///             }
///             Step::Finish => (Next::Stop(StopReason::Graceful), self.0),
///         }
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let counter = BehaviorActor::new(Counting(0)).spawn().await?;
/// assert_eq!(counter.send(Step::Add(2)).await?, 2);
/// assert_eq!(counter.send(Step::Add(3)).await?, 5);
/// assert_eq!(counter.send(Step::Finish).await?, 5);
/// assert_eq!(counter.wait_stopped().await, StopReason::Graceful);
/// # Ok(())
/// # }
/// ```
///
/// The handler cannot await. This does not compile, because `handle` is not
/// an `async fn`:
/// ```rust,compile_fail,E0728
/// use tokio_actors::{Behavior, BehaviorContext, Next};
///
/// struct Waiting;
///
/// impl Behavior for Waiting {
///     type Message = ();
///     type Response = ();
///
///     fn handle(self, _: (), _: &mut BehaviorContext<'_, Self>) -> (Next<Self>, ()) {
///         std::future::ready(()).await;
///         (Next::Become(Waiting), ())
///     }
/// }
/// ```
/// The same behavior without the await compiles:
/// ```rust
/// use tokio_actors::{Behavior, BehaviorContext, Next};
///
/// struct Waiting;
///
/// impl Behavior for Waiting {
///     type Message = ();
///     type Response = ();
///
///     fn handle(self, _: (), _: &mut BehaviorContext<'_, Self>) -> (Next<Self>, ()) {
///         (Next::Become(Waiting), ())
///     }
/// }
/// ```
pub trait Behavior: Sized + Send + 'static {
    /// The type of message the behavior receives.
    type Message: Send + 'static;

    /// The type of response the behavior produces.
    type Response: Send + 'static;

    /// Handles one message with this state and names the next state.
    fn handle(
        self,
        msg: Self::Message,
        cx: &mut BehaviorContext<'_, Self>,
    ) -> (Next<Self>, Self::Response);
}

/// What follows a message a [`Behavior`] handled.
///
/// There is no variant that keeps the state: a handler that wants the same
/// behavior for the next message returns `Become(self)`.
///
/// # Examples
/// ```rust
/// use tokio_actors::{Behavior, BehaviorContext, Next, StopReason};
///
/// struct Door;
///
/// impl Behavior for Door {
///     type Message = bool;
///     type Response = ();
///
///     fn handle(self, close: bool, _: &mut BehaviorContext<'_, Self>) -> (Next<Self>, ()) {
///         // Keep the state, or end the actor.
///         if close {
///             (Next::Stop(StopReason::Graceful), ())
///         } else {
///             (Next::Become(self), ())
///         }
///     }
/// }
/// ```
#[derive(Debug)]
pub enum Next<B> {
    /// The next message is handled by this state.
    Become(B),
    /// The actor stops with this reason, as
    /// [`ActorContext::stop`](crate::actor::context::ActorContext::stop) stops
    /// it: the response of this message is held until the stop is sealed, and
    /// no further message is handled. `StopReason::Kill` is refused there, and
    /// here it fails the actor with an error.
    Stop(StopReason),
}

/// The part of an actor's context a [`Behavior`] handler may use.
///
/// It offers the calls that do not await: identity, requests, monitors, links,
/// aliases, a continuation and the actor's own stop. Spawning children,
/// timers and streams are not offered, because a behavior is a message
/// handler and nothing more.
///
/// # Examples
/// ```rust
/// use tokio_actors::{
///     actor::ActorExt, ActorId, Behavior, BehaviorActor, BehaviorContext, Next,
/// };
///
/// struct WhoAmI;
///
/// impl Behavior for WhoAmI {
///     type Message = ();
///     type Response = ActorId;
///
///     fn handle(self, _: (), cx: &mut BehaviorContext<'_, Self>) -> (Next<Self>, ActorId) {
///         let id = cx.actor_id().clone();
///         (Next::Become(self), id)
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let actor = BehaviorActor::new(WhoAmI).spawn().await?;
/// assert_eq!(&actor.send(()).await?, actor.id());
/// # Ok(())
/// # }
/// ```
pub struct BehaviorContext<'a, B: Behavior> {
    ctx: &'a mut ActorContext<BehaviorActor<B>>,
}

impl<B: Behavior> std::fmt::Debug for BehaviorContext<'_, B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BehaviorContext")
            .field("actor_id", self.ctx.actor_id())
            .finish_non_exhaustive()
    }
}

impl<B: Behavior> BehaviorContext<'_, B> {
    /// The actor's id. See
    /// [`ActorContext::actor_id`](crate::actor::context::ActorContext::actor_id).
    pub fn actor_id(&self) -> &ActorId {
        self.ctx.actor_id()
    }

    /// A handle to this actor. See
    /// [`ActorContext::self_handle`](crate::actor::context::ActorContext::self_handle).
    pub fn self_handle(&self) -> ActorHandle<BehaviorActor<B>> {
        self.ctx.self_handle()
    }

    /// Asks another actor and gets the answer as a message. See
    /// [`ActorContext::request`](crate::actor::context::ActorContext::request).
    ///
    /// # Errors
    /// As for the context's method.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult, Behavior, BehaviorActor, BehaviorContext, Next, RequestReply,
    /// };
    ///
    /// struct Doubler;
    ///
    /// impl Actor for Doubler {
    ///     type Message = u32;
    ///     type Response = u32;
    ///     async fn handle(&mut self, msg: u32, _: &mut ActorContext<Self>) -> ActorResult<u32> {
    ///         Ok(msg * 2)
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Ask(ActorHandle<Doubler>, u32),
    ///     Answer(RequestReply<u32>),
    /// }
    ///
    /// struct Asker(mpsc::UnboundedSender<u32>);
    ///
    /// impl Behavior for Asker {
    ///     type Message = Msg;
    ///     type Response = ();
    ///
    ///     fn handle(self, msg: Msg, cx: &mut BehaviorContext<'_, Self>) -> (Next<Self>, ()) {
    ///         match msg {
    ///             Msg::Ask(doubler, value) => {
    ///                 cx.request(&doubler, value, Msg::Answer).expect("running");
    ///             }
    ///             Msg::Answer(reply) => {
    ///                 let _ = self.0.send(reply.result.expect("the doubler answers"));
    ///             }
    ///         }
    ///         (Next::Become(self), ())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let (answers, mut received) = mpsc::unbounded_channel();
    /// let doubler = Doubler.spawn().await?;
    /// let asker = BehaviorActor::new(Asker(answers)).spawn().await?;
    /// asker.notify(Msg::Ask(doubler, 4)).await?;
    /// assert_eq!(received.recv().await, Some(8));
    /// # Ok(())
    /// # }
    /// ```
    pub fn request<T, F>(
        &mut self,
        target: &ActorHandle<T>,
        msg: T::Message,
        map: F,
    ) -> Result<RequestId, RequestError>
    where
        T: Actor,
        F: Fn(RequestReply<T::Response>) -> B::Message + Send + Sync + 'static,
    {
        self.ctx.request(target, msg, map)
    }

    /// As [`request`](Self::request), bounded by a deadline. See
    /// [`ActorContext::request_within`](crate::actor::context::ActorContext::request_within).
    ///
    /// # Panics
    /// When the actor's runtime has no Tokio time driver.
    ///
    /// # Errors
    /// As for the context's method.
    pub fn request_within<T, F>(
        &mut self,
        target: &ActorHandle<T>,
        msg: T::Message,
        within: Duration,
        map: F,
    ) -> Result<RequestId, RequestError>
    where
        T: Actor,
        F: Fn(RequestReply<T::Response>) -> B::Message + Send + Sync + 'static,
    {
        self.ctx.request_within(target, msg, within, map)
    }

    /// Drops a request's reply. See
    /// [`ActorContext::cancel_request`](crate::actor::context::ActorContext::cancel_request).
    pub fn cancel_request(&mut self, id: RequestId) -> bool {
        self.ctx.cancel_request(id)
    }

    /// Watches another actor. See
    /// [`ActorContext::monitor`](crate::actor::context::ActorContext::monitor).
    ///
    /// # Errors
    /// As for the context's method.
    pub fn monitor<T, F>(&mut self, target: T, map: F) -> Result<MonitorId, MonitorError>
    where
        T: IntoMonitorTarget,
        F: Fn(Down) -> B::Message + Send + Sync + 'static,
    {
        self.ctx.monitor(target, map)
    }

    /// Revokes a monitor. See
    /// [`ActorContext::demonitor`](crate::actor::context::ActorContext::demonitor).
    pub fn demonitor(&mut self, id: MonitorId) -> bool {
        self.ctx.demonitor(id)
    }

    /// Links this actor to another. See
    /// [`ActorContext::link`](crate::actor::context::ActorContext::link).
    ///
    /// # Errors
    /// As for the context's method.
    pub fn link<T: IntoMonitorTarget>(&mut self, target: T) -> Result<(), LinkError> {
        self.ctx.link(target)
    }

    /// Cancels a link. See
    /// [`ActorContext::unlink`](crate::actor::context::ActorContext::unlink).
    pub fn unlink<T: IntoMonitorTarget>(&mut self, target: T) -> bool {
        self.ctx.unlink(target)
    }

    /// Runs `msg` after this turn. See
    /// [`ActorContext::continue_with`](crate::actor::context::ActorContext::continue_with).
    pub fn continue_with(&mut self, msg: B::Message) {
        self.ctx.continue_with(msg);
    }

    /// Makes an alias for this actor. See
    /// [`ActorContext::alias`](crate::actor::context::ActorContext::alias).
    pub fn alias(&mut self, options: AliasOptions) -> Alias<BehaviorActor<B>> {
        self.ctx.alias(options)
    }

    /// Deactivates an alias. See
    /// [`ActorContext::unalias`](crate::actor::context::ActorContext::unalias).
    pub fn unalias(&mut self, alias: &Alias<BehaviorActor<B>>) -> bool {
        self.ctx.unalias(alias)
    }

    /// Stops this actor with `reason` after the message in hand. See
    /// [`ActorContext::stop`](crate::actor::context::ActorContext::stop).
    ///
    /// # Errors
    /// As for the context's method.
    pub fn stop(&mut self, reason: StopReason) -> Result<(), StopError> {
        self.ctx.stop(reason)
    }
}

/// Runs a [`Behavior`] as an [`Actor`].
///
/// The actor holds the current state. Each message moves it into
/// [`Behavior::handle`] and puts the state it returns back, so a message that
/// fails the actor (a panic in the handler) leaves no state behind.
///
/// # Examples
/// ```rust
/// use tokio_actors::{actor::ActorExt, Behavior, BehaviorActor, BehaviorContext, Next};
///
/// struct Echo;
///
/// impl Behavior for Echo {
///     type Message = u32;
///     type Response = u32;
///
///     fn handle(self, msg: u32, _: &mut BehaviorContext<'_, Self>) -> (Next<Self>, u32) {
///         (Next::Become(self), msg)
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let echo = BehaviorActor::new(Echo).spawn().await?;
/// assert_eq!(echo.send(7).await?, 7);
/// # Ok(())
/// # }
/// ```
pub struct BehaviorActor<B: Behavior> {
    state: Option<B>,
}

impl<B: Behavior> std::fmt::Debug for BehaviorActor<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BehaviorActor")
            .field("holds_state", &self.state.is_some())
            .finish()
    }
}

impl<B: Behavior> BehaviorActor<B> {
    /// An actor that starts in `behavior`.
    pub fn new(behavior: B) -> Self {
        Self {
            state: Some(behavior),
        }
    }
}

impl<B: Behavior> Actor for BehaviorActor<B> {
    type Message = B::Message;
    type Response = B::Response;

    async fn handle(
        &mut self,
        msg: B::Message,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<B::Response> {
        let Some(state) = self.state.take() else {
            return Err(ActorError::user("the behavior holds no state"));
        };
        let (next, response) = state.handle(msg, &mut BehaviorContext { ctx: &mut *ctx });
        match next {
            Next::Become(state) => self.state = Some(state),
            Next::Stop(reason) => match ctx.stop(reason) {
                Ok(()) | Err(StopError::Closed) => {}
                Err(StopError::KillRefused) => {
                    return Err(ActorError::user(
                        "a behavior cannot stop its actor with Kill",
                    ));
                }
            },
        }
        Ok(response)
    }
}
