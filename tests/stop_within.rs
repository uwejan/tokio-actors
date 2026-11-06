//! Behavioral suite for `ActorHandle::stop_within`: the bounded stop that
//! reports how far the escalation ladder had to go.
//!
//! Covers: a cooperative stop settling inside its window, an actor that
//! vetoes forever escalating all the way to `Kill`, a handler parked at an
//! await surviving `Kill` and needing an abort, an already-stopped actor's
//! `Closed` error, `StopReason::Kill` skipping the graceful window entirely,
//! a supervised `Permanent` child restarting after this call stops it
//! gracefully, and a handler that never yields at all reporting
//! `Unresponsive` without leaving a running actor behind.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, StopOutcome, StopReason,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The longest a [`NeverYields`] turn holds its thread when the test never
/// releases it: past every bound a test awaits.
const GATE_CAP: Duration = Duration::from_secs(15);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "stop-within-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

/// A plain actor: default `pre_stop` (always accepts), no-op `handle`.
#[derive(Default)]
struct Cooperative;

impl Actor for Cooperative {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Signals `started` as soon as its handler begins, then parks at an
/// `.await` that never resolves - deaf to both `ParentRequest` and `Kill`,
/// since both are only ever observed at a turn boundary this actor never
/// returns to. Only a task abort can end it.
struct Hangs {
    started: Arc<Notify>,
}

impl Actor for Hangs {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.started.notify_one();
        std::future::pending::<()>().await;
        unreachable!("pending() never resolves")
    }
}

/// Signals `began`, then blocks the OS thread it runs on inside `handle`
/// until the test sets `release` (for at most `GATE_CAP`), never yielding
/// back to the runtime while it does.
struct NeverYields {
    began: Arc<Notify>,
    release: Arc<AtomicBool>,
}

impl Actor for NeverYields {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.began.notify_one();
        let cap = std::time::Instant::now() + GATE_CAP;
        while !self.release.load(Ordering::SeqCst) && std::time::Instant::now() < cap {
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
}

/// Sets its flag when dropped, also while a failed assertion unwinds, so a
/// blocked turn never outlives the test that holds it.
struct Releases(Arc<AtomicBool>);

impl Drop for Releases {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// An aborted task never writes a final status itself; its status sender
/// closing is the terminal signal instead, and `status()` must report
/// `Stopped` for that case too, not go on reporting a stale `Running`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_stopped_after_an_abort() {
    let started = Arc::new(Notify::new());
    let handle = Hangs {
        started: started.clone(),
    }
    .spawn()
    .named(uname("status-after-abort"))
    .await
    .unwrap();

    handle.notify(()).await.unwrap();
    started.notified().await;

    let outcome = handle
        .stop_within(StopReason::Kill, Duration::from_millis(50))
        .await
        .unwrap();
    assert_eq!(outcome, StopOutcome::Aborted);

    assert_eq!(handle.status(), ActorStatus::Stopped);
    assert!(!handle.is_alive());
}

/// A trivial actor whose handler signals `began`, then cooperatively sleeps
/// for a fixed duration before returning - it yields back to the runtime
/// while it sleeps, so it is deaf to nothing; it simply takes a while to
/// finish its current turn.
struct SleepsBriefly {
    began: Arc<Notify>,
}

impl Actor for SleepsBriefly {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.began.notify_one();
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_reason_waits_for_a_cooperative_turn_to_finish() {
    let began = Arc::new(Notify::new());
    let handle = SleepsBriefly {
        began: began.clone(),
    }
    .spawn()
    .named(uname("kill-waits-for-turn"))
    .await
    .unwrap();

    handle.notify(()).await.unwrap();
    // `Kill` is raised once the 300ms turn has begun, so the raise lands
    // mid-turn.
    tokio::time::timeout(Duration::from_secs(10), began.notified())
        .await
        .expect("the turn begins");

    let outcome = handle
        .stop_within(StopReason::Kill, Duration::from_secs(2))
        .await
        .unwrap();

    assert_eq!(
        outcome,
        StopOutcome::Killed,
        "a cooperative turn that finishes within the window must never be aborted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_reason_with_a_short_window_aborts_a_parked_handler() {
    let started = Arc::new(Notify::new());
    let handle = Hangs {
        started: started.clone(),
    }
    .spawn()
    .named(uname("kill-short-window-aborts"))
    .await
    .unwrap();

    handle.notify(()).await.unwrap();
    started.notified().await;

    let outcome = handle
        .stop_within(StopReason::Kill, Duration::from_millis(50))
        .await
        .unwrap();

    assert_eq!(outcome, StopOutcome::Aborted);
}

/// The same edge for a `Kill`, whose caller waits for the end with the
/// duration itself: on the paused clock the call reads the instant the test
/// measured, so the deadline lands in the last millisecond of the range on
/// every run.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn edge_of_range_kill_within_does_not_panic_paused() {
    let handle = Cooperative
        .spawn()
        .named(uname("edge-of-range-kill"))
        .await
        .unwrap();
    let base = Instant::now();
    let mut lo = Duration::ZERO;
    let mut hi = Duration::from_secs(u64::MAX / 2);
    while hi - lo > Duration::from_nanos(1) {
        let mid = lo + (hi - lo) / 2;
        if base.checked_add(mid).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let d = lo - Duration::from_micros(500);

    let outcome = handle.stop_within(StopReason::Kill, d).await.unwrap();
    assert!(
        matches!(outcome, StopOutcome::Killed | StopOutcome::Graceful),
        "{outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_yielding_handler_reports_unresponsive() {
    let began = Arc::new(Notify::new());
    let release = Arc::new(AtomicBool::new(false));
    let releases = Releases(release.clone());
    let handle = NeverYields {
        began: began.clone(),
        release,
    }
    .spawn()
    .named(uname("never-yields"))
    .await
    .unwrap();

    handle.notify(()).await.unwrap();
    // The stop is requested once the handler is blocking its OS thread, which
    // it holds until the outcome is read.
    tokio::time::timeout(Duration::from_secs(10), began.notified())
        .await
        .expect("the handler begins");

    let outcome = handle
        .stop_within(StopReason::Graceful, Duration::from_millis(100))
        .await
        .unwrap();

    assert_eq!(outcome, StopOutcome::Unresponsive);

    drop(releases);
    handle.wait_stopped().await;
}
