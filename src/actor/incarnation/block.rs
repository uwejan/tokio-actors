//! The block every incarnation shares and the reference to it.

use super::owed::*;
use super::signals::*;
use super::subtree::*;
#[cfg(test)]
use crate::actor::envelope::SystemMessage;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// The record of the first abort issued against one incarnation's task, kept
/// beside its stop lane (every clone of the lane shares it) and outside the
/// lane's state, so it never adds a receiver to the lane.
///
/// Written once: the first abort wins, and a later abort or a re-raised
/// `Kill` never moves it. From the moment an abort is owed (a reaper entry
/// exists for the task, or a ladder raised its `Kill`) until the abort is
/// issued or the task ends, the record holds an owed mark, counted on the
/// incarnation's subtree token and on every ancestor's. The task's end
/// releases the mark when the actor ends, and when the join arm of a ladder,
/// an observer of a lost runner, the reaper's entry or the drop of the hold
/// sees the task end, whichever comes first. The record holds
/// its token weakly, so it never keeps a subtree alive, and the token's
/// shared state strongly (counters and a notification, which pin nothing), so
/// a reader can tell after the subtree ended that it did; the mark holds the
/// shared state of each token it counted on, so its release reaches them
/// whatever ended in between.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default))]
pub(crate) struct IncarnationRef {
    pub(super) shared: Arc<IncarnationBlock>,
}

#[derive(Debug)]
pub(super) struct IncarnationBlock {
    pub(super) linked: OnceLock<Linked>,
    pub(super) owed: Mutex<Owed>,
    /// The incarnation's status plane (see `StatusCell`).
    pub(super) status: crate::actor::incarnation::status_cell::StatusCell,
    /// The public name of this run, drawn from the process-wide counter when
    /// the block is made. Every spawn path makes a block per run, so a
    /// supervised restart gets a new one.
    pub(super) incarnation: crate::types::Incarnation,
    /// How many monitors other actors hold on this run that have not fired
    /// or been revoked.
    pub(super) monitored_by: std::sync::atomic::AtomicU32,
    /// The run loop's waker slot (see `WakeSlot`): woken by a raise of the
    /// stop lane.
    pub(super) reader: crate::actor::incarnation::WakeSlot,
    /// The stop lane's cell: its state, version and receiver-gone flag.
    pub(super) lane: crate::actor::incarnation::LaneCell,
    /// Woken, after the cell's lock is released, by every new version of the
    /// status plane and of the stop lane, and by the close of the plane's
    /// last sender. A waiter creates its notification before it reads, and
    /// re-checks what it waits for on every wake, so a wake meant for the
    /// other signal of the block is harmless.
    pub(super) signals: tokio::sync::Notify,
    /// The incarnation's system plane. It stores no waker: a requester wakes
    /// the run loop through `reader` after it queued its request.
    pub(super) system: Mutex<SystemSlot>,
    /// What the plane holds, as `SYSTEM_HELD` and `SYSTEM_OPEN`, written
    /// under `system`'s lock and read without it, and the flags of the
    /// signal machinery (`SYSTEM_SIGNALS` and the bits after it): they live
    /// in this one word so that the order of two operations on them is the
    /// order of the word's updates, with no lock taken by an incarnation
    /// that uses none of them.
    pub(super) system_state: std::sync::atomic::AtomicU8,
    /// Everything that only an incarnation using links, exit trapping, a
    /// process group or a signal map keeps (see `SignalInner`), made by the
    /// first use: an incarnation that uses none holds the empty cell.
    pub(super) signal_cell: OnceLock<Box<Mutex<SignalInner>>>,
    // The `sys` bundle's bits, the supervised-children count and the
    // guard-ended flag of the incarnation live in the tail padding of `lane`
    // (see `LaneCell`), so the small atomics of this block fit one word.
    /// The telemetry state of the system the incarnation was spawned in, set
    /// once before the incarnation is registered. It holds the system weakly,
    /// so a handle never keeps a system alive; it is the incarnation's way to
    /// its system and, past two relaxed loads, to its events.
    pub(super) mode: OnceLock<Arc<crate::telemetry::Mode>>,
}

impl Drop for IncarnationBlock {
    /// The last holder of the record lets go of the incarnation's identity,
    /// if the system recorded one: the identity lives exactly as long as the
    /// record that names it, so a request made through a handle after the
    /// actor's end still finds the incarnation it is made on.
    fn drop(&mut self) {
        if let Some(mode) = self.mode.get() {
            mode.release(std::ptr::from_ref::<Self>(self) as usize);
        }
    }
}

impl IncarnationBlock {
    pub(super) fn with_mode(
        mode: OnceLock<Arc<crate::telemetry::Mode>>,
        incarnation: crate::types::Incarnation,
    ) -> Self {
        Self {
            linked: OnceLock::new(),
            owed: Mutex::new(Owed::default()),
            status: crate::actor::incarnation::status_cell::StatusCell::new(),
            incarnation,
            monitored_by: std::sync::atomic::AtomicU32::new(0),
            signals: tokio::sync::Notify::new(),
            reader: crate::actor::incarnation::WakeSlot::default(),
            lane: crate::actor::incarnation::LaneCell::default(),
            system: Mutex::new(SystemSlot::default()),
            system_state: std::sync::atomic::AtomicU8::new(0),
            signal_cell: OnceLock::new(),
            mode,
        }
    }
}

/// A block a unit test builds by hand, with a number of its own. No
/// production path builds one: a spawn draws the number first and passes it
/// to [`IncarnationRef::in_system`].
#[cfg(test)]
impl Default for IncarnationBlock {
    fn default() -> Self {
        Self::with_mode(OnceLock::new(), crate::types::Incarnation::mint())
    }
}

impl IncarnationRef {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The public name of the run this block belongs to.
    pub(crate) fn incarnation(&self) -> crate::types::Incarnation {
        self.shared.incarnation
    }

    /// How many monitors other actors hold on this run.
    pub(crate) fn monitored_by(&self) -> usize {
        self.shared.monitored_by.load(Ordering::Acquire) as usize
    }

    /// Counts one more monitor held on this run.
    pub(crate) fn add_monitored_by(&self) {
        self.shared.monitored_by.fetch_add(1, Ordering::AcqRel);
    }

    /// Counts one monitor fewer, stopping at zero.
    pub(crate) fn sub_monitored_by(&self) {
        let count = &self.shared.monitored_by;
        let mut seen = count.load(Ordering::Acquire);
        while seen != 0 {
            match count.compare_exchange_weak(seen, seen - 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => seen = now,
            }
        }
    }

    /// A record of the incarnation `incarnation` (a number the spawn drew),
    /// spawned in the system whose telemetry state is `mode`. The pointer is
    /// in place before the record is shared, so recording it costs no atomic
    /// step.
    pub(crate) fn in_system(
        mode: Arc<crate::telemetry::Mode>,
        incarnation: crate::types::Incarnation,
    ) -> Self {
        Self {
            shared: Arc::new(IncarnationBlock::with_mode(
                OnceLock::from(mode),
                incarnation,
            )),
        }
    }

    /// Records the system a hand-built record belongs to. Set once: a second
    /// call changes nothing.
    #[cfg(test)]
    pub(crate) fn set_system(&self, system: &Arc<crate::system::ActorSystem>) {
        let _ = self.shared.mode.set(system.telemetry_mode().clone());
    }

    /// The telemetry state of the system the incarnation was spawned in, or
    /// `None` for a record no spawn made.
    #[inline]
    pub(crate) fn mode(&self) -> Option<&Arc<crate::telemetry::Mode>> {
        self.shared.mode.get()
    }

    /// The system the incarnation was spawned in, if it is still alive.
    pub(crate) fn system(&self) -> Option<Arc<crate::system::ActorSystem>> {
        self.shared.mode.get().and_then(|mode| mode.system())
    }

    /// The identity of the system the incarnation was spawned in: the address
    /// of its telemetry state, which the record keeps from being reused, so
    /// it is unique to one system for the life of the process; `0` for a
    /// record no spawn made.
    pub(crate) fn system_identity(&self) -> u64 {
        self.shared
            .mode
            .get()
            .map_or(0, |mode| Arc::as_ptr(mode) as usize as u64)
    }

    /// Whether the incarnation was spawned in the system whose telemetry
    /// state `system` points to. The weak reference keeps the state's
    /// allocation, so an address it holds is never reused by another system;
    /// a record no spawn made is in no system.
    pub(crate) fn spawned_in(&self, system: &Weak<crate::telemetry::Mode>) -> bool {
        self.shared
            .mode
            .get()
            .is_some_and(|mode| std::ptr::eq(Arc::as_ptr(mode), system.as_ptr()))
    }

    /// Records how many children the incarnation supervises. Written only by
    /// the incarnation's own supervision state.
    pub(crate) fn set_children(&self, count: usize) {
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        self.shared
            .lane
            .children
            .store(count, std::sync::atomic::Ordering::Relaxed);
    }

    /// How many children the incarnation supervises, as it last recorded.
    pub(crate) fn children(&self) -> usize {
        self.shared
            .lane
            .children
            .load(std::sync::atomic::Ordering::Relaxed) as usize
    }

    /// The address of the shared record, which names the incarnation in the
    /// system's telemetry table for as long as the record lives.
    #[inline]
    pub(crate) fn key(&self) -> usize {
        Arc::as_ptr(&self.shared) as usize
    }

    /// The address of the incarnation's subtree token, once the actor's hold
    /// has linked it. An address only: it is compared, never read through.
    pub(crate) fn token_addr(&self) -> Option<usize> {
        self.shared
            .linked
            .get()
            .map(|linked| linked.token.as_ptr() as usize)
    }

    /// The address of the token of the supervisor above the incarnation, when
    /// the incarnation is linked below one and its token is alive. An
    /// address only, which the supervisor's own record answers to through
    /// [`token_addr`](Self::token_addr).
    pub(crate) fn parent_token_addr(&self) -> Option<usize> {
        let token = self.shared.linked.get()?.token.upgrade()?;
        token
            .parent
            .as_ref()
            .map(|parent| Arc::as_ptr(parent) as usize)
    }

    /// Records that the incarnation has ended (see `ended`), ahead of the
    /// supervisor's book of owed deaths.
    pub(crate) fn mark_guard_ended(&self) {
        self.shared
            .lane
            .ended
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether [`mark_guard_ended`](Self::mark_guard_ended) ran.
    pub(crate) fn has_ended(&self) -> bool {
        self.shared
            .lane
            .ended
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether the incarnation's status plane reads `Stopped`.
    pub(crate) fn has_stopped(&self) -> bool {
        self.shared.status.has_stopped()
    }

    /// The incarnation's status plane storage.
    pub(crate) fn status_cell(&self) -> &crate::actor::incarnation::status_cell::StatusCell {
        &self.shared.status
    }

    /// Whether both records are the same incarnation's block.
    pub(crate) fn same_block(&self, other: &IncarnationRef) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// A sender of the plane's channel, when it exists.
    #[cfg(test)]
    pub(crate) fn system_tx_for_test(&self) -> Option<tokio::sync::mpsc::Sender<SystemMessage>> {
        match &*self.shared.system.lock().unwrap_or_else(|p| p.into_inner()) {
            SystemSlot::Open { tx, .. } => Some(tx.clone()),
            _ => None,
        }
    }

    /// The stop lane's cell.
    pub(crate) fn lane(&self) -> &crate::actor::incarnation::LaneCell {
        &self.shared.lane
    }

    /// Whether the calling task is the one that is polling the incarnation,
    /// so a request it sends to it could never be answered. A thread outside
    /// any task, another task, an ended incarnation and a later incarnation
    /// are never it (see `task_key`).
    #[inline]
    pub(crate) fn is_self_call(&self) -> bool {
        crate::actor::task_key::is_self_call(self.shared.incarnation)
    }
}
