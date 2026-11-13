//! Per-actor turn clock backing the stuck-turn watchdog.
//!
//! Each spawned actor gets one [`TurnClock`], created once at spawn time and
//! held by exactly two parties: the actor's own run loop, which writes it
//! (marking a `handle` invocation's start and end), and the system registry
//! entry for that actor, which reads it (answering
//! [`ActorSystem::turn_elapsed`](crate::system::ActorSystem::turn_elapsed) and
//! [`ActorSystem::stuck_actors`](crate::system::ActorSystem::stuck_actors)).
//! It is never reachable from an [`ActorHandle`](crate::actor::handle::ActorHandle),
//! so cloning a handle never touches it.
//!
//! The clock is gated by its owning system's mode word: the watchdog bit is
//! set while a turn warning threshold is in effect (see
//! [`ActorSystem::set_turn_warning_threshold`](crate::system::ActorSystem::set_turn_warning_threshold)).
//! While it is off, starting a turn costs one relaxed load and a branch, with
//! no clock read and no store, so an actor whose system never enables the
//! watchdog pays nothing beyond that check. The clock is purely
//! observational either way: nothing here ever reads back into the run loop,
//! raises a stop, or changes how a callback completes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::actor::panic::PanicPayload;
use crate::actor::subtree::AbortRecord;
use crate::telemetry::{
    push_about, EventKind, HookKind, Mode, TelemetryEvent, TurnOutcome, CTL_DEADLOCK,
    CTL_TRACK_TURNS, CTL_WATCHDOG,
};

/// Process-wide reference point every [`TurnClock`] timestamp is measured
/// from. A plain `std::time::Instant`, not tokio's, so a paused test clock
/// can never freeze it: the watchdog's notion of elapsed time always tracks
/// the real wall clock, even under a paused tokio time source.
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

#[cfg(test)]
thread_local! {
    /// The clock reads this thread has made through [`now_nanos`].
    static CLOCK_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many times this thread has read the clock through [`now_nanos`], for
/// tests that assert a path reads none.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn clock_reads_this_thread() -> u64 {
    CLOCK_READS.with(std::cell::Cell::get)
}

/// Nanoseconds elapsed since the process-wide epoch, saturating rather than
/// overflowing at the extremes.
pub(crate) fn now_nanos() -> u64 {
    #[cfg(test)]
    CLOCK_READS.with(|reads| reads.set(reads.get() + 1));
    let elapsed = Instant::now().saturating_duration_since(epoch());
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

/// The cells of one actor's clock: the identity the wait graph names the
/// actor by, the start of the turn in flight and the turns begun.
#[derive(Debug, Default)]
struct TurnCell {
    /// The actor's incarnation number, fixed at the spawn (see
    /// [`TurnClock::id`]); `0` for a clock no spawn made.
    id: u64,
    /// `0` means no turn is in flight; any other value is one plus the
    /// nanosecond offset, from the process-wide epoch, at which the current
    /// turn began.
    started: AtomicU64,
    /// The turns begun while turns are counted.
    turns: AtomicU64,
}

/// The control bits that make a turn write its start into the cell.
const STORES: u64 = CTL_WATCHDOG | CTL_TRACK_TURNS;

/// The flags of a [`TurnMode`], in its top four bits.
const FLAG_STORED: u64 = 1 << 63;
const FLAG_TRACKS: u64 = 1 << 62;
const FLAG_DEADLOCK: u64 = 1 << 61;
const FLAG_FINISHED: u64 = 1 << 60;
/// The clock offset of a [`TurnMode`], in its low sixty bits.
const OFFSET: u64 = (1 << 60) - 1;

/// What a turn's start read from the mode word, handed back to its end: the
/// flags the turn needs and the clock offset it read, in one word so a hook
/// that keeps it across a wait grows by eight bytes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TurnMode(u64);

#[allow(dead_code)]
impl TurnMode {
    /// Whether the turn's start found nothing that asks for the clock, the
    /// caller token or the turn's event: no flag is set and no clock offset
    /// was read, so the turn's end has nothing to undo or report.
    #[inline(always)]
    pub(crate) fn is_idle(self) -> bool {
        self.0 == 0
    }

    /// Whether turns are counted and flagged in flight.
    pub(crate) fn tracks(self) -> bool {
        self.0 & FLAG_TRACKS != 0
    }

    /// Whether sends that wait are recorded in the wait graph.
    pub(crate) fn deadlock(self) -> bool {
        self.0 & FLAG_DEADLOCK != 0
    }

    /// Whether some sink took events of `kind` when the turn began. A turn
    /// holds the answer for `TurnFinished` only, the one kind it raises; any
    /// other kind reads as not armed.
    pub(crate) fn armed(self, kind: EventKind) -> bool {
        matches!(kind, EventKind::TurnFinished) && self.0 & FLAG_FINISHED != 0
    }

    /// One plus the clock offset `begin` read, or `0` when it read none.
    fn t0(self) -> u64 {
        self.0 & OFFSET
    }
}

/// Per-actor record of the currently in-flight `handle` invocation, if any,
/// gated by the owning system's mode word.
///
/// Created once per spawned actor and given to exactly two owners: the run
/// loop's own copy (the writer, via [`begin`](Self::begin) and
/// [`end`](Self::end)) and the system registry entry's copy (the reader, via
/// [`token`](Self::token) and [`elapsed`](Self::elapsed)).
///
/// Holds two shared cells: the actor's own turn cell (`started` is `0` when
/// no turn is in flight, otherwise one plus the nanosecond offset, from the
/// process-wide epoch, at which the current turn began), and the owning
/// system's mode word, whose watchdog bit says whether turns are timed.
/// Cloning a `TurnClock` shares both, so the run loop's copy and the
/// registry's copy always agree on the current turn state and on whether the
/// watchdog is currently on.
///
/// Reading the mode word on every [`begin`](Self::begin) means a turn already
/// in flight when the watchdog is first switched on was never timed and
/// stays untimed for its own duration; the next turn is timed normally.
/// Switching the watchdog off mid-turn does not un-time that turn early -
/// [`elapsed`](Self::elapsed) still reports it until
/// [`end`](Self::end) clears it - but no further turn is timed until it is
/// switched on again.
#[derive(Debug, Clone)]
pub(crate) struct TurnClock {
    cell: Arc<TurnCell>,
    mode: Arc<Mode>,
}

impl Default for TurnClock {
    /// A clock on a disarmed mode: no turn is ever timed.
    fn default() -> Self {
        Self::new(Mode::new())
    }
}

impl TurnClock {
    /// Creates a clock on `mode`: typically the owning
    /// [`ActorSystem`](crate::system::ActorSystem)'s own mode, so a switch
    /// made there takes effect for this clock immediately, with no separate
    /// signaling.
    pub(crate) fn new(mode: Arc<Mode>) -> Self {
        Self::with_id(mode, 0)
    }

    /// A clock on `mode` whose id is `id`: the incarnation number of the actor
    /// it serves, which is also its wait token.
    pub(crate) fn with_id(mode: Arc<Mode>, id: u64) -> Self {
        Self {
            cell: Arc::new(TurnCell {
                id,
                ..TurnCell::default()
            }),
            mode,
        }
    }

    /// A clock whose watchdog is on, for tests that exercise actual timing.
    #[cfg(test)]
    pub(crate) fn timed() -> Self {
        let mode = Mode::new();
        mode.set_ctl(CTL_WATCHDOG, true);
        Self::new(mode)
    }

    /// The wait token of the incarnation this clock serves: its incarnation
    /// number, drawn once from the process-wide counter, so it is nonzero, the
    /// same for every clone and never given to another incarnation, in this
    /// system or any other; the wait graph never reads a successor as its
    /// predecessor. `0` for a clock no spawn made.
    pub(crate) fn id(&self) -> u64 {
        self.cell.id
    }

    /// Starts a turn. One relaxed load of the mode word decides everything:
    /// the clock is read only when the watchdog, turn tracking or a sink of
    /// `TurnFinished` asks for it, and the turn's start and the turn count are
    /// stored only under the watchdog or turn tracking. With none of them on,
    /// that load and a branch are the whole cost. Returns what the word said,
    /// for the turn's end; a caller may ignore it.
    #[inline]
    pub(crate) fn begin(&self) -> TurnMode {
        let word = self.mode.word();
        if word & (STORES | CTL_DEADLOCK | EventKind::TurnFinished.bit()) == 0 {
            return TurnMode(0);
        }
        self.begin_flagged(word)
    }

    /// The part of [`begin`](Self::begin) that runs only when something asks
    /// for the clock or the caller token.
    #[inline(never)]
    fn begin_flagged(&self, word: u64) -> TurnMode {
        let mut flags = 0;
        if word & STORES != 0 {
            flags |= FLAG_STORED;
        }
        if word & CTL_TRACK_TURNS != 0 {
            flags |= FLAG_TRACKS;
        }
        if word & CTL_DEADLOCK != 0 {
            flags |= FLAG_DEADLOCK;
        }
        let finished = word & EventKind::TurnFinished.bit() != 0;
        if finished {
            flags |= FLAG_FINISHED;
        }
        if word & STORES == 0 && !finished {
            return TurnMode(flags);
        }
        let t0 = now_nanos().saturating_add(1).min(OFFSET);
        if word & STORES != 0 {
            if word & CTL_TRACK_TURNS != 0 {
                // Single writer: the task that runs the turns.
                let begun = self.cell.turns.load(Ordering::Relaxed);
                self.cell
                    .turns
                    .store(begun.wrapping_add(1), Ordering::Relaxed);
            }
            self.cell.started.store(t0, Ordering::Relaxed);
        }
        TurnMode(flags | t0)
    }

    /// Ends the turn `mode` began: clears the start `begin` stored, if it
    /// stored one, and returns how long the turn took when `begin` read the
    /// clock and a sink took `TurnFinished` at that moment. One clock read
    /// at most.
    #[inline]
    pub(crate) fn end_timed(&self, mode: TurnMode) -> Option<Duration> {
        if mode.0 & FLAG_STORED != 0 {
            self.cell.started.store(0, Ordering::Relaxed);
        }
        if mode.t0() == 0 || mode.0 & FLAG_FINISHED == 0 {
            return None;
        }
        Some(Duration::from_nanos(
            now_nanos().saturating_sub(mode.t0() - 1),
        ))
    }

    /// The caller token a hook polls under: the identity of this clock when
    /// `mode` says sends that wait are recorded, otherwise `0`.
    #[inline]
    pub(crate) fn caller_token(&self, mode: TurnMode) -> u64 {
        if mode.deadlock() {
            self.id()
        } else {
            0
        }
    }

    /// The turns begun while turns were counted.
    #[allow(dead_code)]
    pub(crate) fn turns(&self) -> u64 {
        self.cell.turns.load(Ordering::Relaxed)
    }

    /// Clears the clock: no turn is in flight. A no-op (one relaxed load, no
    /// store) if no turn was ever recorded, which is always the case while
    /// the watchdog is off.
    pub(crate) fn end(&self) {
        if self.cell.started.load(Ordering::Relaxed) != 0 {
            self.cell.started.store(0, Ordering::Relaxed);
        }
    }

    /// Returns the raw token: `0` when no turn is in flight (including
    /// whenever the watchdog is off), otherwise one plus the turn's start
    /// offset in nanoseconds since the epoch.
    pub(crate) fn token(&self) -> u64 {
        self.cell.started.load(Ordering::Relaxed)
    }

    /// Returns how long the current turn has been running, or `None` if no
    /// turn is in flight (including whenever the watchdog is off).
    pub(crate) fn elapsed(&self) -> Option<Duration> {
        let token = self.token();
        if token == 0 {
            return None;
        }
        let started_nanos = token - 1;
        Some(Duration::from_nanos(
            now_nanos().saturating_sub(started_nanos),
        ))
    }

    /// Returns the current token together with the elapsed time it implies,
    /// both derived from a single atomic load - unlike calling
    /// [`token`](Self::token) and [`elapsed`](Self::elapsed) separately,
    /// which reads the underlying cell twice and can therefore observe two
    /// different turns if the actor ends one and starts the next between
    /// the two loads. `None` if no turn is in flight (including whenever the
    /// watchdog is off).
    pub(crate) fn snapshot(&self) -> Option<(u64, Duration)> {
        let token = self.cell.started.load(Ordering::Relaxed);
        if token == 0 {
            return None;
        }
        let started_nanos = token - 1;
        let elapsed = Duration::from_nanos(now_nanos().saturating_sub(started_nanos));
        Some((token, elapsed))
    }
}

/// Holds an actor's turn clock for the run loop and clears the turn in flight
/// when the task ends on any path, an abort or the end of the runtime that
/// drops a hook's future mid-turn included, so no reader sees a turn that no
/// task runs.
#[derive(Debug)]
pub(crate) struct TurnGuard(TurnClock);

impl TurnGuard {
    /// A guard over `clock`.
    pub(crate) fn new(clock: TurnClock) -> Self {
        Self(clock)
    }
}

impl std::ops::Deref for TurnGuard {
    type Target = TurnClock;

    fn deref(&self) -> &TurnClock {
        &self.0
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// How a hook that returns a `Result` ended.
pub(crate) fn outcome_of<T, E>(result: &Result<Result<T, E>, PanicPayload>) -> TurnOutcome {
    match result {
        Ok(Ok(_)) => TurnOutcome::Returned,
        Ok(Err(_)) => TurnOutcome::Failed,
        Err(_) => TurnOutcome::Panicked,
    }
}

/// How a hook that returns no `Result` ended.
pub(crate) fn outcome_plain<T>(result: &Result<T, PanicPayload>) -> TurnOutcome {
    match result {
        Ok(_) => TurnOutcome::Returned,
        Err(_) => TurnOutcome::Panicked,
    }
}

/// Queues the `TurnFinished` event of a turn that began as `mode` says, when a
/// sink took the kind then. For every hook but a `handle` turn that went on,
/// each of which still owes the actor a step.
#[inline]
pub(crate) fn push_turn(
    record: &AbortRecord,
    mode: TurnMode,
    took: Option<Duration>,
    hook: HookKind,
    outcome: TurnOutcome,
) {
    if !mode.armed(EventKind::TurnFinished) {
        return;
    }
    push_about(record, None, EventKind::TurnFinished, |actor| {
        TelemetryEvent::TurnFinished {
            actor: std::borrow::Cow::Owned(actor.clone()),
            hook,
            took: took.unwrap_or_default(),
            outcome,
        }
    });
}

/// Queues the `RunFinished` event of the run still open when the loop exits,
/// which the stop sequence follows.
pub(crate) fn push_run(record: &AbortRecord, takes: u32, started: std::time::Instant) {
    push_about(record, None, EventKind::RunFinished, |actor| {
        TelemetryEvent::RunFinished {
            actor: std::borrow::Cow::Owned(actor.clone()),
            takes,
            took: started.elapsed(),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock whose watchdog is on, for tests that exercise actual timing.
    fn timed_clock() -> TurnClock {
        TurnClock::timed()
    }
}
