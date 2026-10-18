use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::time::{sleep, timeout};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorResult, ActorStatus, ActorSystem, SendError, Shutdown,
    SpawnError, StopReason, TrySendError,
};

/// The bound on every outcome a test awaits.
const BOUND: Duration = Duration::from_secs(10);

/// Polls `pred` until it holds; fails the test after `BOUND`.
async fn eventually(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + BOUND;
    while !pred() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        sleep(Duration::from_millis(1)).await;
    }
}

// ---------------------------------------------------------------------------
// Shared message/response types
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Msg {
    Ping,
}

enum Resp {
    Ack,
}

// ---------------------------------------------------------------------------
// Test 1: Basic lifecycle hooks (existing test, updated for new signatures)
// ---------------------------------------------------------------------------

struct LifecycleActor {
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl LifecycleActor {
    fn new() -> (Self, Arc<AtomicBool>, Arc<AtomicBool>) {
        let started = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        (
            Self {
                started: started.clone(),
                stopped: stopped.clone(),
            },
            started,
            stopped,
        )
    }
}

impl Actor for LifecycleActor {
    type Message = Msg;
    type Response = Resp;

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            Msg::Ping => Ok(Resp::Ack),
        }
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_hooks_fire_without_supervision() {
    let (actor, started_flag, stopped_flag) = LifecycleActor::new();
    let handle = actor
        .spawn()
        .named("lifecycle")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    eventually("on_started to run", || started_flag.load(Ordering::SeqCst)).await;

    handle.notify(Msg::Ping).await.unwrap();
    handle.stop(StopReason::Graceful).await.unwrap();
    timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the actor stops");

    assert!(stopped_flag.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------------------
// Test 2: pre_start failure prevents startup
// ---------------------------------------------------------------------------

struct FailingPreStartActor {
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl FailingPreStartActor {
    fn new() -> (Self, Arc<AtomicBool>, Arc<AtomicBool>) {
        let started = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        (
            Self {
                started: started.clone(),
                stopped: stopped.clone(),
            },
            started,
            stopped,
        )
    }
}

impl Actor for FailingPreStartActor {
    type Message = Msg;
    type Response = Resp;

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("pre_start validation failed"))
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            Msg::Ping => Ok(Resp::Ack),
        }
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_start_failure_prevents_startup() {
    let (actor, started_flag, stopped_flag) = FailingPreStartActor::new();
    let name = "fail-pre-start";
    let result = actor
        .spawn()
        .named(name)
        .with_config(ActorConfig::default())
        .await;

    // v0.8: the failed pre_start surfaces synchronously to the caller as the
    // spawn ack, instead of dying silently in the background. By the time
    // `.await` resolves, teardown has already fully run (no sleep needed).
    match result {
        Err(SpawnError::Init(_)) => {}
        Ok(_) => panic!("pre_start failure must prevent startup, but spawn succeeded"),
        Err(other) => panic!("expected SpawnError::Init, got: {other}"),
    }

    // on_started should never have fired
    assert!(
        !started_flag.load(Ordering::SeqCst),
        "on_started must NOT fire when pre_start fails"
    );

    // on_stopped should never have fired (matches OTP: init failure = no terminate)
    assert!(
        !stopped_flag.load(Ordering::SeqCst),
        "on_stopped must NOT fire when pre_start fails"
    );

    // The name is released: a failed init never leaves a registered actor behind.
    assert!(
        ActorSystem::default()
            .get::<FailingPreStartActor>(name)
            .is_none(),
        "actor name must not remain registered after a pre_start failure"
    );
}

// ---------------------------------------------------------------------------
// Test 3: pre_stop rejection keeps actor alive
// ---------------------------------------------------------------------------

struct PreStopRejectActor {
    reject_count: Arc<AtomicU32>,
    rejections_remaining: u32,
    stopped: Arc<AtomicBool>,
}

impl PreStopRejectActor {
    /// Create an actor that rejects the first `n` graceful stop attempts.
    fn new(reject_n: u32) -> (Self, Arc<AtomicU32>, Arc<AtomicBool>) {
        let reject_count = Arc::new(AtomicU32::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        (
            Self {
                reject_count: reject_count.clone(),
                rejections_remaining: reject_n,
                stopped: stopped.clone(),
            },
            reject_count,
            stopped,
        )
    }
}

impl Actor for PreStopRejectActor {
    type Message = Msg;
    type Response = Resp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            Msg::Ping => Ok(Resp::Ack),
        }
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        if self.rejections_remaining > 0 {
            self.rejections_remaining -= 1;
            self.reject_count.fetch_add(1, Ordering::SeqCst);
            false // reject this stop
        } else {
            true // allow stop
        }
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_stop_rejection_keeps_actor_alive() {
    let (actor, reject_count, stopped_flag) = PreStopRejectActor::new(2);
    let handle = actor
        .spawn()
        .named("pre-stop-reject")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    // First stop attempt - should be rejected. A reply to a later message
    // proves the actor kept running after the veto.
    handle.stop(StopReason::Graceful).await.unwrap();
    eventually("the first veto", || {
        reject_count.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(
        matches!(handle.send(Msg::Ping).await, Ok(Resp::Ack)),
        "actor must survive first rejected stop"
    );
    assert!(handle.is_alive(), "actor must survive first rejected stop");
    assert_eq!(reject_count.load(Ordering::SeqCst), 1);

    // Actor can still process messages after rejected stop
    handle.notify(Msg::Ping).await.unwrap();

    // Second stop attempt - should also be rejected
    handle.stop(StopReason::Graceful).await.unwrap();
    eventually("the second veto", || {
        reject_count.load(Ordering::SeqCst) == 2
    })
    .await;
    assert!(
        matches!(handle.send(Msg::Ping).await, Ok(Resp::Ack)),
        "actor must survive second rejected stop"
    );
    assert!(handle.is_alive(), "actor must survive second rejected stop");
    assert_eq!(reject_count.load(Ordering::SeqCst), 2);

    // Third stop attempt - should succeed (no rejections remaining)
    handle.stop(StopReason::Graceful).await.unwrap();
    timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the actor stops once its rejections are exhausted");
    assert!(
        !handle.is_alive(),
        "actor must stop after rejections exhausted"
    );
    assert!(stopped_flag.load(Ordering::SeqCst), "on_stopped must fire");
}

// ---------------------------------------------------------------------------
// Test 4: pre_stop is bypassed for forced stops (Failure/Cancelled)
// ---------------------------------------------------------------------------

struct AlwaysRejectStopActor {
    reject_count: Arc<AtomicU32>,
    stopped: Arc<AtomicBool>,
}

impl AlwaysRejectStopActor {
    fn new() -> (Self, Arc<AtomicU32>, Arc<AtomicBool>) {
        let reject_count = Arc::new(AtomicU32::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        (
            Self {
                reject_count: reject_count.clone(),
                stopped: stopped.clone(),
            },
            reject_count,
            stopped,
        )
    }
}

impl Actor for AlwaysRejectStopActor {
    type Message = Msg;
    type Response = Resp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            Msg::Ping => Ok(Resp::Ack),
        }
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.reject_count.fetch_add(1, Ordering::SeqCst);
        false // always reject
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_stop_bypasses_pre_stop() {
    let (actor, reject_count, stopped_flag) = AlwaysRejectStopActor::new();
    let handle = actor
        .spawn()
        .named("always-reject")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    // Cancelled stop bypasses pre_stop entirely
    handle.stop(StopReason::Cancelled).await.unwrap();
    timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the cancelled actor stops");

    assert_eq!(
        reject_count.load(Ordering::SeqCst),
        0,
        "pre_stop must NOT be called for Cancelled stops"
    );
    assert!(!handle.is_alive(), "actor must be dead after forced stop");
    assert!(stopped_flag.load(Ordering::SeqCst), "on_stopped must fire");
}

// ---------------------------------------------------------------------------
// Stopped implies not alive, on every exit path
// ---------------------------------------------------------------------------

/// A minimal actor with no lifecycle overrides, used to pin `status() ==
/// Stopped` implying `!is_alive()` across many spawn/stop cycles.
#[derive(Default)]
struct PlainActor;

impl Actor for PlainActor {
    type Message = Msg;
    type Response = Resp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            Msg::Ping => Ok(Resp::Ack),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_implies_not_alive() {
    for i in 0..500 {
        let handle = PlainActor.spawn().await.unwrap();

        handle.stop(StopReason::Graceful).await.unwrap();
        handle.wait_stopped().await;

        assert!(
            !handle.is_alive(),
            "iteration {i}: is_alive() must be false once wait_stopped() resolves"
        );
        let err = handle.notify(Msg::Ping).await.unwrap_err();
        assert!(
            matches!(err, SendError::Closed),
            "iteration {i}: notify must report Closed once the actor has stopped, got {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Sends are refused from stop acceptance onward, throughout a slow child
// cascade
// ---------------------------------------------------------------------------

/// Vetoes every stoppable stop forever - only the `Kill` its own supervisor's
/// `Shutdown::Timeout` escalates to can ever end it.
#[derive(Default)]
struct VetoingChild;

impl Actor for VetoingChild {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }
}

/// A supervisor with one child on a long `Shutdown::Timeout`, so its own
/// stop spends a well-defined window inside the child cascade (after
/// `on_stopped` has already run).
#[derive(Default)]
struct SlowCascadeSupervisor;

impl Actor for SlowCascadeSupervisor {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(VetoingChild::default)
            .shutdown(Shutdown::Timeout(Duration::from_millis(600)))
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Ports the report's probe scenario: a supervisor whose one child vetoes
/// with a long `Shutdown::Timeout`, probed 100ms after `stop(Graceful)` -
/// still well inside the child's 600ms window. Every "already stopped"
/// probe must agree from the instant the stop was accepted, not only once
/// the supervisor's task has fully exited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sends_are_refused_from_stop_acceptance_through_a_slow_child_cascade() {
    let handle = SlowCascadeSupervisor.spawn().supervisor().await.unwrap();

    // Let the child actually finish spawning before stopping the supervisor.
    sleep(Duration::from_millis(30)).await;

    handle.stop(StopReason::Graceful).await.unwrap();

    // Still well inside the child's 600ms Shutdown::Timeout window.
    sleep(Duration::from_millis(100)).await;

    assert_eq!(handle.status(), ActorStatus::Stopping);

    let stop_err = handle.stop(StopReason::Graceful).await.unwrap_err();
    assert!(matches!(stop_err, SendError::Closed));

    assert!(
        handle.get_status().await.is_err(),
        "get_status must report an error once a stop has been accepted"
    );

    assert!(
        !handle.is_alive(),
        "is_alive must be false once the actor has left its loop, even mid-cascade"
    );

    let notify_err = handle.notify(()).await.unwrap_err();
    assert!(matches!(notify_err, SendError::Closed));

    let try_notify_err = handle.try_notify(()).unwrap_err();
    assert!(matches!(try_notify_err, TrySendError::Closed));
}
