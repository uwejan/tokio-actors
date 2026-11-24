//! Sending through an alias.

use super::{cast_error, slot_error, Alias};
#[cfg(doc)]
use crate::actor::alias::AliasOptions;
use crate::actor::Actor;
use crate::error::AliasError;

impl<A: Actor> Alias<A> {
    /// Sends `msg` without waiting for a reply, waiting while the mailbox is
    /// full.
    ///
    /// Kind: a send; a stopping actor may still make it. See [`signals`](crate::signals).
    ///
    /// The slot is reserved before the alias is checked, so a send that waits
    /// for room and finds the alias ended meanwhile drops its message and
    /// returns [`AliasError::Inactive`]. A message accepted before the owner
    /// called `unalias` stays queued. A call made on the owner's own task to
    /// its own full mailbox answers [`AliasError::Full`] at once, since that
    /// task is the one that makes room.
    ///
    /// # Errors
    /// - [`AliasError::Inactive`] if `unalias` or a reply alias's message
    ///   ended the alias.
    /// - [`AliasError::Closed`] if the actor has stopped, which ends every
    ///   alias of it.
    /// - [`AliasError::Full`] if the call is made on the owner's own task and
    ///   its mailbox is full.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Alias, AliasOptions,
    /// };
    ///
    /// struct Owner;
    ///
    /// impl Actor for Owner {
    ///     type Message = tokio::sync::oneshot::Sender<Alias<Owner>>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, reply: Self::Message, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let _ = reply.send(ctx.alias(AliasOptions::default()));
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let owner = Owner.spawn().await?;
    /// let (tx, rx) = tokio::sync::oneshot::channel();
    /// owner.notify(tx).await?;
    /// let alias = rx.await?;
    /// let (tx, _rx) = tokio::sync::oneshot::channel();
    /// alias.notify(tx).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn notify(&self, msg: A::Message) -> Result<(), AliasError> {
        let permit = self.handle.reserve_cast(false).await.map_err(cast_error)?;
        self.put(permit, msg)
    }

    /// As [`notify`](Self::notify), without waiting for room.
    ///
    /// Kind: a send; a stopping actor may still make it. See [`signals`](crate::signals).
    ///
    /// `Ok` means the queue accepted the message, not that it will be
    /// handled: one accepted while the owner runs its final deliveries or
    /// `on_stopped`, or as its end closes the queue after the check, is
    /// dropped unhandled. With an error the message is dropped; keep a copy
    /// to retry.
    ///
    /// # Errors
    /// - [`AliasError::Full`] if the mailbox is full.
    /// - [`AliasError::Inactive`] if `unalias` or a reply alias's message
    ///   ended the alias.
    /// - [`AliasError::Closed`] if the actor has stopped, which ends every
    ///   alias of it.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Alias, AliasOptions,
    /// };
    ///
    /// struct Owner;
    ///
    /// impl Actor for Owner {
    ///     type Message = tokio::sync::oneshot::Sender<Alias<Owner>>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, reply: Self::Message, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let _ = reply.send(ctx.alias(AliasOptions::default()));
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let owner = Owner.spawn().await?;
    /// let (tx, rx) = tokio::sync::oneshot::channel();
    /// owner.notify(tx).await?;
    /// let alias = rx.await?;
    /// let (tx, _rx) = tokio::sync::oneshot::channel();
    /// alias.try_notify(tx)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn try_notify(&self, msg: A::Message) -> Result<(), AliasError> {
        let permit = self.handle.try_reserve_signal(false).map_err(slot_error)?;
        self.put(permit, msg)
    }

    /// Sends `msg` on the priority lane when the alias was made with
    /// [`AliasOptions::priority`] and the actor has a lane, and on the
    /// ordinary mailbox otherwise, waiting while the queue is full.
    ///
    /// Kind: a send; a stopping actor may still make it. See [`signals`](crate::signals).
    ///
    /// A message on the lane is received ahead of every ordinary message
    /// still queued, as for
    /// [`ActorHandle::notify_priority`](crate::actor::handle::ActorHandle::notify_priority).
    /// The lane is this alias's whatever the actor's
    /// [`priority_by_alias`](crate::actor::runtime::MailboxConfig::with_priority_by_alias)
    /// setting says. A call made on the owner's own task to its own full
    /// queue answers [`AliasError::Full`] at once, as
    /// [`notify`](Self::notify) does.
    ///
    /// # Errors
    /// - [`AliasError::Inactive`] if `unalias` or a reply alias's message
    ///   ended the alias.
    /// - [`AliasError::Closed`] if the actor has stopped, which ends every
    ///   alias of it.
    /// - [`AliasError::Full`] if the call is made on the owner's own task and
    ///   the queue it uses is full.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorConfig, ActorResult, Alias, AliasOptions,
    /// };
    ///
    /// struct Owner;
    ///
    /// impl Actor for Owner {
    ///     type Message = tokio::sync::oneshot::Sender<Alias<Owner>>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, reply: Self::Message, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let options = AliasOptions { priority: true, ..AliasOptions::default() };
    ///         let _ = reply.send(ctx.alias(options));
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let owner = Owner
    ///     .spawn()
    ///     .with_config(ActorConfig::default().with_priority_lane(4).with_priority_by_alias())
    ///     .await?;
    /// let (tx, rx) = tokio::sync::oneshot::channel();
    /// owner.notify(tx).await?;
    /// let alias = rx.await?;
    /// let (tx, _rx) = tokio::sync::oneshot::channel();
    /// alias.notify_priority(tx).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn notify_priority(&self, msg: A::Message) -> Result<(), AliasError> {
        let permit = self
            .handle
            .reserve_cast(self.shared.priority)
            .await
            .map_err(cast_error)?;
        self.put(permit, msg)
    }

    /// As [`notify_priority`](Self::notify_priority), without waiting for
    /// room.
    ///
    /// Kind: a send; a stopping actor may still make it. See [`signals`](crate::signals).
    ///
    /// `Ok` means the queue accepted the message, not that it will be
    /// handled: one accepted while the owner runs its final deliveries or
    /// `on_stopped`, or as its end closes the queue after the check, is
    /// dropped unhandled. With an error the message is dropped; keep a copy
    /// to retry.
    ///
    /// # Errors
    /// - [`AliasError::Full`] if the queue it uses is full.
    /// - [`AliasError::Inactive`] if `unalias` or a reply alias's message
    ///   ended the alias.
    /// - [`AliasError::Closed`] if the actor has stopped, which ends every
    ///   alias of it.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Alias, AliasOptions,
    /// };
    ///
    /// struct Owner;
    ///
    /// impl Actor for Owner {
    ///     type Message = tokio::sync::oneshot::Sender<Alias<Owner>>;
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, reply: Self::Message, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let _ = reply.send(ctx.alias(AliasOptions::default()));
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let owner = Owner.spawn().await?;
    /// let (tx, rx) = tokio::sync::oneshot::channel();
    /// owner.notify(tx).await?;
    /// let alias = rx.await?;
    /// let (tx, _rx) = tokio::sync::oneshot::channel();
    /// alias.try_notify_priority(tx)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn try_notify_priority(&self, msg: A::Message) -> Result<(), AliasError> {
        let permit = self
            .handle
            .try_reserve_signal(self.shared.priority)
            .map_err(slot_error)?;
        self.put(permit, msg)
    }
}
