use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::time::sleep;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorResult, StopReason,
};

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

    sleep(Duration::from_millis(10)).await;
    assert!(started_flag.load(Ordering::SeqCst));

    handle.notify(Msg::Ping).await.unwrap();
    handle.stop(StopReason::Graceful).await.unwrap();
    sleep(Duration::from_millis(10)).await;

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
    let handle = actor
        .spawn()
        .named("fail-pre-start")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    // Give the actor time to fail
    sleep(Duration::from_millis(50)).await;

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

    // Actor should be dead
    assert!(
        !handle.is_alive(),
        "actor must be dead after pre_start failure"
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

    sleep(Duration::from_millis(10)).await;

    // First stop attempt -- should be rejected
    handle.stop(StopReason::Graceful).await.unwrap();
    sleep(Duration::from_millis(10)).await;
    assert!(handle.is_alive(), "actor must survive first rejected stop");
    assert_eq!(reject_count.load(Ordering::SeqCst), 1);

    // Actor can still process messages after rejected stop
    handle.notify(Msg::Ping).await.unwrap();

    // Second stop attempt -- should also be rejected
    handle.stop(StopReason::Graceful).await.unwrap();
    sleep(Duration::from_millis(10)).await;
    assert!(handle.is_alive(), "actor must survive second rejected stop");
    assert_eq!(reject_count.load(Ordering::SeqCst), 2);

    // Third stop attempt -- should succeed (no rejections remaining)
    handle.stop(StopReason::Graceful).await.unwrap();
    sleep(Duration::from_millis(10)).await;
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

    sleep(Duration::from_millis(10)).await;

    // Cancelled stop bypasses pre_stop entirely
    handle.stop(StopReason::Cancelled).await.unwrap();
    sleep(Duration::from_millis(10)).await;

    assert_eq!(
        reject_count.load(Ordering::SeqCst),
        0,
        "pre_stop must NOT be called for Cancelled stops"
    );
    assert!(!handle.is_alive(), "actor must be dead after forced stop");
    assert!(stopped_flag.load(Ordering::SeqCst), "on_stopped must fire");
}
