//! The per-system gate and the state behind it.

use std::collections::HashMap;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::event::{EventKind, TelemetryEvent};
use super::identity::Identity;
use super::pump::Pump;
use super::sinks::{SinkTables, Sinks};
use super::wait_graph::WaitGraph;
use crate::system::ActorSystem;

/// The watchdog control bit: turns of actors are timed for the stuck-turn
/// watchdog.
pub(crate) const CTL_WATCHDOG: u64 = 1 << 56;
/// The turn-tracking control bit: turns are counted and flagged in flight.
pub(crate) const CTL_TRACK_TURNS: u64 = 1 << 57;
/// The wait-cycle control bit: sends that wait are recorded in the graph.
pub(crate) const CTL_DEADLOCK: u64 = 1 << 58;
/// The class bits of the word: one per [`EventKind`].
pub(crate) const CLASS_MASK: u64 = (1 << 32) - 1;
/// The bits of the word behind which a site that has no id in hand needs the
/// identity of an incarnation: a sink kind, turn tracking, wait detection.
/// While none is set no such site looks one up.
pub(crate) const RESOLVING: u64 = CLASS_MASK | CTL_TRACK_TURNS | CTL_DEADLOCK;

/// The mode word and its generation, alone on a cache line. Bits 0 to 31 of
/// the word are the class (one bit per event kind some sink takes); bits 56
/// to 58 are the control bits.
#[derive(Debug, Default)]
#[repr(align(64))]
struct Gate {
    word: AtomicU64,
    /// Bumped on every recompute, after the class bits are stored.
    generation: AtomicU64,
    /// How many times the resolving bits of the word went from none to some,
    /// bumped right after the change of the word that did it.
    engagements: AtomicU64,
}

/// The telemetry state of one system: the class word that gates every emit
/// site, the sinks and the pump.
pub(crate) struct Mode {
    /// The class word that gates every emit site.
    gate: Gate,
    /// The sends that wait on other actors.
    pub(crate) wait: WaitGraph,
    /// The system monitor's `long_schedule` threshold in nanoseconds, 0 when
    /// off.
    pub(super) monitor_long_schedule_nanos: AtomicU64,
    /// The system monitor's mailbox lengths: `enable` in the high half,
    /// `disable` in the low half, 0 when off.
    pub(super) monitor_queue: AtomicU64,
    pub(crate) sinks: Sinks,
    pub(crate) pump: Pump,
    /// Held weakly; asserted unwind safe because only the pump upgrades it.
    system: OnceLock<AssertUnwindSafe<Weak<ActorSystem>>>,
    /// The name of the system the mode is bound to, for display by a handle
    /// that outlives its system; empty for a mode bound to none.
    name: Box<str>,
    /// The identities of the incarnations spawned while a sink took events,
    /// and of those the registry identified when an event first needed them,
    /// by the address of each incarnation's abort record; an entry lives until
    /// the last holder of the record releases it. A leaf lock: nothing is called with
    /// it held.
    live: Mutex<HashMap<usize, Arc<Identity>>>,
    /// The number of entries of `live`, which a site reads with the class
    /// word before it takes the lock.
    live_count: AtomicUsize,
    /// How many times a control bit changed value, for a reader that must
    /// know a switch moved while it read.
    ctl_changes: AtomicU64,
    /// The engagement whose pass has run: the value of `engagements` read
    /// before the walk of that pass began.
    bound: AtomicU64,
    /// Held while a pass walks the registry, so one engagement has one pass.
    /// It is taken first and held while the pass takes the registry's shard
    /// locks, one at a time, and the lock of `live`; no lock of the crate is
    /// held when it is taken, and nothing takes it under another lock.
    binding: Mutex<()>,
}

impl fmt::Debug for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mode")
            .field("word", &self.gate.word.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Mode {
    /// A disarmed mode with no sinks, bound to no system.
    pub(crate) fn new() -> Arc<Mode> {
        Arc::new(Self::build(""))
    }

    fn build(name: &str) -> Mode {
        Mode {
            gate: Gate::default(),
            wait: WaitGraph::default(),
            monitor_long_schedule_nanos: AtomicU64::new(0),
            monitor_queue: AtomicU64::new(0),
            sinks: Sinks::default(),
            pump: Pump::default(),
            system: OnceLock::new(),
            name: Box::from(name),
            live: Mutex::new(HashMap::new()),
            live_count: AtomicUsize::new(0),
            ctl_changes: AtomicU64::new(0),
            bound: AtomicU64::new(0),
            binding: Mutex::new(()),
        }
    }

    /// A disarmed mode with no sinks, bound to the system `me` belongs to.
    /// The system's constructor calls this, so every event raised for the
    /// system has a system to start its pump on or to be counted as dropped
    /// against.
    pub(crate) fn new_bound(me: Weak<ActorSystem>, name: &str) -> Arc<Mode> {
        let mode = Arc::new(Self::build(name));
        let _ = mode.system.set(AssertUnwindSafe(me));
        mode
    }

    /// The name of the system the mode is bound to.
    pub(crate) fn system_name(&self) -> &str {
        &self.name
    }

    /// Whether some sink takes events of any kind: one relaxed load. The
    /// control bits above the class do not count.
    #[inline]
    pub(crate) fn any_armed(&self) -> bool {
        self.gate.word.load(Ordering::Relaxed) & CLASS_MASK != 0
    }

    /// Whether this system takes events of any kind or holds an identity:
    /// two relaxed loads, the whole cost of an emit site while telemetry is
    /// off.
    #[inline]
    pub(crate) fn engaged(&self) -> bool {
        self.any_armed() || self.live_count.load(Ordering::Relaxed) != 0
    }

    /// Records the identity of the incarnation behind the record at `key`.
    pub(crate) fn bind(&self, key: usize, identity: Arc<Identity>) {
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if live.insert(key, identity).is_none() {
            self.live_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records `identity` for `key` unless one is recorded, and returns the
    /// one that stands: two sites resolving one incarnation at once end with
    /// the same identity.
    pub(crate) fn bind_if_absent(&self, key: usize, identity: Arc<Identity>) -> Arc<Identity> {
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = live.get(&key) {
            return existing.clone();
        }
        live.insert(key, identity.clone());
        self.live_count.fetch_add(1, Ordering::Relaxed);
        identity
    }

    /// The identity recorded for `key`, if one was recorded at the spawn or
    /// since, and the record still lives.
    #[inline]
    pub(crate) fn find(&self, key: usize) -> Option<Arc<Identity>> {
        if self.live_count.load(Ordering::Relaxed) == 0 {
            return None;
        }
        self.find_recorded(key)
    }

    #[inline(never)]
    fn find_recorded(&self, key: usize) -> Option<Arc<Identity>> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()
    }

    /// Removes the identity recorded for `key` and returns it. With nothing
    /// recorded, one relaxed load.
    #[inline]
    pub(crate) fn release(&self, key: usize) -> Option<Arc<Identity>> {
        if self.live_count.load(Ordering::Relaxed) == 0 {
            return None;
        }
        self.release_recorded(key)
    }

    #[inline(never)]
    fn release_recorded(&self, key: usize) -> Option<Arc<Identity>> {
        let removed = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        if removed.is_some() {
            self.live_count.fetch_sub(1, Ordering::Relaxed);
        }
        removed
    }

    /// The number of identities recorded.
    #[cfg(test)]
    pub(crate) fn live_len(&self) -> usize {
        self.live_count.load(Ordering::Relaxed)
    }

    /// Whether some sink takes events of `kind`: one relaxed load.
    #[inline]
    pub(crate) fn armed(&self, kind: EventKind) -> bool {
        self.gate.word.load(Ordering::Relaxed) & kind.bit() != 0
    }

    /// The mode word: class bits and control bits, one relaxed load.
    #[inline]
    pub(crate) fn word(&self) -> u64 {
        self.gate.word.load(Ordering::Relaxed)
    }

    /// Sets or clears the control bit `bit`. The class bits are not touched.
    pub(crate) fn set_ctl(&self, bit: u64, on: bool) {
        debug_assert_eq!(bit & CLASS_MASK, 0, "a control bit is above the class");
        let before = if on {
            self.gate.word.fetch_or(bit, Ordering::AcqRel)
        } else {
            self.gate.word.fetch_and(!bit, Ordering::AcqRel)
        };
        if (before & bit != 0) != on {
            self.ctl_changes.fetch_add(1, Ordering::AcqRel);
        }
        if on {
            self.note_engaged(before, before | bit);
        }
    }

    /// Counts an engagement when one change of the word took its resolving
    /// bits from none to some. The bump follows that change, so a reader
    /// that acquires the bumped count sees the word as changed.
    fn note_engaged(&self, before: u64, after: u64) {
        if before & RESOLVING == 0 && after & RESOLVING != 0 {
            self.gate.engagements.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Whether the system took up resolving identities since the last pass
    /// recorded every registered incarnation: two loads.
    #[inline]
    pub(crate) fn bind_owed(&self) -> bool {
        self.bound.load(Ordering::Acquire) != self.gate.engagements.load(Ordering::Acquire)
    }

    /// Marks every engagement so far as passed, for a system whose registry
    /// is still empty (its constructor switched tracking or detection on).
    pub(crate) fn mark_bound(&self) {
        let _pass = self.binding.lock().unwrap_or_else(|p| p.into_inner());
        self.bound.store(
            self.gate.engagements.load(Ordering::Acquire),
            Ordering::Release,
        );
    }

    /// Runs `pass` once for the engagements counted so far, unless a pass for
    /// them already ran. The count is read before the walk and stored after
    /// it, so an engagement counted while the walk runs is left owed: its own
    /// pass walks again and sees every spawn the first walk missed. A caller
    /// that finds a pass running waits for it, which is at most one walk.
    /// Never call it with the registry mutex of the sinks held, or under a
    /// registry guard.
    pub(crate) fn bind_once(&self, pass: impl FnOnce()) {
        let _pass = self.binding.lock().unwrap_or_else(|p| p.into_inner());
        let target = self.gate.engagements.load(Ordering::Acquire);
        if self.bound.load(Ordering::Relaxed) == target {
            return;
        }
        pass();
        self.bound.store(target, Ordering::Release);
    }

    /// The pass of an engagement, run by the call that engaged once it holds
    /// no lock: every registered incarnation gets its identity recorded, so a
    /// site with no id in hand finds one. Nothing when no pass is owed or the
    /// system is gone.
    pub(crate) fn engage_pass(&self) {
        if !self.bind_owed() {
            return;
        }
        if let Some(system) = self.system() {
            system.bind_registered();
        }
    }

    /// The number of times a control bit changed value so far: two reads
    /// that return the same number saw no switch move between them.
    pub(crate) fn ctl_changes(&self) -> u64 {
        self.ctl_changes.load(Ordering::Acquire)
    }

    /// The system monitor's `long_schedule` threshold in nanoseconds, 0 when
    /// off.
    pub(crate) fn monitor_long_schedule_nanos(&self) -> u64 {
        self.monitor_long_schedule_nanos.load(Ordering::Relaxed)
    }

    /// The system monitor's mailbox lengths, packed: `enable` in the high
    /// half, `disable` in the low half, 0 when off.
    pub(crate) fn monitor_queue(&self) -> u64 {
        self.monitor_queue.load(Ordering::Relaxed)
    }

    /// The number of recomputes so far: moves whenever the registries change.
    pub(crate) fn generation(&self) -> u64 {
        self.gate.generation.load(Ordering::Acquire)
    }

    /// Stores the union of the kinds the registries take as the class bits,
    /// leaving the control bits as they are, and bumps the generation. Called
    /// only with the registry mutex held, `tables` being what it guards.
    pub(crate) fn recompute(&self, tables: &SinkTables) {
        let mut bits = 0u64;
        for entry in tables.handlers.iter() {
            bits |= entry.kinds.bits();
        }
        for entry in tables.subs.iter() {
            bits |= entry.filter.kinds.bits();
        }
        #[cfg(test)]
        {
            bits |= self.sinks.test_bits();
        }
        let mut old = self.gate.word.load(Ordering::Relaxed);
        loop {
            let next = (old & !CLASS_MASK) | (bits & CLASS_MASK);
            match self.gate.word.compare_exchange_weak(
                old,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.note_engaged(old, next);
                    break;
                }
                Err(seen) => old = seen,
            }
        }
        self.gate.generation.fetch_add(1, Ordering::Release);
    }

    /// Records the system this mode belongs to, once; a mode a system built
    /// is bound already. Tests bind the modes they build by hand.
    #[cfg(test)]
    pub(crate) fn bind_system(&self, system: &Arc<ActorSystem>) {
        if self.system.get().is_some() {
            return;
        }
        let _ = self.system.set(AssertUnwindSafe(Arc::downgrade(system)));
    }

    /// The system this mode belongs to, if it is bound and alive.
    pub(crate) fn system(&self) -> Option<Arc<ActorSystem>> {
        self.system.get().and_then(|weak| weak.0.upgrade())
    }

    /// Whether the mode is bound to a system that has been dropped.
    pub(crate) fn system_gone(&self) -> bool {
        self.system
            .get()
            .is_some_and(|weak| weak.0.strong_count() == 0)
    }

    /// Builds the event and delivers it to the sinks at once, on the calling
    /// thread; returns without building when no sink takes `kind`. Never
    /// waits. Only the pump's own continuations call it (the pump runs no
    /// crate step, but a handler that blocks there holds a runtime worker);
    /// every other site of the
    /// library goes through [`push`](Self::push), because a handler must never
    /// run on a path that still owes the crate a step.
    #[inline]
    pub(crate) fn dispatch(
        &self,
        kind: EventKind,
        build: impl FnOnce() -> TelemetryEvent<'static>,
    ) {
        if !self.armed(kind) {
            return;
        }
        let event = build();
        self.sinks.deliver(self, kind, event);
    }

    /// Builds the event and queues it for the pump; returns without building
    /// when no sink takes `kind`. Runs no user code, so it is safe in a drop
    /// and under a lock.
    #[inline]
    pub(crate) fn push(
        self: &Arc<Self>,
        kind: EventKind,
        build: impl FnOnce() -> TelemetryEvent<'static>,
    ) {
        if !self.armed(kind) {
            return;
        }
        // An event raised from inside a handler of this system is skipped, so
        // a handler that raises on every event cannot feed the pump. It is
        // dropped like any other: counted, and carried to the subscriptions
        // that would have taken it. A handler of another system does not
        // make this one skip.
        if super::handlers::inside_handler_of(self) {
            let event = build();
            self.pump.drop_event(self, kind, &event);
            return;
        }
        let event = build();
        self.pump.enqueue(self, kind, event);
    }

    /// [`push`](Self::push) for an event about the incarnation `owner`: the
    /// incarnation's count of events in the pump is raised with the event's
    /// acceptance and lowered after its delivery.
    #[inline]
    pub(crate) fn push_owned(
        self: &Arc<Self>,
        owner: &Arc<Identity>,
        kind: EventKind,
        build: impl FnOnce() -> TelemetryEvent<'static>,
    ) {
        if !self.armed(kind) {
            return;
        }
        let event = build();
        self.push_built_owned(owner, kind, event);
    }

    /// [`push_built`](Self::push_built) for an event about the incarnation
    /// `owner` (see [`push_owned`](Self::push_owned)).
    #[inline]
    pub(crate) fn push_built_owned(
        self: &Arc<Self>,
        owner: &Arc<Identity>,
        kind: EventKind,
        event: TelemetryEvent<'static>,
    ) {
        if super::handlers::inside_handler_of(self) {
            self.pump.drop_event(self, kind, &event);
            return;
        }
        self.pump
            .enqueue_for(self, kind, event, Some(owner.clone()));
    }

    /// Queues an event already built, as [`push`](Self::push) does.
    #[inline]
    pub(crate) fn push_built(self: &Arc<Self>, kind: EventKind, event: TelemetryEvent<'static>) {
        if super::handlers::inside_handler_of(self) {
            self.pump.drop_event(self, kind, &event);
            return;
        }
        self.pump.enqueue(self, kind, event);
    }
}

impl Drop for Mode {
    /// The last holder is gone, so no helper will run again: whatever is still
    /// queued (a push from a thread with no runtime, or one that raced the
    /// helper's last take) is counted as dropped and carried, never lost
    /// silently.
    fn drop(&mut self) {
        self.pump.discard_queued(self);
    }
}

/// Holds a system's [`Mode`] and closes its pump when the system drops.
#[derive(Debug)]
pub(crate) struct ModeOwner(pub(crate) Arc<Mode>);

impl ModeOwner {
    /// A new owner of a disarmed mode, bound to the system `me` belongs to.
    pub(crate) fn bound(me: Weak<ActorSystem>, name: &str) -> Self {
        Self(Mode::new_bound(me, name))
    }

    /// A new owner of a disarmed mode, bound to no system.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self(Mode::new())
    }
}

impl Drop for ModeOwner {
    fn drop(&mut self) {
        self.0.pump.close();
    }
}
