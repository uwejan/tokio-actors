//! The receive timeout, the threshold of the idle hook and `continue_with`.

use std::sync::Arc;
use std::time::Duration;

use super::ActorContext;
use crate::actor::monitor::MonitorState;
use crate::actor::Actor;
use crate::time_probe::require_runtime_timers;

impl<A: Actor> ActorContext<A> {
    /// Sets a receive timeout: when the actor has been idle for `after`,
    /// `map` makes a message and the actor handles it, once per idle spell.
    ///
    /// A spell starts when the run loop is about to wait with the normal
    /// mailbox and the priority lane empty, and ends when a message starts a
    /// turn. A request on the system channel, such as a status read, neither
    /// ends it nor restarts it. The message is handled inline, on the actor's
    /// task, through `handle`: it takes no mailbox slot, leaves the status
    /// `Running`, and has no caller to answer. It fires again in the next
    /// spell, so the timeout stands until
    /// [`cancel_receive_timeout`](Self::cancel_receive_timeout) clears it. A
    /// duration of zero fires once per spell and never spins. A later call
    /// replaces the length and the function without restarting the spell, so a
    /// handler that sets the timeout again does not make it fire a second time
    /// in the same spell: the next fire waits for the next message. A
    /// `Kill` stored on the stop lane when a threshold falls due ends the
    /// actor in place of the fire.
    ///
    /// `map` runs on the actor's task; a panic in it ends the actor with
    /// `Failure`, as a panic in `handle` does.
    ///
    /// Erlang/OTP: the `{timeout, Time, Message}` action, which system
    /// messages do not restart (the legacy `Time` is restarted by them); here
    /// the timeout stands until it is cancelled.
    ///
    /// # Panics
    /// Panics if the actor's runtime was built without the Tokio time driver.
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// enum Msg {
    ///     Tick,
    ///     Count,
    /// }
    ///
    /// struct Poller {
    ///     ticks: u32,
    /// }
    ///
    /// impl Actor for Poller {
    ///     type Message = Msg;
    ///     type Response = u32;
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.receive_timeout(Duration::from_secs(5), || Msg::Tick);
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
    ///         if let Msg::Tick = msg {
    ///             self.ticks += 1;
    ///         }
    ///         Ok(self.ticks)
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let poller = Poller { ticks: 0 }.spawn().await?;
    /// assert_eq!(poller.send(Msg::Count).await?, 0);
    /// # Ok(())
    /// # }
    /// ```
    pub fn receive_timeout<F>(&mut self, after: Duration, map: F)
    where
        F: Fn() -> A::Message + Send + Sync + 'static,
    {
        require_runtime_timers("ActorContext::receive_timeout", &self.runtime);
        self.turn_mut().set_receive(after, Arc::new(map));
    }

    /// Clears the receive timeout set with
    /// [`receive_timeout`](Self::receive_timeout).
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// struct Poller;
    ///
    /// impl Actor for Poller {
    ///     type Message = ();
    ///     type Response = ();
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.receive_timeout(Duration::from_secs(5), || ());
    ///         ctx.cancel_receive_timeout();
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// Poller.spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn cancel_receive_timeout(&mut self) {
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.clear_receive();
        }
    }

    /// Sets how long the actor may be idle before
    /// [`on_idle`](Actor::on_idle) runs, once per idle spell. The spell is the
    /// one [`receive_timeout`](Self::receive_timeout) describes: a status
    /// read does not restart it, and a message ends it. A duration of zero
    /// runs the hook once per spell and never spins. A later call replaces
    /// the length without restarting the spell.
    ///
    /// Erlang/OTP: the trigger of `hibernate_after`, without the hibernation.
    ///
    /// # Panics
    /// Panics if the actor's runtime was built without the Tokio time driver.
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// struct Cache;
    ///
    /// impl Actor for Cache {
    ///     type Message = ();
    ///     type Response = ();
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.idle_after(Duration::from_secs(30));
    ///         Ok(())
    ///     }
    ///
    ///     async fn on_idle(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// Cache.spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn idle_after(&mut self, after: Duration) {
        require_runtime_timers("ActorContext::idle_after", &self.runtime);
        self.turn_mut().set_idle(after);
    }

    /// Clears the idle length set with [`idle_after`](Self::idle_after).
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// struct Cache;
    ///
    /// impl Actor for Cache {
    ///     type Message = ();
    ///     type Response = ();
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.idle_after(Duration::from_secs(30));
    ///         ctx.cancel_idle_after();
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// Cache.spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn cancel_idle_after(&mut self) {
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.clear_idle();
        }
    }

    /// Asks the actor to handle `msg` next, right after this callback, before
    /// it reads any other message, request or stop.
    ///
    /// The step runs after the reply of the current request is sent, takes no
    /// mailbox slot and has no caller to answer. A step may call
    /// `continue_with` again, so a chain of steps runs without anything else
    /// between them; a request on the system channel, such as a status read,
    /// waits until the chain ends, and so does a graceful stop asked from
    /// outside, as a system message waits behind `{continue, _}` in OTP. A
    /// stored `Kill` is read before each step and ends the actor without
    /// running the rest. Each step spends the task's cooperative budget, so a
    /// long chain gives the runtime's other tasks their turn. A second call in one
    /// callback replaces the first. A message set in `pre_start` or
    /// `on_started` runs once after the start is acknowledged, before the
    /// first message, and one set in [`on_idle`](Actor::on_idle) runs after it.
    ///
    /// The message is dropped, not handled, when the callback returns an
    /// `Err` or panics, when it calls [`stop`](Self::stop), when the start
    /// fails, when it is set in a callback that runs after the stop began, in
    /// `on_child_stopped` or in a `pre_stop` that vetoes the stop, and when the
    /// actor ends first.
    ///
    /// Erlang/OTP: the `{continue, Term}` return of a `gen_server` callback
    /// and `handle_continue/2`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// enum Msg {
    ///     Start,
    ///     Step,
    ///     Count,
    /// }
    ///
    /// struct Job {
    ///     log: Vec<&'static str>,
    /// }
    ///
    /// impl Actor for Job {
    ///     type Message = Msg;
    ///     type Response = usize;
    ///
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<usize> {
    ///         match msg {
    ///             Msg::Start => {
    ///                 self.log.push("start");
    ///                 ctx.continue_with(Msg::Step);
    ///             }
    ///             Msg::Step => self.log.push("step"),
    ///             Msg::Count => {}
    ///         }
    ///         Ok(self.log.len())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let job = Job { log: Vec::new() }.spawn().await?;
    /// assert_eq!(job.send(Msg::Start).await?, 1);
    /// assert_eq!(job.send(Msg::Count).await?, 2);
    /// # Ok(())
    /// # }
    /// ```
    pub fn continue_with(&mut self, msg: A::Message) {
        self.turn_mut().set_continuation(msg);
    }
}
