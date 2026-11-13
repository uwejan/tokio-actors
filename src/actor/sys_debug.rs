//! The debug state of one incarnation's run loop and the calls that change
//! it. The state lives in the context's lazily made extras, so an actor with
//! no debug option holds none of it, and it changes only between two turns:
//! a call is served at a turn boundary, never inside a handler.

use std::sync::{Mutex, PoisonError};
use std::time::Instant;

#[cfg(not(feature = "tracing"))]
use super::panic::discard;
use super::panic::{catch_sync, send_contained};
use super::runtime::Owner;
use super::sys::{clamp_log_capacity, DebugEntry, DebugEvent, DebugOptions, Install, Statistics};
use super::sys_loop::LoopSys;
use crate::telemetry::handlers::drop_contained;
use crate::types::{Reply, StopReason};

/// A function installed on the entries, with the id it was installed under.
pub(crate) type Installed = (String, Box<dyn FnMut(&DebugEntry) -> Install + Send>);

/// The counters of the statistics.
#[derive(Debug)]
pub(crate) struct Stats {
    pub(crate) since: Instant,
    pub(crate) messages_in: u64,
    pub(crate) messages_out: u64,
    pub(crate) turns: u64,
}

impl Stats {
    pub(crate) fn new() -> Self {
        Self {
            since: Instant::now(),
            messages_in: 0,
            messages_out: 0,
            turns: 0,
        }
    }
}

/// The log: the last entries, oldest first.
///
/// The vector grows to twice the capacity and then drops the older half in
/// one move, so a push costs the same however full the log is, and the
/// entries a caller sees are always the last `capacity` of them.
#[derive(Debug)]
pub(crate) struct Ring {
    entries: Vec<DebugEntry>,
    capacity: usize,
}

impl Ring {
    pub(crate) fn new(capacity: u16) -> Self {
        Self {
            entries: Vec::new(),
            capacity: usize::from(capacity),
        }
    }

    fn push(&mut self, entry: DebugEntry) {
        if self.entries.len() >= self.capacity * 2 {
            self.entries.drain(..self.capacity);
        }
        self.entries.push(entry);
    }

    fn visible(&self) -> &[DebugEntry] {
        &self.entries[self.entries.len().saturating_sub(self.capacity)..]
    }
}

/// What the run loop keeps while any debug option is on.
pub(crate) struct DebugState {
    stats: Option<Stats>,
    ring: Option<Ring>,
    trace: bool,
    /// Behind a lock only so the state is `Sync`, as the context's shared
    /// empty set needs; the loop reaches it with `get_mut`, which takes no
    /// lock.
    installed: Mutex<Vec<Installed>>,
    /// The actor, named by the log line of an installed function that
    /// panicked; set when the first function is installed.
    owner: Option<Owner>,
    seq: u64,
    /// The entries of the turn in flight that a trace will raise once the
    /// turn returned: at most the message taken and the reply sent.
    traced: [Option<DebugEntry>; 2],
}

impl std::fmt::Debug for DebugState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugState")
            .field("stats", &self.stats)
            .field("ring", &self.ring)
            .field("trace", &self.trace)
            .field(
                "installed",
                &self.installed.lock().map_or(0, |installed| installed.len()),
            )
            .field("seq", &self.seq)
            .finish()
    }
}

impl Drop for DebugState {
    /// The installed functions are the user's, and so is what they captured:
    /// each is dropped inside a catch.
    fn drop(&mut self) {
        let installed = self
            .installed
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        for (_, f) in std::mem::take(installed) {
            drop_contained(f);
        }
    }
}

impl DebugState {
    fn empty() -> Self {
        Self {
            stats: None,
            ring: None,
            trace: false,
            installed: Mutex::new(Vec::new()),
            owner: None,
            seq: 0,
            traced: [None, None],
        }
    }

    /// The state a spawn's options start with; `None` when every option is
    /// off.
    #[inline]
    pub(crate) fn from_options(options: DebugOptions) -> Option<Self> {
        if options.any() {
            Some(Self::started_with(options))
        } else {
            None
        }
    }

    /// The state of a spawn that turned an option on, kept out of line: a
    /// spawn with every option off tests them and goes on.
    #[inline(never)]
    fn started_with(options: DebugOptions) -> Self {
        let mut state = Self::empty();
        state.stats = options.statistics.then(Stats::new);
        state.ring = options.log.map(|capacity| Ring::new(capacity.get()));
        state.trace = options.trace;
        state
    }

    /// Whether nothing is on, so the state need not be kept.
    fn is_idle(&mut self) -> bool {
        self.stats.is_none()
            && self.ring.is_none()
            && !self.trace
            && self
                .installed
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
    }

    /// The log's entries, oldest first.
    pub(crate) fn ring_entries(&self) -> Option<&[DebugEntry]> {
        self.ring.as_ref().map(Ring::visible)
    }

    /// The statistics as a caller reads them.
    fn statistics(&self) -> Option<Statistics> {
        self.stats.as_ref().map(|stats| Statistics {
            elapsed: stats.since.elapsed(),
            messages_in: stats.messages_in,
            messages_out: stats.messages_out,
            turns: stats.turns,
        })
    }

    /// Numbers `event` as the next entry.
    fn entry(&mut self, event: DebugEvent) -> DebugEntry {
        self.seq += 1;
        DebugEntry {
            seq: self.seq,
            event,
        }
    }

    /// Shows `entry` to each installed function, in the order installed. A
    /// function that returns `Remove` or panics is dropped, and the others
    /// still see the entry. A panic is logged with the `tracing` feature,
    /// naming the actor and the function's id.
    fn show_installed(&mut self, entry: &DebugEntry) {
        let owner = &self.owner;
        let installed = self
            .installed
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        let mut at = 0;
        while at < installed.len() {
            let verdict = catch_sync(|| (installed[at].1)(entry));
            match verdict {
                Ok(Install::Keep) => at += 1,
                Ok(Install::Remove) => {
                    let (_, f) = installed.remove(at);
                    drop_contained(f);
                }
                Err(payload) => {
                    let (id, f) = installed.remove(at);
                    log_panicked(owner.as_ref(), &id, payload);
                    drop_contained(f);
                }
            }
        }
    }

    /// A message was taken, just before its handler runs: counts it and the
    /// turn, logs it and shows it to the installed functions. A trace raises
    /// it once the turn has returned.
    #[inline(never)]
    pub(crate) fn message_in(&mut self, reply_expected: bool, priority: bool) {
        self.traced = [None, None];
        if let Some(stats) = &mut self.stats {
            stats.messages_in += 1;
            stats.turns += 1;
        }
        let entry = self.entry(DebugEvent::MessageIn {
            reply_expected,
            priority,
        });
        if let Some(ring) = &mut self.ring {
            ring.push(entry.clone());
        }
        self.show_installed(&entry);
        if self.trace {
            self.traced[0] = Some(entry);
        }
    }

    /// The reply of the turn was sent, `delivered` or not: counts it, logs it
    /// and shows it to the installed functions.
    #[inline(never)]
    pub(crate) fn message_out(&mut self, delivered: bool) {
        if let Some(stats) = &mut self.stats {
            stats.messages_out += 1;
        }
        let entry = self.entry(DebugEvent::MessageOut { delivered });
        if let Some(ring) = &mut self.ring {
            ring.push(entry.clone());
        }
        self.show_installed(&entry);
        if self.trace {
            self.traced[1] = Some(entry);
        }
    }

    /// The actor began to stop. Only the log keeps it: the path that stops
    /// the actor owes steps of its own, so no installed function and no
    /// handler runs on it.
    pub(crate) fn stopping(&mut self, reason: &StopReason) {
        let entry = self.entry(DebugEvent::Stopping(reason.clone()));
        if let Some(ring) = &mut self.ring {
            ring.push(entry);
        }
        self.traced = [None, None];
    }

    /// Whether the trace is on.
    #[inline]
    pub(crate) fn is_tracing(&self) -> bool {
        self.trace
    }

    /// The entries of the turn that returned, for the trace to raise; empty
    /// while the trace is off.
    #[inline(never)]
    pub(crate) fn take_traced(&mut self) -> [Option<DebugEntry>; 2] {
        std::mem::take(&mut self.traced)
    }
}

/// Runs `change` on the debug state, making it first when `make` is true and
/// there is none, and dropping it again when nothing is left on.
fn change(sys: &mut LoopSys<'_>, make: bool, edit: impl FnOnce(&mut DebugState)) {
    let slot = sys.debug_slot();
    if slot.is_none() {
        if !make {
            return;
        }
        *slot = Some(Box::new(DebugState::empty()));
    }
    if let Some(state) = slot.as_deref_mut() {
        edit(state);
        if state.is_idle() {
            *slot = None;
        }
    }
}

pub(crate) fn set_statistics(sys: &mut LoopSys<'_>, on: bool, reply: Reply<()>) {
    change(sys, on, |state| state.stats = on.then(Stats::new));
    let _ = send_contained(reply, ());
}

pub(crate) fn statistics(sys: &mut LoopSys<'_>, reply: Reply<Option<Statistics>>) {
    let stats = sys.debug_mut().and_then(|state| state.statistics());
    let _ = send_contained(reply, stats);
}

pub(crate) fn set_log(sys: &mut LoopSys<'_>, capacity: Option<usize>, reply: Reply<()>) {
    change(sys, capacity.is_some(), |state| {
        state.ring = capacity.map(|n| Ring::new(clamp_log_capacity(n)));
    });
    let _ = send_contained(reply, ());
}

pub(crate) fn log(sys: &mut LoopSys<'_>, reply: Reply<Option<Vec<DebugEntry>>>) {
    let entries = sys.debug_ring().map(<[DebugEntry]>::to_vec);
    let _ = send_contained(reply, entries);
}

pub(crate) fn set_trace(sys: &mut LoopSys<'_>, on: bool, reply: Reply<()>) {
    change(sys, on, |state| state.trace = on);
    let _ = send_contained(reply, ());
}

/// Logs, with the `tracing` feature, that the function installed under `id`
/// panicked and is removed; without it the payload is dropped inside a catch.
fn log_panicked(owner: Option<&Owner>, id: &str, payload: super::panic::PanicPayload) {
    #[cfg(feature = "tracing")]
    {
        let text = super::panic::payload_into_string(payload);
        if let Some(owner) = owner {
            caught_log!(warn,
                actor_id = %owner,
                installed = %id,
                panic = %owner.fields().panic_text(&text),
                "an installed sys function panicked; it is removed"
            );
        }
    }
    #[cfg(not(feature = "tracing"))]
    {
        let _ = (owner, id);
        discard(payload);
    }
}

pub(crate) fn install(
    sys: &mut LoopSys<'_>,
    owner: Owner,
    id: String,
    f: Box<dyn FnMut(&DebugEntry) -> Install + Send>,
    reply: Reply<()>,
) {
    change(sys, true, |state| {
        state.owner.get_or_insert(owner);
        let installed = state
            .installed
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        match installed.iter().position(|(known, _)| *known == id) {
            Some(at) => {
                let old = std::mem::replace(&mut installed[at].1, f);
                drop_contained(old);
            }
            None => installed.push((id, f)),
        }
    });
    let _ = send_contained(reply, ());
}

pub(crate) fn remove(sys: &mut LoopSys<'_>, id: String, reply: Reply<bool>) {
    let mut removed = false;
    change(sys, false, |state| {
        let installed = state
            .installed
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(at) = installed.iter().position(|(known, _)| *known == id) {
            let (_, f) = installed.remove(at);
            drop_contained(f);
            removed = true;
        }
    });
    let _ = send_contained(reply, removed);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::actor::sys::DebugOptions;

    /// An installed function that panics is removed, and with the `tracing`
    /// feature one line names the actor and the function's id.
    #[cfg(feature = "tracing")]
    #[test]
    fn a_panicking_installed_function_is_removed_and_logged() {
        let lines = crate::actor::supervision::lost_runner::capture::install();
        let who = format!("sys-debug-{}", uuid::Uuid::new_v4());
        let mut state = DebugState::from_options(DebugOptions::default().log(3)).unwrap();
        state.owner = Some(Owner::new(crate::types::ActorId::from(who.clone()), false));
        state
            .installed
            .get_mut()
            .unwrap()
            .push(("boom".into(), Box::new(|_| panic!("the function failed"))));
        state.message_in(true, false);
        assert!(state.installed.get_mut().unwrap().is_empty());
        let token = format!("actor_id={who} ");
        let logged: Vec<String> = lines
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(&token))
            .cloned()
            .collect();
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert!(logged[0].contains("boom") && logged[0].contains("the function failed"));
    }

    /// The stop path owes steps of its own: the entry of the stop reaches the
    /// log and nothing else, so no installed function runs there and no
    /// trace entry is left to raise.
    #[test]
    fn the_stop_entry_reaches_the_log_alone() {
        let mut state =
            DebugState::from_options(DebugOptions::default().log(3).trace(true)).unwrap();
        let shown = Arc::new(AtomicUsize::new(0));
        let counter = shown.clone();
        state.installed.get_mut().unwrap().push((
            "count".into(),
            Box::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Install::Keep
            }),
        ));
        state.message_in(true, false);
        assert_eq!(shown.load(Ordering::SeqCst), 1);
        state.stopping(&StopReason::Graceful);
        assert_eq!(shown.load(Ordering::SeqCst), 1, "no function ran for it");
        assert!(state.take_traced().iter().all(Option::is_none));
        let last = state.ring_entries().unwrap().last().unwrap();
        assert_eq!(last.event, DebugEvent::Stopping(StopReason::Graceful));
    }
}
