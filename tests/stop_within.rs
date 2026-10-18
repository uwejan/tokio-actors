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
    ActorHandle, ActorResult, ActorStatus, ActorSystem, RestartType, SendError, StopOutcome,
    StopReason,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The time an actor is given between a `Kill` and its abort, 120 ms. The
/// suite sets it on the actor whose timing it asserts
/// (`.kill_grace(ASSUMED_KILL_GRACE)`), so the bound does not depend on the
/// crate's default. Not part of the public API, so the value is named here
/// once.
const ASSUMED_KILL_GRACE: Duration = Duration::from_millis(120);

/// The bound on a timed outcome on a real-time runtime. A tighter limit would
/// fail on a host that stalls its threads; the exact timing is pinned on the
/// paused clock, where an outcome lands on its instant.
const HANG_BOUND: Duration = Duration::from_secs(10);

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

/// Polls until a live actor is registered under `name`, or panics after
/// `timeout_ms`.
async fn live_handle<A: Actor>(sys: &ActorSystem, name: &str, timeout_ms: u64) -> ActorHandle<A> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Some(h) = sys.get::<A>(name) {
            if h.is_alive() {
                return h;
            }
        }
        if Instant::now() >= deadline {
            panic!("timed out after {timeout_ms}ms waiting for live actor '{name}'");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

/// Vetoes every stoppable stop forever - only `Kill` can ever end it.
#[derive(Default)]
struct AlwaysVetoes;

impl Actor for AlwaysVetoes {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
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

/// A trivial supervised child.
#[derive(Default)]
struct Worker;

impl Actor for Worker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A supervisor that spawns one `Permanent` `Worker` child under a
/// caller-chosen name.
struct Sup {
    child_name: String,
}

impl Actor for Sup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(Worker::default)
            .named(self.child_name.clone())
            .restart_type(RestartType::Permanent)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cooperative_actor_reports_graceful() {
    let handle = Cooperative
        .spawn()
        .named(uname("cooperative"))
        .await
        .unwrap();

    let outcome = handle
        .stop_within(StopReason::Graceful, Duration::from_secs(2))
        .await
        .unwrap();

    assert_eq!(outcome, StopOutcome::Graceful);
    handle.wait_stopped().await;
}

async fn always_vetoing_actor_is_killed_by_the_deadline(paused: bool) {
    let within = Duration::from_millis(150);

    let handle = AlwaysVetoes
        .spawn()
        .named(uname("always-vetoes"))
        .kill_grace(ASSUMED_KILL_GRACE)
        .await
        .unwrap();

    let started = Instant::now();
    let outcome = handle
        .stop_within(StopReason::Graceful, within)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(outcome, StopOutcome::Killed);
    let bound = if paused {
        within + ASSUMED_KILL_GRACE * 3
    } else {
        HANG_BOUND
    };
    assert!(
        elapsed < bound,
        "expected the ladder to finish well within {bound:?}, took {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn always_vetoing_actor_is_killed_by_the_deadline_multi_thread() {
    always_vetoing_actor_is_killed_by_the_deadline(false).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn always_vetoing_actor_is_killed_by_the_deadline_current_thread() {
    always_vetoing_actor_is_killed_by_the_deadline(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_parked_at_an_await_is_aborted() {
    let started = Arc::new(Notify::new());
    let handle = Hangs {
        started: started.clone(),
    }
    .spawn()
    .named(uname("hangs"))
    .await
    .unwrap();

    handle.notify(()).await.unwrap();
    started.notified().await;

    let outcome = handle
        .stop_within(StopReason::Graceful, Duration::from_millis(100))
        .await
        .unwrap();

    assert_eq!(outcome, StopOutcome::Aborted);
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn already_stopped_actor_reports_closed() {
    let handle = Cooperative
        .spawn()
        .named(uname("already-stopped"))
        .await
        .unwrap();

    handle.stop(StopReason::Kill).await.unwrap();
    handle.wait_stopped().await;

    let err = handle
        .stop_within(StopReason::Graceful, Duration::from_millis(100))
        .await
        .unwrap_err();

    assert!(matches!(err, SendError::Closed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_reason_skips_the_graceful_window() {
    let within = Duration::from_secs(5);

    let handle = AlwaysVetoes
        .spawn()
        .named(uname("kill-skips-graceful"))
        .await
        .unwrap();

    let started = Instant::now();
    let outcome = handle.stop_within(StopReason::Kill, within).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(outcome, StopOutcome::Killed);
    assert!(
        elapsed < within / 2,
        "a Kill reason must skip the graceful window entirely; took {elapsed:?} against a \
         {within:?} window"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervised_permanent_child_is_restarted_after_a_graceful_stop_within() {
    let sys = ActorSystem::create(uname("sys")).unwrap();
    let child_name = uname("child");

    let _sup = Sup {
        child_name: child_name.clone(),
    }
    .spawn()
    .named(uname("sup"))
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();

    let child: ActorHandle<Worker> = live_handle(&sys, &child_name, 2_000).await;

    let outcome = child
        .stop_within(StopReason::Graceful, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(outcome, StopOutcome::Graceful);

    // Permanent restarts even after a graceful stop - a fresh incarnation
    // must reappear live under the same name.
    let restarted: ActorHandle<Worker> = live_handle(&sys, &child_name, 2_000).await;
    assert!(restarted.is_alive());
}

/// A duration right at the edge of what `Instant::checked_add` accepts must
/// not panic when handed to `stop_within`: the timer it eventually reaches
/// rounds a deadline up to the next millisecond with its own unchecked
/// addition, which panics if that deadline was already within the last
/// millisecond of the representable `Instant` range.
///
/// The call reads its own clock a little after the duration was measured, so
/// the duration is measured again for each attempt, with a margin that grows
/// by 900 us per attempt: one of the attempts lands in that last
/// millisecond however long the call takes to start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edge_of_range_within_does_not_panic() {
    for step in 0..16u32 {
        let handle = Cooperative
            .spawn()
            .named(uname("edge-of-range"))
            .await
            .unwrap();

        // Measured after the spawn, so the spawn does not count against the
        // margin.
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
        let d = lo - Duration::from_micros(500) - Duration::from_micros(900) * step;

        let outcome = handle.stop_within(StopReason::Graceful, d).await.unwrap();
        assert_eq!(outcome, StopOutcome::Graceful, "attempt {step}");
    }
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
