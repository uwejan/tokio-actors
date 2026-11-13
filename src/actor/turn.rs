//! The state behind the receive timeout, the idle hook and `continue_with`.
//!
//! It lives in the lazily made monitor state of the context, so an actor that
//! sets no deadline, continues nothing and makes no alias holds none of it.
//!
//! An idle spell starts when the run loop is about to wait with the normal
//! mailbox and the priority lane empty, and ends when a user message starts a
//! turn. A request on the system channel (a status read, a link drain) neither
//! ends it nor restarts it. Each threshold fires at most once per spell.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::actor::alias::AliasShared;
use crate::actor::panic::catch_sync;
use crate::actor::runtime::{end_value, log_late_drop, saturating_deadline};
use crate::actor::Actor;

/// Which threshold of a spell fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleKind {
    /// The receive timeout: its message goes through `handle`.
    Receive,
    /// The idle hook: `on_idle` runs.
    Idle,
}

/// The function that makes the message of a receive timeout.
pub(crate) type ReceiveFn<M> = dyn Fn() -> M + Send + Sync + 'static;

/// The turn state of one actor. Every field reads as unset on an actor whose
/// context holds no monitor state.
pub(crate) struct TurnState<A: Actor> {
    /// The message the last callback asked to handle next.
    continuation: Option<A::Message>,
    /// The receive timeout: its length and the message function.
    receive: Option<(Duration, Arc<ReceiveFn<A::Message>>)>,
    /// The idle length.
    idle_after: Option<Duration>,
    /// When the current spell started.
    spell: Option<Instant>,
    /// The receive timeout fired in this spell.
    receive_fired: bool,
    /// The idle hook ran in this spell.
    idle_fired: bool,
    /// A fire is running: the spell is not ended by the turns it causes.
    inline: bool,
    /// The aliases this actor made and has not ended.
    aliases: Vec<Arc<AliasShared>>,
    /// The actor this state belongs to, named in the log of a contained drop.
    owner: crate::actor::runtime::Owner,
}

impl<A: Actor> TurnState<A> {
    pub(crate) fn new(owner: crate::actor::runtime::Owner) -> Self {
        Self {
            continuation: None,
            receive: None,
            idle_after: None,
            spell: None,
            receive_fired: false,
            idle_fired: false,
            inline: false,
            aliases: Vec::new(),
            owner,
        }
    }

    /// Whether a threshold is set, fired or not.
    pub(crate) fn armed(&self) -> bool {
        self.receive.is_some() || self.idle_after.is_some()
    }

    /// Sets the receive timeout, replacing an earlier one.
    pub(crate) fn set_receive(&mut self, after: Duration, map: Arc<ReceiveFn<A::Message>>) {
        let earlier = self.receive.replace((after, map));
        self.drop_map(earlier);
    }

    /// Clears the receive timeout.
    pub(crate) fn clear_receive(&mut self) {
        let earlier = self.receive.take();
        self.drop_map(earlier);
    }

    /// Drops a message function that was replaced or cleared, its drop caught.
    fn drop_map(&self, map: Option<(Duration, Arc<ReceiveFn<A::Message>>)>) {
        if let Some(panic) = map.and_then(|(_, map)| end_value(map)) {
            log_late_drop(&panic, &self.owner);
        }
    }

    /// Sets the idle length, replacing an earlier one.
    pub(crate) fn set_idle(&mut self, after: Duration) {
        self.idle_after = Some(after);
    }

    /// Clears the idle length.
    pub(crate) fn clear_idle(&mut self) {
        self.idle_after = None;
    }

    /// The nearest threshold that has not fired in the current spell and its
    /// instant. The spell starts now when it has not started and `quiet`
    /// says the loop is about to wait; a loop that holds a message starts
    /// none, since a turn is about to end it. When both thresholds are due
    /// together the receive timeout comes first.
    pub(crate) fn idle_next(&mut self, now: Instant, quiet: bool) -> Option<(Instant, IdleKind)> {
        if self.spell.is_none() {
            if !quiet {
                return None;
            }
            self.spell = Some(now);
        }
        let spell = self.spell?;
        let receive = match &self.receive {
            Some((after, _)) if !self.receive_fired => {
                Some((saturating_deadline(spell, *after), IdleKind::Receive))
            }
            _ => None,
        };
        let idle = match self.idle_after {
            Some(after) if !self.idle_fired => {
                Some((saturating_deadline(spell, after), IdleKind::Idle))
            }
            _ => None,
        };
        match (receive, idle) {
            (Some(r), Some(i)) => Some(if i.0 < r.0 { i } else { r }),
            (r, i) => r.or(i),
        }
    }

    /// Records that a threshold fired in this spell.
    pub(crate) fn mark_fired(&mut self, kind: IdleKind) {
        match kind {
            IdleKind::Receive => self.receive_fired = true,
            IdleKind::Idle => self.idle_fired = true,
        }
    }

    /// Marks a fire as running or done.
    pub(crate) fn set_inline(&mut self, on: bool) {
        self.inline = on;
    }

    /// Ends the spell, unless a fire is running: the turn that starts now is
    /// the fire's own.
    pub(crate) fn end_spell(&mut self) {
        if !self.inline {
            self.spell = None;
            self.receive_fired = false;
            self.idle_fired = false;
        }
    }

    /// Makes the message of the receive timeout, inside a catch. `None`: no
    /// receive timeout is set. `Some(Err)`: the function panicked.
    pub(crate) fn make_receive(&self) -> Option<Result<A::Message, String>> {
        let (_, map) = self.receive.as_ref()?;
        Some(catch_sync(|| map()).map_err(crate::actor::panic::payload_into_string))
    }

    /// Keeps `message` as the next to handle. An earlier one is dropped, its
    /// drop caught.
    pub(crate) fn set_continuation(&mut self, message: A::Message) {
        let earlier = self.continuation.replace(message);
        self.drop_value(earlier);
    }

    /// Whether a message waits to be handled next.
    pub(crate) fn has_continuation(&self) -> bool {
        self.continuation.is_some()
    }

    /// Takes the message to handle next.
    pub(crate) fn take_continuation(&mut self) -> Option<A::Message> {
        self.continuation.take()
    }

    /// Drops the message to handle next, its drop caught: a panic of the drop
    /// is contained, since the message was never delivered.
    pub(crate) fn drop_continuation(&mut self) {
        let cell = self.continuation.take();
        self.drop_value(cell);
    }

    fn drop_value(&self, value: Option<A::Message>) {
        if let Some(panic) = value.and_then(end_value) {
            log_late_drop(&panic, &self.owner);
        }
    }

    /// Records an alias this actor made, forgetting those that ended.
    pub(crate) fn add_alias(&mut self, alias: Arc<AliasShared>) {
        self.aliases.retain(|held| held.is_active());
        self.aliases.push(alias);
    }

    /// Forgets an alias.
    pub(crate) fn remove_alias(&mut self, alias: &Arc<AliasShared>) {
        self.aliases.retain(|held| !Arc::ptr_eq(held, alias));
    }

    /// The aliases made and not ended.
    pub(crate) fn alias_count(&self) -> usize {
        self.aliases.iter().filter(|held| held.is_active()).count()
    }
}

impl<A: Actor> Drop for TurnState<A> {
    fn drop(&mut self) {
        // The actor's end: no alias of it accepts a message from here on, and
        // a message waiting to be handled is dropped.
        for alias in self.aliases.drain(..) {
            alias.end();
        }
        self.drop_continuation();
        let map = self.receive.take();
        self.drop_map(map);
    }
}
