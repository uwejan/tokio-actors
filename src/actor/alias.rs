//! Aliases: a name for sending to one run of an actor that its owner can end.
//!
//! An alias is made by the actor it reaches ([`alias`](crate::actor::context::ActorContext::alias))
//! and handed to others. Messages sent through it reach the owner's mailbox
//! until the owner calls [`unalias`](crate::actor::context::ActorContext::unalias),
//! the alias is a reply alias that has taken its one message, or the owner
//! ends. After `unalias` returns `true` no message of that alias enters the
//! mailbox; a message that entered before it stays queued.
//!
//! A send reserves its mailbox slot first, which is the only wait for room,
//! then takes the read side of the alias's guard (it waits there only while
//! `unalias` holds or is waiting for the guard), checks that
//! the alias is active and puts the message in. `unalias` takes the write
//! side, so it waits out a send
//! that is between its check and its put, and no send begins its check after
//! it. An alias holds a handle to the actor, which never keeps the actor
//! alive: an actor lives until it is stopped.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use crate::actor::handle::{ActorHandle, ForwardPermit};
use crate::actor::Actor;
use crate::error::{AliasError, TrySendError};
use crate::types::{ActorId, Incarnation};

/// The alias accepts messages.
const ACTIVE: u8 = 1;
/// The owner ended the alias.
const INACTIVE: u8 = 0;
/// A reply alias took its message.
const SPENT: u8 = 2;

/// What an alias is made with.
///
/// Erlang/OTP: the options of `alias/1`, `reply` and `priority`.
///
/// # Examples
/// ```rust
/// use tokio_actors::AliasOptions;
///
/// let options = AliasOptions { reply: true, ..AliasOptions::default() };
/// assert!(options.reply && !options.priority);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AliasOptions {
    /// The alias ends with the first message it accepts. Messages sent
    /// through it after that return [`AliasError::Inactive`].
    pub reply: bool,
    /// The alias's priority methods ride the actor's priority lane, if it has
    /// one. Without this option they use the ordinary mailbox.
    pub priority: bool,
}

/// The state an alias shares with its owner.
pub(crate) struct AliasShared {
    lock: RwLock<()>,
    state: AtomicU8,
    priority: bool,
    reply: bool,
    owner: (ActorId, Incarnation),
    /// Runs between a send's check and its put, in tests that order the two
    /// against `unalias`.
    #[cfg(test)]
    pub(crate) between: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl std::fmt::Debug for AliasShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AliasShared")
            .field("owner", &self.owner.0)
            .field("state", &self.state.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl AliasShared {
    pub(crate) fn new(options: AliasOptions, owner: (ActorId, Incarnation)) -> Self {
        Self {
            lock: RwLock::new(()),
            state: AtomicU8::new(ACTIVE),
            priority: options.priority,
            reply: options.reply,
            owner,
            #[cfg(test)]
            between: std::sync::Mutex::new(None),
        }
    }

    /// Whether the alias accepts messages now.
    pub(crate) fn is_active(&self) -> bool {
        self.state.load(Ordering::Acquire) == ACTIVE
    }

    /// Whether the alias was made with the priority option.
    pub(crate) fn is_priority(&self) -> bool {
        self.priority
    }

    /// The run the alias belongs to.
    pub(crate) fn owner(&self) -> &(ActorId, Incarnation) {
        &self.owner
    }

    /// The read side of the guard: while it is held `deactivate` waits.
    pub(crate) fn read(&self) -> RwLockReadGuard<'_, ()> {
        self.lock.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends the alias under the write side of the guard. True when it was
    /// active. When this returns no send is between its check and its put.
    pub(crate) fn deactivate(&self) -> bool {
        let _write = self.lock.write().unwrap_or_else(PoisonError::into_inner);
        self.state.swap(INACTIVE, Ordering::AcqRel) == ACTIVE
    }

    /// Ends the alias at its owner's end, without the guard. No message is
    /// read from the owner's queues after its end, so there is no send to
    /// order against, and the end waits on no sender: a message dropped under
    /// the read side, whose drop is user code, cannot delay it.
    pub(crate) fn end(&self) {
        self.state.store(INACTIVE, Ordering::Release);
    }

    /// Checks that the alias accepts a message and runs `put` under the read
    /// guard. A reply alias takes its one message here, by compare-exchange.
    /// `None`: the alias does not accept messages, and `put` did not run.
    pub(crate) fn enter<R>(&self, put: impl FnOnce() -> R) -> Option<R> {
        let _read = self.read();
        let accepted = if self.reply {
            self.state
                .compare_exchange(ACTIVE, SPENT, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        } else {
            self.is_active()
        };
        if !accepted {
            return None;
        }
        #[cfg(test)]
        {
            let hook = self
                .between
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(hook) = hook {
                hook();
            }
        }
        Some(put())
    }
}

/// A name for sending to one run of an actor, made by that actor with
/// [`alias`](crate::actor::context::ActorContext::alias).
///
/// The actor ends the alias with
/// [`unalias`](crate::actor::context::ActorContext::unalias); a reply alias
/// ends itself with the first message it accepts; every alias ends with its
/// owner. A send through an alias that `unalias` or its reply message ended
/// returns [`AliasError::Inactive`] and the message is dropped; once the owner
/// has ended, its mailbox is closed and a send returns [`AliasError::Closed`]
/// first. Once `unalias` has returned `true` no message of that alias is
/// queued after it. An alias
/// never keeps its actor alive.
///
/// Erlang/OTP: the reference `alias/1` returns. Where OTP drops a message sent
/// through an inactive alias without a word, a send here returns the result.
///
/// # Examples
/// ```rust
/// use tokio::sync::oneshot;
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     Alias, AliasError, AliasOptions, ActorResult,
/// };
///
/// enum Msg {
///     Alias(oneshot::Sender<Alias<Owner>>),
///     End,
///     Ping,
/// }
///
/// struct Owner {
///     alias: Option<Alias<Owner>>,
///     pings: u32,
/// }
///
/// impl Actor for Owner {
///     type Message = Msg;
///     type Response = u32;
///
///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
///         match msg {
///             Msg::Alias(reply) => {
///                 let alias = ctx.alias(AliasOptions::default());
///                 self.alias = Some(alias.clone());
///                 let _ = reply.send(alias);
///             }
///             Msg::End => {
///                 if let Some(alias) = self.alias.take() {
///                     ctx.unalias(&alias);
///                 }
///             }
///             Msg::Ping => self.pings += 1,
///         }
///         Ok(self.pings)
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let owner = Owner { alias: None, pings: 0 }.spawn().await?;
/// let (tx, rx) = oneshot::channel();
/// owner.send(Msg::Alias(tx)).await?;
/// let alias = rx.await?;
/// alias.notify(Msg::Ping).await?;
/// assert_eq!(owner.send(Msg::End).await?, 1);
/// assert_eq!(alias.notify(Msg::Ping).await, Err(AliasError::Inactive));
/// # Ok(())
/// # }
/// ```
pub struct Alias<A: Actor> {
    handle: ActorHandle<A>,
    shared: Arc<AliasShared>,
}

impl<A: Actor> Clone for Alias<A> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            shared: self.shared.clone(),
        }
    }
}

impl<A: Actor> std::fmt::Debug for Alias<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Alias")
            .field("owner", &self.shared.owner.0)
            .field("active", &self.shared.is_active())
            .field("priority", &self.shared.priority)
            .field("reply", &self.shared.reply)
            .finish()
    }
}

impl<A: Actor> Alias<A> {
    pub(crate) fn new(handle: ActorHandle<A>, shared: Arc<AliasShared>) -> Self {
        Self { handle, shared }
    }

    /// The state the alias shares with its owner.
    pub(crate) fn shared(&self) -> &Arc<AliasShared> {
        &self.shared
    }

    /// The handle the alias sends through.
    pub(crate) fn handle(&self) -> &ActorHandle<A> {
        &self.handle
    }

    /// Puts `msg` in the permit's queue if the alias still accepts messages.
    /// The put runs under the read side of the guard, and a push that finds
    /// the queue closed drops the message there, so a message whose drop is
    /// slow holds the owner's `unalias` for that long. The owner's end takes
    /// no part in the guard. The permit is dropped after the guard is
    /// released.
    fn put(&self, permit: ForwardPermit<'_, A>, msg: A::Message) -> Result<(), AliasError> {
        match self.shared.enter(move || permit.send(msg)) {
            Some(()) => Ok(()),
            None => Err(AliasError::Inactive),
        }
    }

    /// Sends `msg` without waiting for a reply, waiting while the mailbox is
    /// full.
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

    /// Whether the alias accepts messages now: it was not ended by its owner,
    /// it is not a reply alias that took its message, and its owner has not
    /// ended.
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
    /// assert!(rx.await?.is_active());
    /// # Ok(())
    /// # }
    /// ```
    pub fn is_active(&self) -> bool {
        self.shared.is_active()
    }
}

/// The alias error of a waiting cast's refusal.
fn cast_error(err: crate::error::SendError) -> AliasError {
    match err {
        crate::error::SendError::Full => AliasError::Full,
        crate::error::SendError::Closed | crate::error::SendError::NotFound => AliasError::Closed,
    }
}

fn slot_error(err: TrySendError) -> AliasError {
    match err {
        TrySendError::Full => AliasError::Full,
        TrySendError::Closed => AliasError::Closed,
    }
}
