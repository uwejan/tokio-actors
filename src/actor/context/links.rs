//! Links and exit signals: `link`, `unlink`, `trap_exit` and `exit`.

use std::sync::Arc;

use super::ActorContext;
use crate::actor::alias::AliasShared;
#[cfg(doc)]
use crate::actor::handle::ActorHandle;
use crate::actor::link::ExitPush;
use crate::actor::monitor::IntoMonitorTarget;
use crate::actor::{link, monitor, Actor};
#[cfg(doc)]
use crate::error::ActorError;
use crate::error::LinkError;
use crate::system::{raise_kill_declared, KillKey};
#[cfg(doc)]
use crate::types::ActorId;
use crate::types::{LinkExit, PeerRef, StopReason};

impl<A: Actor> ActorContext<A> {
    /// Links this actor to another: when either ends, the other is told.
    ///
    /// Kind: a registration of a notice or an answer the actor then waits for; refused with `LinkError::Stopping` while the actor is stopping or stopped. See [`signals`](crate::signals).
    ///
    /// The target is an [`ActorHandle`], an [`ActorId`] or a name, as for
    /// [`monitor`](Self::monitor). When the actor it names ends, this actor
    /// ends with the same reason, with no hook run, as an Erlang/OTP process
    /// that does not trap exits ends with its partner's exit reason, unless
    /// the reason is [`Graceful`](StopReason::Graceful): an orderly end of
    /// the other actor does not end this one. An actor that called
    /// [`trap_exit`](Self::trap_exit) is told with a [`LinkExit`] instead,
    /// through the map it gave, for every reason including `Graceful`. The
    /// flag is read when the other actor's end is seen.
    ///
    /// The link is bound to one run of each actor. It is symmetric, and both
    /// ends hold one state, so two actors that link each other at once share
    /// one link, and one [`unlink`](Self::unlink) cancels it for both. A
    /// repeated call is the same link. A link to oneself does nothing.
    ///
    /// Linking an actor that is not running, or that no key resolves to, is
    /// not an error: the exit arrives at once with
    /// [`DownReason::NoProc`](crate::types::DownReason::NoProc), and an actor
    /// that does not trap exits ends with
    /// [`ActorError::NoProc`], as `link/1`
    /// on a missing process does in Erlang/OTP.
    ///
    /// A trapped exit enters through an ordinary mailbox slot, after every
    /// message the other actor sent before it ended, so a queued `LinkExit`
    /// is a queued message; [`link_priority`](Self::link_priority) takes the
    /// priority lane instead. An exit still pending when this actor stops for
    /// any reason but `Kill` is delivered once before `on_stopped`, and none
    /// is delivered under `Kill`.
    ///
    /// Links are made and cancelled with no await, and may cross runtimes
    /// and systems (each entry runs on its owner's runtime).
    ///
    /// # Errors
    /// [`LinkError::Stopping`] once this actor is stopping.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult, DownReason, LinkExit, StopReason,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Watch,
    ///     Exit(LinkExit),
    /// }
    ///
    /// struct Boss {
    ///     worker: ActorHandle<Worker>,
    ///     seen: tokio::sync::mpsc::UnboundedSender<LinkExit>,
    /// }
    ///
    /// impl Actor for Boss {
    ///     type Message = Msg;
    ///     type Response = bool;
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<bool> {
    ///         match msg {
    ///             Msg::Watch => {
    ///                 ctx.trap_exit(Msg::Exit);
    ///                 Ok(ctx.link(&self.worker).is_ok())
    ///             }
    ///             Msg::Exit(exit) => {
    ///                 let _ = self.seen.send(exit);
    ///                 Ok(true)
    ///             }
    ///         }
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let worker = Worker.spawn().await?;
    /// let (seen, mut exits) = tokio::sync::mpsc::unbounded_channel();
    /// let boss = Boss { worker: worker.clone(), seen }.spawn().await?;
    /// assert!(boss.send(Msg::Watch).await?);
    /// worker.stop(StopReason::Graceful).await?;
    /// let exit = exits.recv().await.expect("the boss is told");
    /// assert_eq!(exit.actor_id, *worker.id());
    /// assert_eq!(exit.reason, DownReason::Stopped(StopReason::Graceful));
    /// # Ok(())
    /// # }
    /// ```
    pub fn link<T: IntoMonitorTarget>(&mut self, target: T) -> Result<(), LinkError> {
        link::link(self, target, false, false)
    }

    /// As [`link`](Self::link), and this actor's exit takes the priority lane
    /// when the actor has one, otherwise the ordinary mailbox. The option is
    /// this actor's own: the other end's lane is its own choice, and a
    /// repeated call to `link` or `link_priority` sets this end's lane.
    ///
    /// Kind: a registration of a notice or an answer the actor then waits for; refused with `LinkError::Stopping` while the actor is stopping or stopped. See [`signals`](crate::signals).
    ///
    /// # Errors
    /// [`LinkError::Stopping`] once this actor is stopping.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorConfig, ActorHandle, ActorResult, LinkExit, MailboxConfig,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Boss(ActorHandle<Worker>);
    ///
    /// impl Actor for Boss {
    ///     type Message = Option<LinkExit>;
    ///     type Response = bool;
    ///     async fn handle(
    ///         &mut self,
    ///         msg: Option<LinkExit>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<bool> {
    ///         if msg.is_none() {
    ///             ctx.trap_exit(Some);
    ///             return Ok(ctx.link_priority(&self.0).is_ok());
    ///         }
    ///         Ok(true)
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let worker = Worker.spawn().await?;
    /// let config = ActorConfig {
    ///     mailbox: MailboxConfig {
    ///         priority_capacity: Some(8),
    ///         ..MailboxConfig::default()
    ///     },
    ///     ..ActorConfig::default()
    /// };
    /// let boss = Boss(worker).spawn().with_config(config).await?;
    /// assert!(boss.send(None).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn link_priority<T: IntoMonitorTarget>(&mut self, target: T) -> Result<(), LinkError> {
        link::link(self, target, true, false)
    }

    /// Cancels the link to an actor. True means the cancel was in time: no
    /// exit from that link will be acted on from here on, however the other
    /// actor ends, as Erlang/OTP guarantees for `unlink/1`. False means no
    /// such link is held, or the exit was already acted on (it may be queued
    /// in this actor's mailbox, and it is still delivered).
    ///
    /// Kind: a revoke, answers `bool`. See [`signals`](crate::signals).
    ///
    /// The other actor is told, and drops its end of the link.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult, StopReason,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Boss(ActorHandle<Worker>);
    ///
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = (bool, bool);
    ///     async fn handle(
    ///         &mut self,
    ///         _: (),
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<(bool, bool)> {
    ///         ctx.link(&self.0).expect("running");
    ///         let first = ctx.unlink(&self.0);
    ///         let second = ctx.unlink(&self.0);
    ///         Ok((first, second))
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let worker = Worker.spawn().await?;
    /// let boss = Boss(worker.clone()).spawn().await?;
    /// assert_eq!(boss.send(()).await?, (true, false));
    /// // With the link cancelled, the boss goes on when its worker fails.
    /// worker.stop(StopReason::Failure(tokio_actors::ActorError::user("boom"))).await?;
    /// assert!(boss.is_alive());
    /// # Ok(())
    /// # }
    /// ```
    pub fn unlink<T: IntoMonitorTarget>(&mut self, target: T) -> bool {
        link::unlink(self, target)
    }

    /// Makes this actor trap exits: a linked actor's end, and an exit signal
    /// sent to this actor, reach `handle` as the message `map` makes from a
    /// [`LinkExit`], instead of ending this actor. A later call replaces the
    /// map.
    ///
    /// The map runs on a task of the crate, concurrently with this actor, except
    /// for an exit signal that rides the priority lane (an
    /// [`exit_priority`](Self::exit_priority) through an active priority alias
    /// to an actor with a lane): that map runs on the actor's own task between
    /// turns, as a turn the stuck-turn watchdog sees. It must not block either
    /// way, and a panic of it is contained: the actor ends with
    /// [`StopReason::Failure`] carrying that panic, unless a `Kill` or a
    /// `Failure` already stands, as it does for a monitor's map, whatever
    /// the linked actor's reason was (an orderly `Graceful` end included), and
    /// the panic is logged with the `tracing` feature. The flag is read when a linked
    /// actor's end is seen, so a call made just before that end is in time
    /// and a call made after it is not.
    ///
    /// A supervisor traps exits from its creation, as an Erlang/OTP
    /// supervisor does. With no map it drops an exit that is not its
    /// child's, silently.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, LinkExit,
    /// };
    ///
    /// struct Trapper;
    ///
    /// impl Actor for Trapper {
    ///     type Message = Option<LinkExit>;
    ///     type Response = ();
    ///     async fn handle(
    ///         &mut self,
    ///         msg: Option<LinkExit>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<()> {
    ///         if msg.is_none() {
    ///             ctx.trap_exit(Some);
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let trapper = Trapper.spawn().await?;
    /// assert!(!trapper.get_status().await?.trap_exit);
    /// trapper.send(None).await?;
    /// assert!(trapper.get_status().await?.trap_exit);
    /// # Ok(())
    /// # }
    /// ```
    pub fn trap_exit<F>(&mut self, map: F)
    where
        F: Fn(LinkExit) -> A::Message + Send + Sync + 'static,
    {
        link::trap_exit(self, Arc::new(map));
    }

    /// Stops trapping exits: a linked actor's end ends this actor again.
    /// The flag is cleared first and the map after. A supervisor goes on
    /// trapping, and only its map is cleared.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, LinkExit,
    /// };
    ///
    /// struct Toggle;
    ///
    /// impl Actor for Toggle {
    ///     type Message = bool;
    ///     type Response = ();
    ///     async fn handle(&mut self, on: bool, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         if on {
    ///             ctx.trap_exit(|_: LinkExit| false);
    ///         } else {
    ///             ctx.untrap_exit();
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let toggle = Toggle.spawn().await?;
    /// toggle.send(true).await?;
    /// assert!(toggle.get_status().await?.trap_exit);
    /// toggle.send(false).await?;
    /// assert!(!toggle.get_status().await?.trap_exit);
    /// # Ok(())
    /// # }
    /// ```
    pub fn untrap_exit(&mut self) {
        link::untrap_exit(self);
    }
}

impl<A: Actor> ActorContext<A> {
    /// Sends `target` an exit signal carrying `reason`, from this actor.
    ///
    /// Kind: a send; a stopping actor may still make it. See [`signals`](crate::signals).
    ///
    /// An actor that does not trap exit signals ends at once with `reason`
    /// as its reason, runs no `on_stopped`, and a request it was handling
    /// reads `ResponseDropped` carrying `reason`. A `reason` of
    /// [`Kill`](StopReason::Kill) is not trappable. An actor that traps
    /// exit signals gets the signal as a
    /// [`LinkExit`] through its map, like a link's
    /// end. A [`Graceful`](StopReason::Graceful) signal to another actor that
    /// does not trap is ignored; to this actor itself it ends a non-trapping
    /// actor. The target's end is backstopped like a kill: a target that does
    /// not reach a turn boundary within its kill grace is aborted. A signal to
    /// an actor that has ended, or to a name that matches none, does nothing.
    ///
    /// A supervised child that ends this way is restarted or not by its
    /// restart type, like any other end: a `Transient` child that receives
    /// [`Shutdown`](StopReason::Shutdown) is not restarted.
    ///
    /// The signals waiting for a trapping actor are bounded: as many signals
    /// sent with `exit` or [`exit_priority`](Self::exit_priority) may wait
    /// for it as its mailbox holds (64 for an actor that traps without having
    /// called [`trap_exit`](Self::trap_exit), as a supervisor does), counted
    /// until each one's delivery takes a slot of that mailbox. The exits of
    /// linked peers are not counted.
    ///
    /// Erlang/OTP: `exit(Pid, Reason)`; Erlang/OTP queues every signal
    /// without a bound, where this call refuses one over the bound.
    ///
    /// # Errors
    /// - [`ExitError::Full`](crate::error::ExitError::Full) when the target
    ///   traps exit signals and as many signals already wait for it as that
    ///   bound allows; the signal is not sent. A signal to a target that
    ///   does not trap, a `Kill`, and a signal to an ended or missing target
    ///   are never refused.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorError, ActorHandle, ActorResult, StopReason,
    /// };
    ///
    /// struct Idle;
    ///
    /// impl Actor for Idle {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Sender;
    ///
    /// impl Actor for Sender {
    ///     type Message = ActorHandle<Idle>;
    ///     type Response = ();
    ///     async fn handle(
    ///         &mut self,
    ///         target: ActorHandle<Idle>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<()> {
    ///         ctx.exit(&target, StopReason::Failure(ActorError::user("signalled")))
    ///             .expect("a target that does not trap never refuses");
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let target = Idle.spawn().await?;
    /// let sender = Sender.spawn().await?;
    /// sender.send(target.clone()).await?;
    /// assert!(matches!(target.wait_stopped().await, StopReason::Failure(_)));
    /// # Ok(())
    /// # }
    /// ```
    pub fn exit<T: IntoMonitorTarget>(
        &mut self,
        target: T,
        reason: StopReason,
    ) -> Result<(), crate::error::ExitError> {
        self.exit_with(&target, reason, None)
    }

    /// `exit`, with the alias the signal is sent through, if any. The alias's
    /// read guard is held from the check that it is active to the end of the
    /// call, the push into the target's inbox and the raise on a receiver
    /// that does not trap, so the alias's `unalias` is ordered against all of
    /// it: a signal through an alias that is not active has no effect, and
    /// once `unalias` returned `true` nothing of that alias takes effect. The
    /// signal rides the priority lane only for an alias made with the
    /// priority option.
    pub(super) fn exit_with<T: IntoMonitorTarget>(
        &mut self,
        target: &T,
        reason: StopReason,
        alias: Option<&AliasShared>,
    ) -> Result<(), crate::error::ExitError> {
        // The guard is released explicitly, before the locals below drop, so
        // the drop of a clone of the system or of a reason never runs under
        // it.
        let gate = alias.map(AliasShared::read);
        if alias.is_some_and(|alias| !alias.is_active()) {
            return Ok(());
        }
        let via = monitor::route_of(target);
        // An id or a name is looked up in this actor's system; a handle names
        // its own, reached by identity through its record. A system that is
        // gone has no actor left to signal.
        let system = match &via {
            monitor::ExitVia::Handle(system) => match system {
                Some(system) => system.clone(),
                None => {
                    drop(gate);
                    return Ok(());
                }
            },
            monitor::ExitVia::Id | monitor::ExitVia::Name => self.resolved_system(),
        };
        let resolved = monitor::resolve_target(target, &self.resolved_system());
        let (actor_id, Some(status_rx), Some(lane)) =
            (resolved.actor_id, resolved.status_rx, resolved.lane)
        else {
            drop(gate);
            return Ok(());
        };
        let own = self.stop_lane().abort_record();
        let to_self = status_rx.record().same_block(own);
        let from = PeerRef {
            actor_id: self.actor_id.clone(),
            incarnation: own.incarnation(),
        };
        // `Kill` is not trappable: it never enters an inbox. Any other signal
        // is handed to a trapping actor as a message, the flag read where the
        // signal is pushed, and only a non-trapping one is raised on below.
        if !matches!(reason, StopReason::Kill) {
            let pushed = match alias {
                None => status_rx.record().push_exit_or_trap(from, reason.clone()),
                Some(alias) => status_rx.record().push_exit_or_trap_with(
                    from,
                    reason.clone(),
                    alias.is_priority(),
                ),
            };
            match pushed {
                ExitPush::Pushed | ExitPush::Sealed => {
                    drop(gate);
                    return Ok(());
                }
                ExitPush::Full => {
                    drop(gate);
                    return Err(crate::error::ExitError::Full);
                }
                ExitPush::NotTrapping => {}
            }
        }
        let cause = match reason {
            StopReason::Kill => None,
            StopReason::Graceful if !to_self => {
                drop(gate);
                return Ok(());
            }
            other => Some(other),
        };
        let key = match via {
            monitor::ExitVia::Name => KillKey::Name(actor_id.as_str()),
            _ => KillKey::Id(&actor_id),
        };
        raise_kill_declared(
            Some(&system),
            key,
            &status_rx,
            &lane,
            cause.map_or(
                crate::system::KillFrom::Plain,
                crate::system::KillFrom::Caused,
            ),
        );
        drop(gate);
        Ok(())
    }
}
