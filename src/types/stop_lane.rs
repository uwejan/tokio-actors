//! The stop lane: a `watch`-based signal used to deliver stop/kill requests.
//!
//! Every actor owns one lane. Raising it is synchronous and infallible - it
//! never awaits mailbox capacity, unlike a channel send - and the run loop
//! observes it with the highest priority of anything in its `select!`, ahead
//! of the system channel and the user mailbox. It is the single transport for
//! every stop or kill signal: spawning-side, supervision-side, and
//! system-shutdown-side alike.

use tokio::sync::watch;

use crate::actor::subtree::AbortRecord;
use crate::actor::supervision::rules::lane_accept;
use crate::types::StopReason;

/// Escalation tier of a stop request, derived from its [`StopReason`]. Used
/// only to keep the lane monotone: comparing two requests' tiers is how
/// `raise` decides whether an incoming request may overwrite the pending one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StopSeverity {
    /// Vetoable via `pre_stop`: [`StopReason::Graceful`], [`StopReason::ParentRequest`].
    Requested,
    /// Non-vetoable, but `on_stopped` still runs: [`StopReason::Failure`],
    /// [`StopReason::ParentKilled`], [`StopReason::Cancelled`].
    Forced,
    /// Bypasses every lifecycle hook: [`StopReason::Kill`].
    Kill,
}

impl StopSeverity {
    pub(crate) fn of(reason: &StopReason) -> Self {
        match reason {
            StopReason::Graceful | StopReason::ParentRequest => StopSeverity::Requested,
            StopReason::Failure(_) | StopReason::ParentKilled | StopReason::Cancelled => {
                StopSeverity::Forced
            }
            StopReason::Kill => StopSeverity::Kill,
        }
    }

    /// The order of two requests of the same severity: a parent's request
    /// outranks a plain graceful one, and a failure outranks an inherited
    /// stop, which outranks a cancellation.
    pub(crate) fn rank(reason: &StopReason) -> u8 {
        match reason {
            StopReason::Graceful | StopReason::Cancelled | StopReason::Kill => 0,
            StopReason::ParentRequest | StopReason::ParentKilled => 1,
            StopReason::Failure(_) => 2,
        }
    }
}

/// Value carried by the stop lane: the highest-severity request raised so
/// far, and a generation counter bumped on every accepted raise - including a
/// re-raise at the same severity as the one already pending, which changes
/// nothing about the tier but still guarantees the run loop re-observes it
/// (for example, re-firing `pre_stop` after an earlier veto).
///
/// `kill_only` is set at the closing point of the message loop: from there
/// the lane accepts a `Kill` and nothing else. `sealed` is set at the end of
/// the actor's teardown, just before its queued messages and its value are
/// dropped: from there it accepts nothing.
#[derive(Debug, Clone, Default)]
pub(crate) struct LaneState {
    pub(crate) reason: Option<StopReason>,
    pub(crate) generation: u64,
    pub(crate) kill_only: bool,
    pub(crate) sealed: bool,
}

/// How the lane answered a raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RaiseResult {
    /// The request was accepted: the generation moved, and the stored reason
    /// is the new one unless an equal-severity request of higher rank is
    /// pending.
    Recorded,
    /// A strictly higher severity is pending: nothing was written.
    Superseded,
    /// The lane takes no such request any more (sealed, open to `Kill` only,
    /// or its actor's task is gone): nothing was written.
    Refused,
}

/// Per-actor stop/kill signal.
///
/// `raise` never blocks and never fails: it is a plain `watch` write, not a
/// channel send, so there is no mailbox capacity to await and no `Result` to
/// hand back to the caller. [`try_raise`](Self::try_raise) is the same write
/// that also says how the lane answered. Severity only ever increases - a
/// `Kill` already raised can never be displaced by a later
/// `Graceful`/`ParentRequest` call - while a request at the same severity as
/// the one already pending still lands and bumps the generation, so it is
/// never silently swallowed; it replaces the stored reason only when it ranks
/// at least as high. Multiple raises that land between two run-loop
/// observations coalesce into the single latest state: the loop sees at
/// least one observation per distinct severity level reached, never one per
/// individual `raise` call.
#[derive(Debug, Clone)]
pub(crate) struct StopLane {
    tx: watch::Sender<LaneState>,
    /// The first abort issued against the incarnation's task: beside the
    /// lane's state, never in it, so recording it adds no lane receiver.
    aborted: AbortRecord,
}

impl StopLane {
    /// Creates a fresh, unraised lane and the run loop's receiving half.
    pub(crate) fn new() -> (StopLane, watch::Receiver<LaneState>) {
        let (tx, rx) = watch::channel(LaneState::default());
        (
            StopLane {
                tx,
                aborted: AbortRecord::new(),
            },
            rx,
        )
    }

    /// The incarnation's abort record.
    pub(crate) fn abort_record(&self) -> &AbortRecord {
        &self.aborted
    }

    /// Raises a stop request at `reason`'s severity and reports how the lane
    /// answered. The decision and the write are one step.
    ///
    /// Refused once the lane is sealed, when it is open to `Kill` only and
    /// `reason` is not `Kill`, and when the actor's task has dropped the
    /// receiving half (an aborted task never reaches the tail that seals the
    /// lane). Superseded when a strictly higher severity is pending.
    /// Otherwise recorded, and the generation counter is bumped even when the
    /// specific reason is unchanged from the one already pending.
    pub(crate) fn try_raise(&self, reason: StopReason) -> RaiseResult {
        let mut result = RaiseResult::Refused;
        self.tx.send_if_modified(|state| {
            let (answer, store, bump) = lane_accept(state, &reason, self.tx.is_closed());
            result = answer;
            if bump {
                state.generation = state.generation.wrapping_add(1);
                if store {
                    state.reason = Some(reason);
                }
            }
            bump
        });
        result
    }

    /// Raises a stop request exactly like [`try_raise`](Self::try_raise),
    /// for the callers that have no one to tell: a refused request is
    /// dropped silently.
    pub(crate) fn raise(&self, reason: StopReason) {
        let _ = self.try_raise(reason);
    }

    /// The request currently pending on the lane, if any: the
    /// highest-severity reason raised so far.
    pub(crate) fn reason(&self) -> Option<StopReason> {
        self.tx.borrow().reason.clone()
    }

    /// Opens the lane to `Kill` only, without waking the actor. Called at
    /// the closing point of the message loop; the receiver stays alive.
    pub(crate) fn close_to_non_kill(&self) {
        self.tx.send_if_modified(|state| {
            state.kill_only = true;
            false
        });
    }

    /// Seals the lane and returns the request pending on it, in one step.
    /// From here every raise is refused. Idempotent, and it works when the
    /// receiving half is gone.
    pub(crate) fn seal(&self) -> Option<StopReason> {
        let mut pending = None;
        self.tx.send_if_modified(|state| {
            state.sealed = true;
            pending = state.reason.clone();
            false
        });
        pending
    }

    /// True once the lane takes no request but `Kill` any more: the actor
    /// has finished its message loop, or its task is gone. The callers that
    /// must answer a caller read the verdict of `try_raise` instead, which
    /// decides and writes in one step.
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        let state = self.tx.borrow();
        state.sealed || state.kill_only || self.tx.is_closed()
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

    // - Exhaustive raise table ------------------------------------------------

    /// One representative [`StopReason`] per severity, used to build the
    /// literal raise table below without reaching for `StopSeverity` itself.
    #[derive(Debug, Clone, Copy)]
    enum RaiseChoice {
        Requested,
        Forced,
        Kill,
    }

    impl RaiseChoice {
        fn reason(self) -> StopReason {
            match self {
                RaiseChoice::Requested => StopReason::Graceful,
                RaiseChoice::Forced => StopReason::Cancelled,
                RaiseChoice::Kill => StopReason::Kill,
            }
        }

        /// True if `stored` holds exactly the reason this choice raises.
        fn matches_stored(self, stored: &Option<StopReason>) -> bool {
            matches!(
                (self, stored),
                (RaiseChoice::Requested, Some(StopReason::Graceful))
                    | (RaiseChoice::Forced, Some(StopReason::Cancelled))
                    | (RaiseChoice::Kill, Some(StopReason::Kill))
            )
        }
    }

    // Twelve rows: every combination of a prior lane state (none, or one of
    // the three severities already raised) and a newly raised severity. This
    // is an independent statement of the contract - not derived from
    // `StopSeverity`'s ordering or from `StopLane::raise`'s own comparison.
    const RAISE_TABLE: [(Option<RaiseChoice>, RaiseChoice, bool); 12] = [
        // no prior raise: everything is accepted
        (None, RaiseChoice::Requested, true),
        (None, RaiseChoice::Forced, true),
        (None, RaiseChoice::Kill, true),
        // prior: Requested (Graceful/ParentRequest)
        (Some(RaiseChoice::Requested), RaiseChoice::Requested, true),
        (Some(RaiseChoice::Requested), RaiseChoice::Forced, true),
        (Some(RaiseChoice::Requested), RaiseChoice::Kill, true),
        // prior: Forced (Failure/Cancelled)
        (Some(RaiseChoice::Forced), RaiseChoice::Requested, false),
        (Some(RaiseChoice::Forced), RaiseChoice::Forced, true),
        (Some(RaiseChoice::Forced), RaiseChoice::Kill, true),
        // prior: Kill
        (Some(RaiseChoice::Kill), RaiseChoice::Requested, false),
        (Some(RaiseChoice::Kill), RaiseChoice::Forced, false),
        (Some(RaiseChoice::Kill), RaiseChoice::Kill, true),
    ];

    #[test]
    fn raise_table_is_monotone() {
        for &(prior, raised, accepted) in RAISE_TABLE.iter() {
            let (lane, rx) = StopLane::new();
            if let Some(prior) = prior {
                lane.raise(prior.reason());
            }
            let generation_before = rx.borrow().generation;

            lane.raise(raised.reason());

            let state = rx.borrow();
            if accepted {
                assert_eq!(
                    state.generation,
                    generation_before.wrapping_add(1),
                    "prior {:?} raised {:?} should be accepted",
                    prior,
                    raised
                );
                assert!(
                    raised.matches_stored(&state.reason),
                    "prior {:?} raised {:?}: stored reason should become the raised one, got {:?}",
                    prior,
                    raised,
                    state.reason
                );
            } else {
                let prior = prior.expect("a rejected raise always has a prior");
                assert_eq!(
                    state.generation, generation_before,
                    "prior {:?} raised {:?} should be rejected",
                    prior, raised
                );
                assert!(
                    prior.matches_stored(&state.reason),
                    "prior {:?} raised {:?}: stored reason should stay unchanged, got {:?}",
                    prior,
                    raised,
                    state.reason
                );
            }
        }
    }

    // - Rank, kill-only and sealed rows ---------------------------------------

    /// How a row's lane is prepared before the request under test.
    #[derive(Debug, Clone, Copy)]
    enum Mode {
        Open,
        KillOnly,
        Sealed,
    }

    fn failure(text: &str) -> StopReason {
        StopReason::Failure(ActorError::user(text))
    }

    /// Prepares a lane holding `stored` in `mode`, raises `new`, and returns
    /// the answer, the generation change and the stored reason.
    fn row(
        stored: Option<StopReason>,
        mode: Mode,
        new: StopReason,
    ) -> (RaiseResult, u64, Option<StopReason>) {
        let (lane, rx) = StopLane::new();
        if let Some(stored) = stored {
            lane.raise(stored);
        }
        match mode {
            Mode::Open => {}
            Mode::KillOnly => lane.close_to_non_kill(),
            Mode::Sealed => {
                lane.close_to_non_kill();
                lane.seal();
            }
        }
        let before = rx.borrow().generation;
        let answer = lane.try_raise(new);
        let state = rx.borrow();
        (
            answer,
            state.generation.wrapping_sub(before),
            state.reason.clone(),
        )
    }

    #[test]
    fn rank_inside_a_severity_keeps_the_higher_request_and_always_bumps() {
        // A parent's request above a graceful one, in both orders.
        let (answer, bump, kept) = row(
            Some(StopReason::ParentRequest),
            Mode::Open,
            StopReason::Graceful,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::ParentRequest)), "{kept:?}");
        let (answer, bump, kept) = row(
            Some(StopReason::Graceful),
            Mode::Open,
            StopReason::ParentRequest,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::ParentRequest)), "{kept:?}");
        // A failure above a cancellation, in both orders.
        let (answer, bump, kept) = row(Some(failure("f")), Mode::Open, StopReason::Cancelled);
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Failure(_))), "{kept:?}");
        let (answer, bump, kept) = row(Some(StopReason::Cancelled), Mode::Open, failure("f"));
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Failure(_))), "{kept:?}");
        // Between two failures the newer is stored.
        let (_, bump, kept) = row(Some(failure("old")), Mode::Open, failure("new"));
        assert_eq!(bump, 1);
        assert!(
            matches!(&kept, Some(StopReason::Failure(err)) if err.to_string().contains("new")),
            "{kept:?}"
        );
        // A strictly lower severity is superseded without a bump.
        let (answer, bump, kept) = row(Some(failure("f")), Mode::Open, StopReason::ParentRequest);
        assert_eq!((answer, bump), (RaiseResult::Superseded, 0));
        assert!(matches!(kept, Some(StopReason::Failure(_))), "{kept:?}");
    }

    #[test]
    fn an_inherited_stop_ranks_between_a_failure_and_a_cancellation() {
        // A failure keeps its place over a later inherited stop, and an
        // inherited stop is replaced by a later failure: every accepted
        // raise bumps the generation.
        let (answer, bump, kept) = row(Some(failure("f")), Mode::Open, StopReason::ParentKilled);
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Failure(_))), "{kept:?}");
        let (answer, bump, kept) = row(Some(StopReason::ParentKilled), Mode::Open, failure("f"));
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Failure(_))), "{kept:?}");
        // An inherited stop keeps its place over a later cancellation, and
        // replaces an earlier one.
        let (answer, bump, kept) = row(
            Some(StopReason::ParentKilled),
            Mode::Open,
            StopReason::Cancelled,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::ParentKilled)), "{kept:?}");
        let (answer, bump, kept) = row(
            Some(StopReason::Cancelled),
            Mode::Open,
            StopReason::ParentKilled,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::ParentKilled)), "{kept:?}");
        // Two inherited stops: the newer is stored and the generation moves.
        let (answer, bump, kept) = row(
            Some(StopReason::ParentKilled),
            Mode::Open,
            StopReason::ParentKilled,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::ParentKilled)), "{kept:?}");
        // It replaces a vetoable request, and a vetoable request after it is
        // superseded without a bump.
        for requested in [StopReason::Graceful, StopReason::ParentRequest] {
            let (answer, bump, kept) = row(
                Some(requested.clone()),
                Mode::Open,
                StopReason::ParentKilled,
            );
            assert_eq!((answer, bump), (RaiseResult::Recorded, 1), "{requested:?}");
            assert!(matches!(kept, Some(StopReason::ParentKilled)), "{kept:?}");
            let (answer, bump, kept) = row(
                Some(StopReason::ParentKilled),
                Mode::Open,
                requested.clone(),
            );
            assert_eq!(
                (answer, bump),
                (RaiseResult::Superseded, 0),
                "{requested:?}"
            );
            assert!(matches!(kept, Some(StopReason::ParentKilled)), "{kept:?}");
        }
        // Kill-only and sealed lanes refuse it without a bump, and a Kill is
        // accepted over a stored one while kill-only.
        let (answer, bump, kept) = row(None, Mode::KillOnly, StopReason::ParentKilled);
        assert_eq!((answer, bump), (RaiseResult::Refused, 0));
        assert!(kept.is_none(), "{kept:?}");
        let (answer, bump, kept) = row(None, Mode::Sealed, StopReason::ParentKilled);
        assert_eq!((answer, bump), (RaiseResult::Refused, 0));
        assert!(kept.is_none(), "{kept:?}");
        let (answer, bump, kept) = row(
            Some(StopReason::ParentKilled),
            Mode::KillOnly,
            StopReason::Kill,
        );
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Kill)), "{kept:?}");
    }

    #[test]
    fn kill_only_lane_takes_a_kill_and_nothing_else() {
        for new in [
            StopReason::Graceful,
            StopReason::ParentRequest,
            StopReason::ParentKilled,
            StopReason::Cancelled,
            failure("f"),
        ] {
            let (answer, bump, kept) = row(None, Mode::KillOnly, new.clone());
            assert_eq!((answer, bump), (RaiseResult::Refused, 0), "{new:?}");
            assert!(kept.is_none(), "{new:?} stored {kept:?}");
        }
        let (answer, bump, kept) =
            row(Some(StopReason::Graceful), Mode::KillOnly, StopReason::Kill);
        assert_eq!((answer, bump), (RaiseResult::Recorded, 1));
        assert!(matches!(kept, Some(StopReason::Kill)), "{kept:?}");
        // A pending request does not change what the lane refuses.
        let (answer, bump, _) = row(
            Some(StopReason::Graceful),
            Mode::KillOnly,
            StopReason::ParentRequest,
        );
        assert_eq!((answer, bump), (RaiseResult::Refused, 0));
    }

    #[test]
    fn sealed_lane_refuses_everything() {
        for new in [
            StopReason::Kill,
            StopReason::Graceful,
            StopReason::ParentKilled,
            StopReason::Cancelled,
            failure("f"),
        ] {
            let (answer, bump, kept) = row(None, Mode::Sealed, new.clone());
            assert_eq!((answer, bump), (RaiseResult::Refused, 0), "{new:?}");
            assert!(kept.is_none(), "{new:?} stored {kept:?}");
        }
    }

    #[test]
    fn lane_without_a_receiver_refuses_everything() {
        for new in [
            StopReason::Kill,
            StopReason::Graceful,
            StopReason::ParentRequest,
            StopReason::ParentKilled,
            StopReason::Cancelled,
            failure("f"),
        ] {
            let (lane, rx) = StopLane::new();
            drop(rx);
            assert_eq!(lane.try_raise(new.clone()), RaiseResult::Refused, "{new:?}");
            assert!(lane.reason().is_none(), "{new:?} was stored");
        }
    }

    #[test]
    fn seal_is_idempotent_and_works_with_no_receiver() {
        let (lane, rx) = StopLane::new();
        lane.raise(StopReason::Graceful);
        assert!(matches!(lane.seal(), Some(StopReason::Graceful)));
        assert!(matches!(lane.seal(), Some(StopReason::Graceful)));
        drop(rx);
        assert!(matches!(lane.seal(), Some(StopReason::Graceful)));
        let (lane, rx) = StopLane::new();
        drop(rx);
        assert!(lane.seal().is_none());
        assert!(lane.is_closed());
    }

    #[test]
    fn try_raise_decides_and_writes_in_one_step() {
        // Whenever a raise of `Kill` is recorded, the seal that races it reads
        // that `Kill`: the decision and the write cannot be separated by it.
        for _ in 0..2_000 {
            let (lane, _rx) = StopLane::new();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let raiser = {
                let (lane, barrier) = (lane.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    lane.try_raise(StopReason::Kill)
                })
            };
            barrier.wait();
            let pending = lane.seal();
            let answer = raiser.join().unwrap();
            match answer {
                RaiseResult::Recorded => assert!(
                    matches!(pending, Some(StopReason::Kill)),
                    "a recorded Kill must be read by the seal, got {pending:?}"
                ),
                RaiseResult::Refused => assert!(pending.is_none(), "{pending:?}"),
                RaiseResult::Superseded => panic!("nothing is pending to supersede a Kill"),
            }
        }
    }

    #[test]
    fn generation_wraps_instead_of_overflowing() {
        let (tx, rx) = watch::channel(LaneState {
            reason: None,
            generation: u64::MAX,
            ..Default::default()
        });
        let lane = StopLane {
            tx,
            aborted: AbortRecord::new(),
        };

        lane.raise(StopReason::Graceful);

        let state = rx.borrow();
        assert_eq!(state.generation, 0);
        assert!(matches!(state.reason, Some(StopReason::Graceful)));
    }
}
