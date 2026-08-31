//! The stop lane: a `watch`-based signal used to deliver stop/kill requests.
//!
//! Every actor owns one lane. Raising it is synchronous and infallible - it
//! never awaits mailbox capacity, unlike a channel send - and the run loop
//! observes it with the highest priority of anything in its `select!`, ahead
//! of the system channel and the user mailbox. It is the single transport for
//! every stop or kill signal: spawning-side, supervision-side, and
//! system-shutdown-side alike.

use tokio::sync::watch;

use crate::types::StopReason;

/// Escalation tier of a stop request, derived from its [`StopReason`]. Used
/// only to keep the lane monotone: comparing two requests' tiers is how
/// `raise` decides whether an incoming request may overwrite the pending one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum StopSeverity {
    /// Vetoable via `pre_stop`: [`StopReason::Graceful`], [`StopReason::ParentRequest`].
    Requested,
    /// Non-vetoable, but `on_stopped` still runs: [`StopReason::Failure`], [`StopReason::Cancelled`].
    Forced,
    /// Bypasses every lifecycle hook: [`StopReason::Kill`].
    Kill,
}

impl StopSeverity {
    fn of(reason: &StopReason) -> Self {
        match reason {
            StopReason::Graceful | StopReason::ParentRequest => StopSeverity::Requested,
            StopReason::Failure(_) | StopReason::Cancelled => StopSeverity::Forced,
            StopReason::Kill => StopSeverity::Kill,
        }
    }
}

/// Value carried by the stop lane: the highest-severity request raised so
/// far, and a generation counter bumped on every accepted raise - including a
/// re-raise at the same severity as the one already pending, which changes
/// nothing about the tier but still guarantees the run loop re-observes it
/// (for example, re-firing `pre_stop` after an earlier veto).
#[derive(Debug, Clone, Default)]
pub(crate) struct LaneState {
    pub(crate) reason: Option<StopReason>,
    pub(crate) generation: u64,
}

/// Per-actor stop/kill signal.
///
/// `raise` never blocks and never fails: it is a plain `watch` write, not a
/// channel send, so there is no mailbox capacity to await and no `Result` to
/// hand back to the caller. Severity only ever increases - a `Kill` already
/// raised can never be displaced by a later `Graceful`/`ParentRequest` call -
/// while a request at the same severity as the one already pending still
/// lands and bumps the generation, so it is never silently swallowed.
/// Multiple raises that land between two run-loop observations coalesce into
/// the single latest state: the loop sees at least one observation per
/// distinct severity level reached, never one per individual `raise` call.
#[derive(Debug, Clone)]
pub(crate) struct StopLane {
    tx: watch::Sender<LaneState>,
}

impl StopLane {
    /// Creates a fresh, unraised lane and the run loop's receiving half.
    pub(crate) fn new() -> (StopLane, watch::Receiver<LaneState>) {
        let (tx, rx) = watch::channel(LaneState::default());
        (StopLane { tx }, rx)
    }

    /// Raises a stop request at `reason`'s severity.
    ///
    /// A no-op if the actor has already finished its message loop (no
    /// receiver left to observe it) or if a strictly higher severity is
    /// already pending - severity never decreases. Otherwise the request is
    /// recorded and the generation counter is bumped, even when the specific
    /// reason is unchanged from the one already pending.
    pub(crate) fn raise(&self, reason: StopReason) {
        let new_severity = StopSeverity::of(&reason);
        self.tx.send_if_modified(move |state| {
            let accept = match state.reason.as_ref() {
                Some(current) => new_severity >= StopSeverity::of(current),
                None => true,
            };
            if accept {
                state.reason = Some(reason);
                state.generation = state.generation.wrapping_add(1);
            }
            accept
        });
    }

    /// True once the actor has finished its message loop and stopped
    /// observing this lane - the same closing point at which its mailbox and
    /// system channel close, so every "already stopped" probe agrees.
    pub(crate) fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ActorError;

    fn severity_of(state: &LaneState) -> Option<StopSeverity> {
        state.reason.as_ref().map(StopSeverity::of)
    }

    #[test]
    fn first_raise_is_always_accepted() {
        let (lane, rx) = StopLane::new();
        lane.raise(StopReason::Graceful);
        let state = rx.borrow();
        assert_eq!(severity_of(&state), Some(StopSeverity::Requested));
        assert_eq!(state.generation, 1);
    }

    #[test]
    fn higher_severity_overwrites_lower() {
        let (lane, rx) = StopLane::new();
        lane.raise(StopReason::Graceful);
        lane.raise(StopReason::Kill);
        let state = rx.borrow();
        assert_eq!(severity_of(&state), Some(StopSeverity::Kill));
        assert_eq!(state.generation, 2);
    }

    #[test]
    fn lower_severity_never_displaces_a_higher_one() {
        let (lane, rx) = StopLane::new();
        lane.raise(StopReason::Kill);
        lane.raise(StopReason::Graceful);
        lane.raise(StopReason::Failure(ActorError::user("x")));
        let state = rx.borrow();
        assert_eq!(
            severity_of(&state),
            Some(StopSeverity::Kill),
            "Kill must never be downgraded by a later, weaker raise"
        );
        assert_eq!(
            state.generation, 1,
            "a rejected (lower-severity) raise must not bump the generation"
        );
    }

    #[test]
    fn equal_severity_reraise_bumps_generation_without_changing_tier() {
        let (lane, rx) = StopLane::new();
        lane.raise(StopReason::Graceful);
        lane.raise(StopReason::ParentRequest);
        let state = rx.borrow();
        assert_eq!(severity_of(&state), Some(StopSeverity::Requested));
        assert_eq!(
            state.generation, 2,
            "same-tier re-raise must still bump the generation for re-observation"
        );
        assert!(matches!(state.reason, Some(StopReason::ParentRequest)));
    }

    #[tokio::test]
    async fn raise_after_receiver_dropped_is_a_harmless_no_op() {
        let (lane, rx) = StopLane::new();
        drop(rx);
        assert!(lane.is_closed());
        lane.raise(StopReason::Kill); // must not panic
    }
}
