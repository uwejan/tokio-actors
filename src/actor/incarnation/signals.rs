//! The signal cell, the system plane and the wake slot of an incarnation.

use super::block::*;
#[cfg(feature = "sys")]
use crate::actor::envelope::SysCommand;
use crate::actor::envelope::{SuperviseRequest, SystemMessage};
use crate::actor::panic::contain_waker_panic;
use crate::types::StopReason;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// What an incarnation that uses the signal machinery keeps, under the lock
/// of its cell.
#[derive(Debug, Default)]
pub(super) struct SignalInner {
    /// The signal inbox and the pair table of the links this incarnation
    /// owns.
    pub(super) block: crate::actor::link::SignalBlock,
    /// How many process-group memberships this run holds: each join not yet
    /// left counts one. Read by the registry guard at the run's end to learn
    /// whether any scope holds the member.
    pub(super) pg_joined: usize,
    /// The first panic of a signal map function (a monitor's, a group
    /// monitor's) that failed the actor, kept where the actor's stop can read
    /// it however late the panic landed. See `keep_map_panic`.
    pub(super) map_panic: Option<String>,
    /// The lengths of the signal tables the actor itself holds, indexed by
    /// [`Coupling`]. The actor writes a length whenever its table changes, so
    /// a snapshot shows them without asking the actor.
    pub(super) coupling: [u32; 3],
    /// The exit signals sent with `exit` that this run has queued and whose
    /// delivery has not yet taken a slot of its mailbox: counted by the push,
    /// given back by the [`ExitSlot`](crate::actor::link::ExitSlot) each one
    /// carries from the drain on.
    pub(super) exits: usize,
    /// The bound of `exits`: the mailbox capacity the actor recorded when it
    /// called `trap_exit`; 0 until then, read as [`DEFAULT_EXIT_CAP`].
    pub(super) exit_cap: usize,
}

/// The bound of the exit signals waiting for an actor that traps exits
/// without having called `trap_exit` (a supervisor traps from its spawn):
/// the default mailbox capacity.
pub(super) const DEFAULT_EXIT_CAP: usize = 64;

/// One of the signal tables an actor holds itself, whose length a snapshot
/// shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Coupling {
    /// The monitors the actor holds.
    Monitor = 0,
    /// The links the actor holds.
    Link = 1,
    /// The group and scope monitors the actor holds.
    GroupMonitor = 2,
}

/// The plane holds a request outside its channel.
pub(super) const SYSTEM_HELD: u8 = 1;
/// The plane's channel exists.
pub(super) const SYSTEM_OPEN: u8 = 2;
/// The signal inbox holds items the run loop has not taken.
pub(super) const SYSTEM_SIGNALS: u8 = 4;
/// The signal inbox is sealed: its owner took the last items it will take,
/// so a later push is refused.
pub(super) const SYSTEM_SEALED: u8 = 8;
/// The actor traps exits. Written by `set_trap` and read by the senders of
/// exit signals.
pub(super) const SYSTEM_TRAP: u8 = 16;
/// A push or a pair was begun: the signal cell may hold an inbox or a pair
/// table. Set, under the cell's lock, by every push and pair before it reads
/// `SYSTEM_SEALED`, so a seal that reads this bit clear in the word it
/// updated knows that every later push finds the seal.
pub(super) const SYSTEM_BLOCK: u8 = 32;
/// A join of a process group was begun: the cell may hold a membership.
pub(super) const SYSTEM_PG_USED: u8 = 64;
/// The run ended: no join is accepted from here on.
pub(super) const SYSTEM_PG_ENDED: u8 = 128;
/// What the end of a run sets: the inbox sealed and the groups closed. The
/// first end site sets both in one update and takes what the inbox holds, so
/// a word that shows both bits shows that an earlier update did.
pub(super) const SYSTEM_ENDED: u8 = SYSTEM_SEALED | SYSTEM_PG_ENDED;
/// The capacity of a plane's channel: in steady state a plane queues this
/// many requests, as many as the system channel of an actor without a held
/// slot does. The request held before the channel opened is taken by the run
/// loop with the channel's receiver, so a plane whose run loop has not yet
/// polled it holds that one request beside a full channel.
pub(super) const SYSTEM_CAPACITY: usize = 64;

pub(super) type StatusReply = tokio::sync::oneshot::Sender<Box<crate::types::ActorStatusInfo>>;

/// The actor is suspended: its loop takes no message.
#[allow(dead_code)]
pub(crate) const SYS_SUSPENDED: u8 = 1;
/// The actor was spawned with state access.
pub(crate) const SYS_ALLOW_STATE: u8 = 2;
/// The actor is sensitive.
pub(crate) const SYS_SENSITIVE: u8 = 4;
/// The handler that runs stopped its own actor with `ActorContext::stop`, so
/// the reply it returns is held to the seal of the stop lane. Written and read
/// only by the actor's own task; it shares the word with the `sys` bits so
/// the flag costs no byte of the context or the block.
pub(super) const SELF_STOP: u8 = 8;
/// The incarnation was spawned as a supervised child, not as a root. Written
/// once, with the spawn's other bits, before the task exists.
pub(crate) const SYS_CHILD: u8 = 16;
/// An application's start took the incarnation as its top. Set once, by
/// `IncarnationRef::take_as_top`, and never cleared.
pub(super) const SYS_TAKEN: u8 = 32;

/// The incarnation's system plane. A query to an actor that has none queued
/// is held in the slot itself, so the first `get_status` allocates nothing
/// for the plane; a request that finds one held creates the channel, which
/// takes every later request. The held request is the oldest, and is
/// answered first.
#[derive(Debug, Default)]
pub(super) enum SystemSlot {
    #[default]
    Idle,
    /// One request, held without a channel.
    One(crate::types::StatusRequest),
    /// The channel exists. `first` is the request held before it was created,
    /// until the run loop takes it.
    Open {
        first: Option<crate::types::StatusRequest>,
        tx: tokio::sync::mpsc::Sender<SystemMessage>,
        rx: Option<tokio::sync::mpsc::Receiver<SystemMessage>>,
    },
    Closed,
}

/// The run loop's end of the incarnation's system plane. The held request is
/// taken from the slot and the channel's receiver is taken here on the next
/// poll; dropping this closes the plane, so a request queued and not
/// answered fails, and a later `get_status` is refused.
///
/// The plane is read by its state bits and by emptiness and never registers
/// a waker of its own: a requester wakes the run loop through the
/// incarnation's waker slot after it queued its request
/// (`IncarnationRef::submit_status`), and the run loop's stop branch arms that
/// slot earlier in the same select poll, so a request queued after this read
/// finds the slot armed, and one queued before it is seen by it.
///
/// The channel behind a full plane is tokio's bounded channel. A requester
/// that waits for room (past `SYSTEM_CAPACITY` queued requests) is woken by
/// tokio in two places. The close of the channel, at the plane's drop, is made
/// inside a catch (see [`contain_waker_panic`]): tokio ends its wake of the
/// parked requesters at a waker that panics, and the requesters behind it may
/// stay unwoken, so their futures finish at their next poll, which finds the
/// plane closed. The run loop's own receive is the other: it frees the slot a parked
/// requester takes, and it returns the request it took, which a catch around
/// it would drop, so a panic of that requester's waker there is outside the
/// catches the crate puts on the wakes it makes itself (see
/// [`IncarnationRef::notify_signals`]). A request held outside the channel is
/// dropped inside a catch as well, and so is one queued in the channel when
/// the plane ends.
///
/// The priority lane is tokio's bounded channel too, and a `send_priority`
/// caller that waits for room on a full lane is woken by tokio in the same two
/// places: the close of the lane at the end of the message loop is made inside
/// a catch, with the same limit (the senders behind a waker that panics may
/// stay unwoken, and find the lane closed at their next poll), and the run loop's
/// receive from the lane frees the slot a parked sender takes, outside a
/// catch, for the same reason.
#[derive(Debug)]
pub(crate) struct SystemPlane {
    pub(super) record: IncarnationRef,
    pub(super) rx: Option<tokio::sync::mpsc::Receiver<SystemMessage>>,
    /// The actor the plane belongs to, named by the logs of its end.
    pub(super) owner: crate::types::ActorId,
    /// The system whose helper slot takes a receiver that ends with a slot
    /// still out.
    pub(super) system: std::sync::Arc<crate::system::ActorSystem>,
}

impl SystemPlane {
    pub(crate) fn new(
        record: IncarnationRef,
        owner: crate::types::ActorId,
        system: std::sync::Arc<crate::system::ActorSystem>,
    ) -> Self {
        Self {
            record,
            rx: None,
            owner,
            system,
        }
    }

    pub(super) fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<SystemMessage>> {
        let shared = &self.record.shared;
        let seen = shared.system_state.load(Ordering::Acquire);
        if seen & SYSTEM_SIGNALS != 0 {
            return std::task::Poll::Ready(Some(SystemMessage::Signals));
        }
        if seen & SYSTEM_HELD != 0 {
            let mut slot = shared.system.lock().unwrap_or_else(|p| p.into_inner());
            let held = match &mut *slot {
                SystemSlot::One(_) => match std::mem::take(&mut *slot) {
                    SystemSlot::One(held) => Some(held),
                    _ => None,
                },
                SystemSlot::Open { first, .. } => first.take(),
                _ => None,
            };
            if held.is_some() {
                shared
                    .system_state
                    .fetch_and(!SYSTEM_HELD, Ordering::Release);
            }
            drop(slot);
            if let Some(request) = held {
                return std::task::Poll::Ready(Some(SystemMessage::GetStatus(request)));
            }
        }
        #[cfg(test)]
        between_loads();
        if self.rx.is_none() {
            if shared.system_state.load(Ordering::Acquire) & SYSTEM_OPEN == 0 {
                return std::task::Poll::Pending;
            }
            let mut slot = shared.system.lock().unwrap_or_else(|p| p.into_inner());
            if let SystemSlot::Open { first, rx, .. } = &mut *slot {
                self.rx = rx.take();
                // A request held before the channel opened is older than
                // every request in it, and may have been queued after this
                // poll's read of the held bit: it is handed out first.
                if let Some(request) = first.take() {
                    shared
                        .system_state
                        .fetch_and(!SYSTEM_HELD, Ordering::Release);
                    drop(slot);
                    return std::task::Poll::Ready(Some(SystemMessage::GetStatus(request)));
                }
            }
        }
        match self.rx.as_mut() {
            // Empty and open: pend without registering on the channel.
            Some(rx) if rx.is_empty() && !rx.is_closed() => std::task::Poll::Pending,
            Some(rx) => rx.poll_recv(cx),
            // Open, and the receiver is not in the slot: only this
            // owner takes it, so this is not reached.
            None => std::task::Poll::Pending,
        }
    }

    /// Whether the run loop's poll of the plane would pend now: no request
    /// held, and no channel opened, or its receiver taken here, empty and
    /// open. A channel opened and not yet taken reads busy, so the poll takes
    /// its receiver.
    pub(crate) fn is_idle(&self) -> bool {
        let state = self.record.shared.system_state.load(Ordering::Acquire);
        if state & (SYSTEM_HELD | SYSTEM_SIGNALS) != 0 {
            return false;
        }
        match &self.rx {
            Some(rx) => rx.is_empty() && !rx.is_closed(),
            None => state & SYSTEM_OPEN == 0,
        }
    }

    /// The next request on the plane; pends while no request is queued.
    pub(crate) async fn recv(&mut self) -> Option<SystemMessage> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Drops a request held outside the channel, inside a catch (see
    /// [`contain_waker_panic`]).
    pub(super) fn drop_held(&self, request: Option<crate::types::StatusRequest>) {
        contain_waker_panic("status request", Some(self.owner.as_who()), move || {
            drop(request)
        });
    }
}

impl Drop for SystemPlane {
    fn drop(&mut self) {
        // The plane's contents are moved out under the lock and dropped after
        // it is released: the channel's close, the wakes of parked requesters
        // and the drops of queued requests (which wake their requesters) run
        // outside the lock.
        let open = {
            let mut slot = self
                .record
                .shared
                .system
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            std::mem::replace(&mut *slot, SystemSlot::Closed)
        };
        // A request held outside the channel is dropped, and so is the
        // slot's sender: the plane does not rely on a requester's sender
        // having gone out of scope. The drop of a request wakes its
        // requester, whose waker belongs to the requester's task, so it runs
        // inside a catch: a panic of that waker must not turn the actor's
        // reason into a failure, and the plane is dropped before the reason is
        // final. Each receiver, the one taken here and the one still in the
        // slot, is ended like any queue: closed and drained, and handed to a
        // task that drops what a sender pushes through a slot it reserved
        // before the close.
        let slot_rx = match open {
            SystemSlot::Open { first, tx, rx } => {
                self.drop_held(first);
                drop(tx);
                rx
            }
            SystemSlot::One(request) => {
                self.drop_held(Some(request));
                None
            }
            SystemSlot::Idle | SystemSlot::Closed => None,
        };
        for rx in self.rx.take().into_iter().chain(slot_rx) {
            if let Some(panic) = crate::actor::runtime::end_queue_late(
                rx,
                &self.system,
                &crate::actor::runtime::Owner::of(&self.owner, &self.record),
                Some(&self.record),
            ) {
                crate::actor::panic::log_waker_panic(
                    "status queue",
                    &panic,
                    Some(self.owner.as_who()),
                );
            }
        }
    }
}

// Runs once, between the two reads of the plane's state in a poll, when a
// test installs it: it lets a test queue requests inside that window.
#[cfg(test)]
thread_local! {
    pub(super) static BETWEEN_LOADS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn between_loads() {
    if let Some(hook) = BETWEEN_LOADS.with(|h| h.borrow_mut().take()) {
        hook();
    }
}

impl IncarnationRef {
    /// The cell of the signal machinery, made by the first use.
    pub(super) fn cell(&self) -> &Mutex<SignalInner> {
        self.shared.signal_cell.get_or_init(Box::default)
    }

    pub(super) fn lock_cell(&self) -> std::sync::MutexGuard<'_, SignalInner> {
        self.cell().lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The cell, when some use made it.
    pub(super) fn existing_cell(&self) -> Option<&Mutex<SignalInner>> {
        self.shared.signal_cell.get().map(|cell| &**cell)
    }

    pub(super) fn state_bits(&self) -> u8 {
        self.shared.system_state.load(Ordering::SeqCst)
    }

    /// Keeps the panic of a signal map function that fails the actor, the
    /// first one only. Every site that raises such a `Failure` on the stop
    /// lane calls this first, because the raise reaches no reader once the
    /// actor's message loop has ended; the stop reads the panic once, after
    /// the last moment a map function can run (`map_panic`).
    pub(crate) fn keep_map_panic(&self, panic: String) {
        let mut inner = self.lock_cell();
        if inner.map_panic.is_none() {
            inner.map_panic = Some(panic);
        }
    }

    /// Records the length of one of the actor's own signal tables. Called on
    /// the actor's own task when the table changes; the first call with a
    /// length above zero makes the signal cell.
    pub(crate) fn set_coupling(&self, kind: Coupling, len: u32) {
        if len == 0 && self.existing_cell().is_none() {
            // Nothing was recorded, so zero already reads; no cell is made.
            return;
        }
        self.lock_cell().coupling[kind as usize] = len;
    }

    /// The lengths last recorded, in the order monitors, links, group
    /// monitors; zeros for an incarnation that never used a signal. Makes no
    /// cell.
    #[cfg(feature = "telemetry")]
    pub(crate) fn coupling(&self) -> [u32; 3] {
        self.existing_cell().map_or([0; 3], |cell| {
            cell.lock().unwrap_or_else(|p| p.into_inner()).coupling
        })
    }

    /// Records the bound of the exit signals that may wait for this actor:
    /// its mailbox capacity. Called by `trap_exit` on the actor's own task;
    /// the first call makes the signal cell.
    pub(crate) fn set_exit_cap(&self, cap: usize) {
        self.lock_cell().exit_cap = cap;
    }

    /// Gives back one exit signal counted by the push, once its delivery took
    /// a mailbox slot, ended without one, or was dropped. Takes the cell's
    /// lock only; wakes nothing and runs no user code.
    pub(crate) fn release_exit(&self) {
        if let Some(cell) = self.existing_cell() {
            let mut inner = cell.lock().unwrap_or_else(|p| p.into_inner());
            inner.exits = inner.exits.saturating_sub(1);
        }
    }

    /// The first panic kept by `keep_map_panic`, if any. An incarnation that
    /// never kept one has no cell to look in.
    pub(crate) fn map_panic(&self) -> Option<String> {
        let cell = self.existing_cell()?;
        cell.lock()
            .unwrap_or_else(|p| p.into_inner())
            .map_panic
            .clone()
    }

    /// Whether the actor traps exits.
    pub(crate) fn trap(&self) -> bool {
        self.state_bits() & SYSTEM_TRAP != 0
    }

    /// Sets or clears the trap flag, ordered against the senders of exit
    /// signals: a sender reads the flag under the cell's lock, after it
    /// marked the cell in use, and this write then waits out a sender that
    /// is inside the lock. A sender therefore sees the flag either before or
    /// after this write, and none pushes once this call has returned.
    /// Allocates nothing.
    ///
    /// Turning the flag on first raises the subtree budget above the actor by
    /// its trapping window, or, for a worker under `Shutdown::Infinity`, counts
    /// it among the unbounded stops until the actor ends, so no wait that
    /// reads the flag can have a bound that does not cover the orderly stop
    /// the flag asks for.
    pub(crate) fn set_trap(&self, on: bool) {
        if on {
            if let Some(linked) = self.shared.linked.get() {
                if let (Some(spec), Some(token)) = (linked.trap.as_deref(), linked.token.upgrade())
                {
                    token.raise_for_trap(spec);
                }
            }
        }
        let before = if on {
            self.shared
                .system_state
                .fetch_or(SYSTEM_TRAP, Ordering::SeqCst)
        } else {
            self.shared
                .system_state
                .fetch_and(!SYSTEM_TRAP, Ordering::SeqCst)
        };
        if before & SYSTEM_BLOCK != 0 {
            if let Some(cell) = self.existing_cell() {
                drop(cell.lock().unwrap_or_else(|p| p.into_inner()));
            }
        }
    }

    /// How many pairs the table of this block holds.
    #[cfg(test)]
    pub(crate) fn pairs_for_test(&self) -> usize {
        self.existing_cell().map_or(0, |cell| {
            cell.lock()
                .unwrap_or_else(|p| p.into_inner())
                .block
                .pairs
                .len()
        })
    }

    /// Whether items wait in the signal inbox.
    pub(crate) fn signals_pending(&self) -> bool {
        self.shared.system_state.load(Ordering::Acquire) & SYSTEM_SIGNALS != 0
    }

    /// The state of the link between this incarnation and `peer`, found in
    /// the block of the lower incarnation of the two, where both ends look.
    /// True when this call made it. A sealed block cannot keep a table, so
    /// it hands out a state of its own; its owner is ending and holds no
    /// entry for the pair.
    pub(crate) fn pair_state(
        &self,
        own: crate::types::Incarnation,
        peer: &IncarnationRef,
    ) -> (Arc<crate::actor::link::LinkState>, bool) {
        let peer_incarnation = peer.incarnation();
        if own < peer_incarnation {
            self.table_state(peer_incarnation)
        } else {
            peer.table_state(own)
        }
    }

    pub(super) fn table_state(
        &self,
        higher: crate::types::Incarnation,
    ) -> (Arc<crate::actor::link::LinkState>, bool) {
        let mut inner = self.lock_cell();
        let before = self
            .shared
            .system_state
            .fetch_or(SYSTEM_BLOCK, Ordering::SeqCst);
        if before & SYSTEM_SEALED != 0 {
            return (Arc::new(crate::actor::link::LinkState::new()), true);
        }
        match inner.block.pairs.entry(higher) {
            std::collections::btree_map::Entry::Occupied(found) => (found.get().clone(), false),
            std::collections::btree_map::Entry::Vacant(room) => {
                let state = Arc::new(crate::actor::link::LinkState::new());
                room.insert(state.clone());
                (state, true)
            }
        }
    }

    /// Forgets the pair between `own` and `peer` when the table still holds
    /// `state` for it; called on the record of the lower incarnation. A state
    /// of an earlier link between the same two runs is never matched by the
    /// pair's key alone.
    pub(crate) fn remove_pair(
        &self,
        own: crate::types::Incarnation,
        peer: crate::types::Incarnation,
        state: &Arc<crate::actor::link::LinkState>,
    ) {
        let Some(cell) = self.existing_cell() else {
            return;
        };
        let removed = {
            let mut inner = cell.lock().unwrap_or_else(|p| p.into_inner());
            let key = own.max(peer);
            match inner.block.pairs.get(&key) {
                Some(held) if Arc::ptr_eq(held, state) => inner.block.pairs.remove(&key),
                _ => None,
            }
        };
        drop(removed);
    }

    /// Queues `item` in the inbox and wakes the run loop, once the lock is
    /// released. The run loop's waker is the actor's own task's, and the
    /// slot contains a panic of its wake.
    pub(super) fn push(
        &self,
        item: crate::actor::link::InboxItem,
    ) -> crate::actor::link::PushResult {
        {
            let mut inner = self.lock_cell();
            let before = self
                .shared
                .system_state
                .fetch_or(SYSTEM_BLOCK, Ordering::SeqCst);
            if before & SYSTEM_SEALED != 0 {
                drop(inner);
                drop(item);
                return crate::actor::link::PushResult::Sealed;
            }
            inner.block.inbox.push_back(item);
            self.shared
                .system_state
                .fetch_or(SYSTEM_SIGNALS, Ordering::Release);
        }
        self.shared.reader.wake();
        crate::actor::link::PushResult::Pushed
    }

    /// Queues a link for this incarnation to install. `Sealed`: its owner is
    /// ending, and the caller's entry reads this incarnation's end itself.
    pub(crate) fn push_link(
        &self,
        item: crate::actor::link::InboxItem,
    ) -> crate::actor::link::PushResult {
        self.push(item)
    }

    /// Queues the cancel of a link.
    pub(crate) fn push_unlink(
        &self,
        from: crate::types::Incarnation,
        state: Arc<crate::actor::link::LinkState>,
    ) -> crate::actor::link::PushResult {
        self.push(crate::actor::link::InboxItem::Unlink { from, state })
    }

    /// Queues an exit signal when the actor traps exits, reading the flag
    /// under the cell's lock; pushes nothing otherwise. A read of the word
    /// that already shows a seal or no trap answers without the lock, as a
    /// push that came before the write or the seal would have. A trapping
    /// actor for which as many exit signals wait as its bound allows (see
    /// `exit_cap`) accepts none beyond them: `Full`, and nothing is queued.
    pub(crate) fn push_exit_or_trap(
        &self,
        from: crate::types::PeerRef,
        reason: crate::types::StopReason,
    ) -> crate::actor::link::ExitPush {
        self.push_exit_or_trap_with(from, reason, false)
    }

    /// As [`push_exit_or_trap`](Self::push_exit_or_trap); `priority` asks that
    /// a trapping receiver with a priority lane be given the exit through it.
    pub(crate) fn push_exit_or_trap_with(
        &self,
        from: crate::types::PeerRef,
        reason: crate::types::StopReason,
        priority: bool,
    ) -> crate::actor::link::ExitPush {
        let seen = self.state_bits();
        if seen & SYSTEM_SEALED != 0 {
            return crate::actor::link::ExitPush::Sealed;
        }
        if seen & SYSTEM_TRAP == 0 {
            return crate::actor::link::ExitPush::NotTrapping;
        }
        {
            let mut inner = self.lock_cell();
            let before = self
                .shared
                .system_state
                .fetch_or(SYSTEM_BLOCK, Ordering::SeqCst);
            if before & SYSTEM_SEALED != 0 {
                return crate::actor::link::ExitPush::Sealed;
            }
            if before & SYSTEM_TRAP == 0 {
                return crate::actor::link::ExitPush::NotTrapping;
            }
            let cap = match inner.exit_cap {
                0 => DEFAULT_EXIT_CAP,
                cap => cap,
            };
            if inner.exits >= cap {
                return crate::actor::link::ExitPush::Full;
            }
            inner.exits += 1;
            inner
                .block
                .inbox
                .push_back(crate::actor::link::InboxItem::Exit {
                    from,
                    reason,
                    priority,
                });
            self.shared
                .system_state
                .fetch_or(SYSTEM_SIGNALS, Ordering::Release);
        }
        self.shared.reader.wake();
        crate::actor::link::ExitPush::Pushed
    }

    /// Takes everything queued in the inbox, in order, and clears the
    /// pending mark.
    pub(crate) fn take_signals(&self) -> Vec<crate::actor::link::InboxItem> {
        if self.state_bits() & SYSTEM_BLOCK == 0 {
            return Vec::new();
        }
        let mut inner = self.lock_cell();
        self.shared
            .system_state
            .fetch_and(!SYSTEM_SIGNALS, Ordering::Release);
        std::mem::take(&mut inner.block.inbox).into()
    }

    /// Marks the word with `bits` (which include `SYSTEM_SEALED`) in one
    /// update, then takes what the cell holds when the word showed a push or
    /// a pair: later pushes and pairs are refused, the pair table is dropped
    /// outside the lock, and the inbox is returned. A word that showed none
    /// needs no lock: every later push marks the word before it reads the
    /// seal.
    pub(super) fn seal_with(&self, bits: u8) -> Vec<crate::actor::link::InboxItem> {
        let before = self.shared.system_state.fetch_or(bits, Ordering::SeqCst);
        if before & SYSTEM_BLOCK == 0 {
            return Vec::new();
        }
        self.take_after_seal()
    }

    /// Marks the run ended for the inbox and the groups in one update and
    /// returns the word the update read. What a push or a pair left is taken
    /// and dropped, outside the lock; a word that showed none builds nothing.
    pub(super) fn mark_ended(&self) -> u8 {
        let before = self
            .shared
            .system_state
            .fetch_or(SYSTEM_ENDED, Ordering::SeqCst);
        if before & SYSTEM_BLOCK != 0 {
            drop(self.take_after_seal());
        }
        before
    }

    /// What the cell holds after a seal that found the word showing a push or
    /// a pair: the pair table is dropped outside the lock and the inbox is
    /// returned.
    #[cold]
    #[inline(never)]
    pub(super) fn take_after_seal(&self) -> Vec<crate::actor::link::InboxItem> {
        let Some(cell) = self.existing_cell() else {
            return Vec::new();
        };
        let (inbox, pairs) = {
            let mut inner = cell.lock().unwrap_or_else(|p| p.into_inner());
            self.shared
                .system_state
                .fetch_and(!SYSTEM_SIGNALS, Ordering::Release);
            (
                std::mem::take(&mut inner.block.inbox),
                std::mem::take(&mut inner.block.pairs),
            )
        };
        drop(pairs);
        inbox.into()
    }

    /// As [`take_signals`](Self::take_signals), and seals the block: later
    /// pushes are refused and the pair table is dropped.
    pub(crate) fn take_and_seal_signals(&self) -> Vec<crate::actor::link::InboxItem> {
        self.seal_with(SYSTEM_SEALED)
    }

    /// Seals the block and drops what it holds, each item's drop outside the
    /// lock. Idempotent. The actor's context calls it as it drops, so no join
    /// of a process group is accepted from here on either. A word that shows
    /// both end bits was updated by an earlier call (the registry guard's),
    /// which took what the inbox held, so this one only loads. A word with no
    /// cell proves nothing: a push can make the cell after a read of the
    /// word, and only an update of the word orders it against the seal.
    pub(crate) fn seal_signals(&self) {
        if self.state_bits() & SYSTEM_ENDED == SYSTEM_ENDED {
            return;
        }
        self.mark_ended();
    }

    /// Whether some use made the signal cell: a push, a pair, a kept map
    /// panic, a recorded coupling count or an exit bound.
    #[inline]
    pub(crate) fn has_signal_cell(&self) -> bool {
        self.existing_cell().is_some()
    }

    /// Whether a push or a pair was ever begun on this block: the signal cell
    /// may hold an inbox or a pair table.
    #[inline]
    pub(crate) fn has_signal_state(&self) -> bool {
        self.state_bits() & SYSTEM_BLOCK != 0
    }

    /// The reason a pending `Kill` on the stop lane was raised on behalf of,
    /// if an exit signal raised it: the reason the actor ends with in place
    /// of `Kill`. Read by exactly three sites: the seal of the run loop, the
    /// abort guard's write and the status receiver's read of a plane that
    /// closed without a reason.
    pub(crate) fn kill_cause(&self) -> Option<StopReason> {
        self.shared.lane.kill_cause()
    }

    /// How many process-group memberships this run holds.
    pub(crate) fn pg_joined(&self) -> usize {
        if self.state_bits() & SYSTEM_PG_USED == 0 {
            return 0;
        }
        self.lock_cell().pg_joined
    }

    /// A join begins: counts one membership, unless the run has ended, in
    /// which case it counts nothing and returns `false`. A join and the end
    /// both update the one word, so of the two, at least one sees the other.
    pub(crate) fn pg_join_begin(&self) -> bool {
        let mut inner = self.lock_cell();
        let before = self
            .shared
            .system_state
            .fetch_or(SYSTEM_PG_USED, Ordering::SeqCst);
        if before & SYSTEM_PG_ENDED != 0 {
            return false;
        }
        inner.pg_joined += 1;
        true
    }

    /// One membership fewer, stopping at zero: a leave, or a join that did
    /// not complete.
    pub(crate) fn pg_join_end(&self) {
        if self.state_bits() & SYSTEM_PG_USED == 0 {
            return;
        }
        let mut inner = self.lock_cell();
        inner.pg_joined = inner.pg_joined.saturating_sub(1);
    }

    /// The run ends: no join is accepted from here on, and the inbox is
    /// sealed in the same update (what it holds is dropped outside the lock,
    /// and holds no user code). Returns how many memberships were counted,
    /// read after the mark. A run whose context sealed the block already
    /// takes no further update of the word; a run that never joined pays this
    /// one update, which the context's seal then finds done.
    pub(crate) fn pg_mark_ended(&self) -> usize {
        let seen = self.state_bits();
        let seen = if seen & SYSTEM_ENDED == SYSTEM_ENDED {
            seen
        } else {
            self.mark_ended()
        };
        if seen & SYSTEM_PG_USED == 0 {
            return 0;
        }
        self.lock_cell().pg_joined
    }

    /// The `sys` bundle's bits of the incarnation.
    #[allow(dead_code)]
    pub(crate) fn sys_bits(&self) -> u8 {
        self.shared
            .lane
            .sys_bits
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Sets the bits in `bits`, leaving the others.
    pub(crate) fn set_sys_bits(&self, bits: u8) {
        self.shared
            .lane
            .sys_bits
            .fetch_or(bits, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the incarnation was spawned as a root, an actor with no
    /// supervisor (see [`SYS_CHILD`]).
    pub(crate) fn is_root(&self) -> bool {
        self.sys_bits() & SYS_CHILD == 0
    }

    /// Takes the incarnation as an application's top: `true` when no start
    /// took it before, with one `fetch_or`. Each incarnation is taken at most
    /// once in the process.
    pub(crate) fn take_as_top(&self) -> bool {
        self.shared
            .lane
            .sys_bits
            .fetch_or(SYS_TAKEN, Ordering::AcqRel)
            & SYS_TAKEN
            == 0
    }

    /// Marks that the running handler stopped its own actor (see
    /// [`SELF_STOP`]).
    pub(crate) fn mark_self_stop(&self) {
        self.shared
            .lane
            .sys_bits
            .fetch_or(SELF_STOP, Ordering::Relaxed);
    }

    /// Whether the handler that just returned stopped its own actor; leaves
    /// the mark.
    pub(crate) fn self_stop_set(&self) -> bool {
        self.shared.lane.sys_bits.load(Ordering::Relaxed) & SELF_STOP != 0
    }

    /// Whether the handler that just returned stopped its own actor; reads
    /// the mark once and clears it. An unmarked word is only read.
    pub(crate) fn take_self_stop(&self) -> bool {
        if !self.self_stop_set() {
            return false;
        }
        self.shared
            .lane
            .sys_bits
            .fetch_and(!SELF_STOP, Ordering::Relaxed);
        true
    }

    /// Forgets the mark, before a request is handled.
    pub(crate) fn clear_self_stop(&self) {
        let _ = self.take_self_stop();
    }

    /// Sets or clears one bit.
    #[allow(dead_code)]
    pub(crate) fn set_sys_bit(&self, bit: u8, on: bool) {
        use std::sync::atomic::Ordering;
        if on {
            self.shared.lane.sys_bits.fetch_or(bit, Ordering::Relaxed);
        } else {
            self.shared.lane.sys_bits.fetch_and(!bit, Ordering::Relaxed);
        }
    }

    /// A future woken by the next [`notify_signals`](Self::notify_signals).
    /// Create it before reading what it waits for, so a change after the read
    /// wakes it.
    pub(crate) fn signalled(&self) -> tokio::sync::futures::Notified<'_> {
        self.shared.signals.notified()
    }

    /// Wakes every task waiting on the block's signals, after the lock that
    /// guarded the change is released: a stop lane's new version, a status
    /// plane's new version and the close of the plane. The waiters are other
    /// tasks' (`wait_stopped`, the waits on the status plane, a bounded
    /// stop's wait for the end), and a waker belongs to the task that polls
    /// with it. A panic of one waker's wake is contained here (see
    /// [`contain_waker_panic`]): the raise that is waking, the actor that is
    /// publishing its status and the drop that closes the plane go on, and a
    /// request or a status that was written stays written. Every raise and
    /// every plane write wakes through this call, and nothing else calls
    /// `notify_waiters` on the notification.
    pub(crate) fn notify_signals(&self) {
        contain_waker_panic("signals", None, || self.shared.signals.notify_waiters());
    }

    /// The run loop's waker slot.
    pub(crate) fn reader(&self) -> &crate::actor::incarnation::WakeSlot {
        &self.shared.reader
    }

    /// Queues a status request on the incarnation's system plane, waiting for
    /// room when the plane is full, then wakes the run loop with no await
    /// between the queueing and the wake. `Err` once the run loop closed the
    /// plane.
    ///
    /// No clone of the plane's sender lives across the requester's await on
    /// the reply: the one cloned here goes out of scope when this call
    /// returns, so once the run loop closes the plane and the last sender is
    /// gone, a request still queued is freed and its requester gets an
    /// answer instead of waiting on a value nobody will take. The plane's end
    /// does not depend on this: it also drains the queue and hands a
    /// receiver with slots still reserved to a task that drops late values.
    pub(crate) async fn submit_status(&self, reply: StatusReply) -> Result<(), ()> {
        // Each request carries its own requester kind, so the reply to it
        // reads that request's kind whatever else is queued.
        let request = crate::types::StatusRequest {
            reply,
            task: tokio::task::try_id().is_some(),
        };
        let tx = {
            let mut slot = self.shared.system.lock().unwrap_or_else(|p| p.into_inner());
            match std::mem::take(&mut *slot) {
                SystemSlot::Closed => {
                    *slot = SystemSlot::Closed;
                    return Err(());
                }
                SystemSlot::Idle => {
                    *slot = SystemSlot::One(request);
                    self.shared
                        .system_state
                        .fetch_or(SYSTEM_HELD, Ordering::Release);
                    drop(slot);
                    self.shared.reader.wake();
                    return Ok(());
                }
                SystemSlot::One(first) => {
                    let (tx, rx) = tokio::sync::mpsc::channel(SYSTEM_CAPACITY);
                    *slot = SystemSlot::Open {
                        first: Some(first),
                        tx: tx.clone(),
                        rx: Some(rx),
                    };
                    self.shared
                        .system_state
                        .fetch_or(SYSTEM_OPEN, Ordering::Release);
                    tx
                }
                SystemSlot::Open { first, tx, rx } => {
                    let sender = tx.clone();
                    *slot = SystemSlot::Open { first, tx, rx };
                    sender
                }
            }
        };
        let permit = crate::actor::panic::reserve_held("status queue", None, &tx, |_| {})
            .await
            .ok_or(())?;
        permit.into_inner().send(SystemMessage::GetStatus(request));
        self.shared.reader.wake();
        Ok(())
    }

    /// Queues a request of a `SupervisorRef` on the incarnation's system
    /// plane, waiting for room when the plane is full, then wakes the run
    /// loop with no await between the queueing and the wake. `Err` once the
    /// run loop closed the plane.
    ///
    /// The request always goes through the plane's channel, which is made
    /// here when no status request made it yet, so requests of one sender
    /// reach the run loop in the order they were queued, and a request held
    /// for the first status query never reorders them. As for a status
    /// request, no clone of the sender lives across the caller's await on its
    /// answer.
    pub(crate) async fn submit_supervise(&self, request: SuperviseRequest) -> Result<(), ()> {
        let tx = {
            let mut slot = self.shared.system.lock().unwrap_or_else(|p| p.into_inner());
            match std::mem::take(&mut *slot) {
                SystemSlot::Closed => {
                    *slot = SystemSlot::Closed;
                    return Err(());
                }
                SystemSlot::Idle => {
                    let (tx, rx) = tokio::sync::mpsc::channel(SYSTEM_CAPACITY);
                    *slot = SystemSlot::Open {
                        first: None,
                        tx: tx.clone(),
                        rx: Some(rx),
                    };
                    self.shared
                        .system_state
                        .fetch_or(SYSTEM_OPEN, Ordering::Release);
                    tx
                }
                SystemSlot::One(first) => {
                    let (tx, rx) = tokio::sync::mpsc::channel(SYSTEM_CAPACITY);
                    *slot = SystemSlot::Open {
                        first: Some(first),
                        tx: tx.clone(),
                        rx: Some(rx),
                    };
                    self.shared
                        .system_state
                        .fetch_or(SYSTEM_OPEN, Ordering::Release);
                    tx
                }
                SystemSlot::Open { first, tx, rx } => {
                    let sender = tx.clone();
                    *slot = SystemSlot::Open { first, tx, rx };
                    sender
                }
            }
        };
        let permit = crate::actor::panic::reserve_held("supervise queue", None, &tx, |_| {})
            .await
            .ok_or(())?;
        permit
            .into_inner()
            .send(SystemMessage::Supervise(Box::new(request)));
        self.shared.reader.wake();
        Ok(())
    }

    /// Queues a `sys` command on the incarnation's system plane, waiting for
    /// room when the plane is full, then wakes the run loop with no await
    /// between the queueing and the wake. `Err` once the run loop closed the
    /// plane.
    ///
    /// The held slot takes status requests only, so a command always goes
    /// through the channel: a plane with nothing held opens it, a plane that
    /// holds a status request opens it and keeps that request as the oldest,
    /// answered first, exactly as [`submit_status`](Self::submit_status)
    /// does. As there, no clone of the plane's sender outlives this call, so
    /// the caller's wait for the reply never keeps the channel open.
    #[cfg(feature = "sys")]
    pub(crate) async fn submit_sys(&self, command: SysCommand) -> Result<(), ()> {
        let message = SystemMessage::Sys(Box::new(command));
        let tx = {
            let mut slot = self.shared.system.lock().unwrap_or_else(|p| p.into_inner());
            match std::mem::take(&mut *slot) {
                SystemSlot::Closed => {
                    *slot = SystemSlot::Closed;
                    return Err(());
                }
                SystemSlot::Idle => {
                    let (tx, rx) = tokio::sync::mpsc::channel(SYSTEM_CAPACITY);
                    *slot = SystemSlot::Open {
                        first: None,
                        tx: tx.clone(),
                        rx: Some(rx),
                    };
                    self.shared
                        .system_state
                        .fetch_or(SYSTEM_OPEN, Ordering::Release);
                    tx
                }
                SystemSlot::One(first) => {
                    let (tx, rx) = tokio::sync::mpsc::channel(SYSTEM_CAPACITY);
                    *slot = SystemSlot::Open {
                        first: Some(first),
                        tx: tx.clone(),
                        rx: Some(rx),
                    };
                    self.shared
                        .system_state
                        .fetch_or(SYSTEM_OPEN, Ordering::Release);
                    tx
                }
                SystemSlot::Open { first, tx, rx } => {
                    let sender = tx.clone();
                    *slot = SystemSlot::Open { first, tx, rx };
                    sender
                }
            }
        };
        // The slot is held as every slot of a bounded channel is (see
        // `reserve_held`): a caller that waits on a full plane is woken by the
        // run loop's receive, and its waker's panic stays out of that loop.
        let permit = crate::actor::panic::reserve_held("sys queue", None, &tx, |_| {})
            .await
            .ok_or(())?;
        permit.into_inner().send(message);
        self.shared.reader.wake();
        Ok(())
    }
}
