//! Aliases: `alias`, `unalias` and `exit_priority`.

use std::sync::Arc;

use super::ActorContext;
use crate::actor::alias::{Alias, AliasOptions, AliasShared};
use crate::actor::monitor::MonitorState;
use crate::actor::Actor;
use crate::types::StopReason;

impl<A: Actor> ActorContext<A> {
    /// Makes an alias of this actor: a name others can send to, which this
    /// actor can end with [`unalias`](Self::unalias).
    ///
    /// Kind: a registration of a source of the actor's own; accepted while the actor is stopping, and it then produces nothing. See [`signals`](crate::signals).
    ///
    /// The alias accepts messages until `unalias` returns `true` for it, it
    /// is a reply alias ([`AliasOptions::reply`]) that has taken its one
    /// message, or this actor ends. It never keeps the actor alive. With
    /// [`AliasOptions::priority`] its priority sends ride the actor's
    /// priority lane, which stays the alias's own when the actor's
    /// [`priority_by_alias`](crate::actor::runtime::MailboxConfig::with_priority_by_alias)
    /// option is set.
    ///
    /// Erlang/OTP: `alias/1`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::oneshot;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Alias, AliasOptions,
    /// };
    ///
    /// struct Owner;
    ///
    /// impl Actor for Owner {
    ///     type Message = oneshot::Sender<Alias<Owner>>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, reply: Self::Message, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let options = AliasOptions { reply: true, ..AliasOptions::default() };
    ///         let _ = reply.send(ctx.alias(options));
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let owner = Owner.spawn().await?;
    /// let (tx, rx) = oneshot::channel();
    /// owner.notify(tx).await?;
    /// let alias = rx.await?;
    /// let (first, _first_rx) = oneshot::channel();
    /// alias.notify(first).await?;
    /// let (second, _second_rx) = oneshot::channel();
    /// assert!(alias.notify(second).await.is_err());
    /// # Ok(())
    /// # }
    /// ```
    pub fn alias(&mut self, options: AliasOptions) -> Alias<A> {
        let own = self.stop_lane().abort_record().incarnation();
        let shared = Arc::new(AliasShared::new(options, (self.actor_id.clone(), own)));
        self.turn_mut().add_alias(shared.clone());
        Alias::new(self.self_handle.clone(), shared)
    }

    /// Ends `alias`. Returns `true` when it was active and this actor made it.
    ///
    /// Kind: a revoke, answers `bool`. See [`signals`](crate::signals).
    ///
    /// When this returns `true` no message of the alias enters the mailbox
    /// afterwards; a message that entered before it stays queued and is
    /// handled. A send that was between its check and its put is waited for.
    /// An alias that is not active, or that another actor made, returns
    /// `false` and changes nothing.
    ///
    /// Erlang/OTP: `unalias/1`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::oneshot;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Alias, AliasOptions,
    /// };
    ///
    /// struct Owner;
    ///
    /// impl Actor for Owner {
    ///     type Message = oneshot::Sender<bool>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, reply: Self::Message, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let alias: Alias<Owner> = ctx.alias(AliasOptions::default());
    ///         let first = ctx.unalias(&alias);
    ///         let second = ctx.unalias(&alias);
    ///         let _ = reply.send(first && !second);
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let owner = Owner.spawn().await?;
    /// let (tx, rx) = oneshot::channel();
    /// owner.notify(tx).await?;
    /// assert!(rx.await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn unalias(&mut self, alias: &Alias<A>) -> bool {
        let shared = alias.shared();
        let own = self.stop_lane().abort_record().incarnation();
        let (id, incarnation) = shared.owner();
        if *id != self.actor_id || *incarnation != own {
            return false;
        }
        let was_active = shared.deactivate();
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.remove_alias(shared);
        }
        was_active
    }

    /// Sends `target`'s actor an exit signal carrying `reason`, as
    /// [`exit`](Self::exit) does, and asks that a trapping receiver gets it
    /// ahead of its ordinary messages.
    ///
    /// Kind: a send; a stopping actor may still make it. See [`signals`](crate::signals).
    ///
    /// The signal rides the receiver's priority lane only when all of this
    /// holds: `target` is active and was made with
    /// [`AliasOptions::priority`], the receiver traps exits, and the
    /// receiver has a priority lane. Otherwise it behaves as
    /// [`exit`](Self::exit) does for the alias's actor: through the ordinary
    /// mailbox for a trapping receiver, and as an end for one that does not
    /// trap. A signal sent through an alias that is not active, because
    /// `unalias` ended it or its owner ended, has no effect, and `unalias` is
    /// ordered against this call: once it returned `true` no signal of that
    /// alias is queued or takes effect.
    ///
    /// Erlang/OTP: `exit/3` with the `priority` option and an alias as the
    /// destination; a signal sent to an inactive alias is dropped.
    ///
    /// # Errors
    /// - [`ExitError::Full`](crate::error::ExitError::Full) as for
    ///   [`exit`](Self::exit): the receiver traps exit signals and as many
    ///   signals already wait for it as its bound allows.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::oneshot;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorError, ActorResult, Alias, AliasOptions, StopReason,
    /// };
    ///
    /// struct Owner;
    ///
    /// impl Actor for Owner {
    ///     type Message = oneshot::Sender<Alias<Owner>>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, reply: Self::Message, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let options = AliasOptions { priority: true, ..AliasOptions::default() };
    ///         let _ = reply.send(ctx.alias(options));
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Sender;
    ///
    /// impl Actor for Sender {
    ///     type Message = Alias<Owner>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, alias: Alias<Owner>, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.exit_priority(&alias, StopReason::Failure(ActorError::user("signalled")))
    ///             .expect("a target that does not trap never refuses");
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let owner = Owner.spawn().await?;
    /// let (tx, rx) = oneshot::channel();
    /// owner.notify(tx).await?;
    /// let alias = rx.await?;
    /// let sender = Sender.spawn().await?;
    /// sender.send(alias).await?;
    /// assert!(matches!(owner.wait_stopped().await, StopReason::Failure(_)));
    /// # Ok(())
    /// # }
    /// ```
    pub fn exit_priority<B: Actor>(
        &mut self,
        target: &Alias<B>,
        reason: StopReason,
    ) -> Result<(), crate::error::ExitError> {
        let handle = target.handle();
        self.exit_with(&handle, reason, Some(&**target.shared()))
    }
}
