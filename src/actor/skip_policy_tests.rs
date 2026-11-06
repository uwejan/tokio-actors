//! `MissPolicy::Skip` puts the next tick on the first grid point after now,
//! and from `u32::MAX` or more missed intervals, one interval from now.
//!
//! Every test runs on a paused clock, advanced by the missed time, so a
//! count of missed intervals that real time could never reach is exact.

use std::time::Duration;

use super::*;
use crate::actor::ActorEnvelope;
use crate::error::ActorResult;
use crate::types::StopLane;

struct Sink;

impl Actor for Sink {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A handle on a bare mailbox; `Skip` never sends through it.
fn bare_handle() -> (
    ActorHandle<Sink>,
    crate::actor::mailbox::MailRx<ActorEnvelope<Sink>>,
) {
    let (tx, rx) = crate::actor::mailbox::test_channel(1);
    let (_status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, _) = StopLane::new();
    let handle = ActorHandle::new(
        ActorId::from("skip-policy"),
        tx,
        stop_lane,
        1,
        status_rx,
        "skip-policy-tests".into(),
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    (handle, rx)
}

/// Lets `missed` pass after a tick due now, then adjusts under `Skip`.
/// Returns the new deadline, the tick's original deadline and the time of
/// the adjustment.
async fn skip_after(interval: Duration, missed: Duration) -> (Instant, Instant, Instant) {
    let (handle, _rx) = bare_handle();
    let mut factory: Box<MessageFactory<Sink>> = Box::new(|| ());
    let due = Instant::now();
    let mut next = due;
    time::advance(missed).await;
    let now = Instant::now();
    let adjusted = adjust_next(
        &mut next,
        interval,
        MissPolicy::Skip,
        &CancellationToken::new(),
        &handle,
        &mut factory,
    );
    assert!(matches!(adjusted, Adjusted::Next), "Skip sends nothing");
    (next, due, now)
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).expect("a duration of this test fits in u64 nanoseconds")
}

/// One interval short of `u32::MAX` missed intervals still keeps the grid.
#[tokio::test(start_paused = true)]
async fn skip_keeps_the_grid_up_to_one_interval_short_of_u32_max_missed_intervals() {
    let interval = Duration::from_millis(2);
    let missed_intervals = u64::from(u32::MAX) - 1;
    let missed = Duration::from_nanos(nanos(interval) * missed_intervals + 1_000_000);
    let (next, _due, now) = skip_after(interval, missed).await;
    assert_eq!(next, now + Duration::from_millis(1), "the next grid point");
}

/// From `u32::MAX` missed intervals on, the grid is dropped and the next tick
/// is one interval from now.
#[tokio::test(start_paused = true)]
async fn skip_waits_one_interval_from_now_after_u32_max_or_more_missed_intervals() {
    let interval = Duration::from_millis(2);
    for missed_intervals in [
        u64::from(u32::MAX),
        u64::from(u32::MAX) + 1,
        u64::from(u32::MAX) + 1_000,
    ] {
        let missed = Duration::from_nanos(nanos(interval) * missed_intervals + 1_000_000);
        let (next, _due, now) = skip_after(interval, missed).await;
        assert_eq!(next, now + interval, "{missed_intervals} missed intervals");
    }
}
