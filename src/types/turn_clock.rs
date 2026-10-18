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
//! The clock is gated by its owning system's turn warning threshold (see
//! [`ActorSystem::set_turn_warning_threshold`](crate::system::ActorSystem::set_turn_warning_threshold)):
//! while that threshold is off, starting a turn costs one relaxed load and a
//! branch, with no clock read and no store, so an actor whose system never
//! enables the watchdog pays nothing beyond that check. The clock is purely
//! observational either way: nothing here ever reads back into the run loop,
//! raises a stop, or changes how a callback completes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Process-wide reference point every [`TurnClock`] timestamp is measured
/// from. A plain `std::time::Instant`, not tokio's, so a paused test clock
/// can never freeze it: the watchdog's notion of elapsed time always tracks
/// the real wall clock, even under a paused tokio time source.
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

/// Nanoseconds elapsed since the process-wide epoch, saturating rather than
/// overflowing at the extremes.
fn nanos_since_epoch() -> u64 {
    let elapsed = Instant::now().saturating_duration_since(epoch());
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

/// Per-actor record of the currently in-flight `handle` invocation, if any,
/// gated by the owning system's turn warning threshold.
///
/// Created once per spawned actor and given to exactly two owners: the run
/// loop's own copy (the writer, via [`begin`](Self::begin) and
/// [`end`](Self::end)) and the system registry entry's copy (the reader, via
/// [`token`](Self::token) and [`elapsed`](Self::elapsed)).
///
/// Holds two shared cells: `started`, this actor's own turn state (`0` means
/// no turn is in flight; any other value is one plus the nanosecond offset,
/// from the process-wide epoch, at which the current turn began), and
/// `threshold_nanos`, a clone of the owning system's threshold cell (`0`
/// means the watchdog is off for that system). Cloning a `TurnClock` shares
/// both cells, so the run loop's copy and the registry's copy always agree
/// on the current turn state and on whether the watchdog is currently on.
///
/// Reading `threshold_nanos` on every [`begin`](Self::begin) means a turn
/// already in flight when a threshold is first set was never timed and
/// stays untimed for its own duration; the next turn is timed normally.
/// Switching the threshold off mid-turn does not un-time that turn early -
/// [`elapsed`](Self::elapsed) still reports it until
/// [`end`](Self::end) clears it - but no further turn is timed until a
/// threshold is set again.
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnClock {
    started: Arc<AtomicU64>,
    threshold_nanos: Arc<AtomicU64>,
}

impl TurnClock {
    /// Creates a clock tied to `threshold_nanos`: typically a clone of the
    /// owning [`ActorSystem`](crate::system::ActorSystem)'s own threshold
    /// cell, so a threshold change takes effect for this clock immediately,
    /// with no separate signaling.
    pub(crate) fn new(threshold_nanos: Arc<AtomicU64>) -> Self {
        Self {
            started: Arc::new(AtomicU64::new(0)),
            threshold_nanos,
        }
    }

    /// Records the current instant as the start of a new turn - but only if
    /// the watchdog is currently on for this clock's system. A no-op
    /// (one relaxed load, no clock read, no store) while it is off.
    pub(crate) fn begin(&self) {
        if self.threshold_nanos.load(Ordering::Relaxed) == 0 {
            return;
        }
        self.started
            .store(nanos_since_epoch().saturating_add(1), Ordering::Relaxed);
    }

    /// Clears the clock: no turn is in flight. A no-op (one relaxed load, no
    /// store) if no turn was ever recorded, which is always the case while
    /// the watchdog is off.
    pub(crate) fn end(&self) {
        if self.started.load(Ordering::Relaxed) != 0 {
            self.started.store(0, Ordering::Relaxed);
        }
    }

    /// Returns the raw token: `0` when no turn is in flight (including
    /// whenever the watchdog is off), otherwise one plus the turn's start
    /// offset in nanoseconds since the epoch.
    pub(crate) fn token(&self) -> u64 {
        self.started.load(Ordering::Relaxed)
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
            nanos_since_epoch().saturating_sub(started_nanos),
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
        let token = self.started.load(Ordering::Relaxed);
        if token == 0 {
            return None;
        }
        let started_nanos = token - 1;
        let elapsed = Duration::from_nanos(nanos_since_epoch().saturating_sub(started_nanos));
        Some((token, elapsed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock whose watchdog is on, for tests that exercise actual timing.
    fn timed_clock() -> TurnClock {
        TurnClock::new(Arc::new(AtomicU64::new(1)))
    }

    #[test]
    fn no_turn_in_flight_by_default() {
        let clock = TurnClock::default();
        assert_eq!(clock.token(), 0);
        assert!(clock.elapsed().is_none());
    }

    #[test]
    fn begin_is_a_no_op_while_the_threshold_is_off() {
        let clock = TurnClock::default(); // threshold cell is 0 (off)
        clock.begin();
        assert_eq!(
            clock.token(),
            0,
            "begin must not record a timestamp while the threshold is off"
        );
        assert!(clock.elapsed().is_none());
    }

    #[test]
    fn begin_records_once_the_threshold_is_set() {
        let clock = timed_clock();
        clock.begin();
        assert_ne!(clock.token(), 0);
        assert!(clock.elapsed().is_some());
    }

    #[test]
    fn end_clears_the_turn() {
        let clock = timed_clock();
        clock.begin();
        clock.end();
        assert_eq!(clock.token(), 0);
        assert!(clock.elapsed().is_none());
    }

    #[test]
    fn elapsed_grows_while_a_turn_is_in_flight() {
        let clock = timed_clock();
        clock.begin();
        let first = clock.elapsed().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let second = clock.elapsed().unwrap();
        assert!(second >= first);
    }

    #[test]
    fn clones_share_the_same_underlying_clock() {
        let clock = timed_clock();
        let cloned = clock.clone();
        clock.begin();
        assert_eq!(clock.token(), cloned.token());
    }

    #[test]
    fn snapshot_is_none_exactly_when_no_turn_is_in_flight() {
        let clock = timed_clock();
        assert!(clock.snapshot().is_none());
        clock.begin();
        assert!(clock.snapshot().is_some());
        clock.end();
        assert!(clock.snapshot().is_none());
    }

    #[test]
    fn snapshot_token_and_elapsed_come_from_one_load() {
        let clock = timed_clock();
        clock.begin();
        let expected_token = clock.token();

        let (token, elapsed) = clock.snapshot().unwrap();

        // Both halves of the pair must describe the same turn: the token
        // matches what `token()` reports for it, and the elapsed time
        // matches what `elapsed()` computes from that same token, up to the
        // handful of nanoseconds `snapshot`'s own separate clock read can
        // add relative to a fresh call to `elapsed()`.
        assert_eq!(token, expected_token);
        let elapsed_via_separate_calls = clock.elapsed().unwrap();
        let diff = elapsed_via_separate_calls
            .checked_sub(elapsed)
            .or_else(|| elapsed.checked_sub(elapsed_via_separate_calls))
            .unwrap();
        assert!(
            diff < Duration::from_millis(50),
            "snapshot's elapsed disagreed with a fresh elapsed() call by {diff:?}"
        );
    }
}
