use std::sync::Arc;
use std::time::Duration;

use tokio::time::{sleep, timeout};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, ActorSystem, SendError, Shutdown, StopReason, TrySendError,
};

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// Polls `pred` until it holds; fails the test after `HANG_BOUND`. The bound only limits how long a real hang takes to report.
async fn eventually(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + HANG_BOUND;
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

// ---------------------------------------------------------------------------
// Test 2: pre_start failure prevents startup
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 3: pre_stop rejection keeps actor alive
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 4: pre_stop is bypassed for forced stops (Failure/Cancelled)
// ---------------------------------------------------------------------------

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

/// Holds every stop in `pre_stop` until the test releases it, then accepts. It
/// signals `entered` on entering `pre_stop`.
struct HeldChild {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl Actor for HeldChild {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.entered.notify_one();
        self.release.notified().await;
        true
    }
}

/// A supervisor with one child on `Shutdown::Infinity` whose stop the test
/// holds, so its own stop spends as long as the test wants inside the child
/// cascade (after `on_stopped` has already run).
struct SlowCascadeSupervisor {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl Actor for SlowCascadeSupervisor {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let entered = self.entered.clone();
        let release = self.release.clone();
        ctx.spawn_child(move || HeldChild {
            entered: entered.clone(),
            release: release.clone(),
        })
        .named("slow-cascade-child")
        .shutdown(Shutdown::Infinity)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Ports the report's probe scenario: a supervisor whose one child holds its
/// stop, probed once `stop(Graceful)` was accepted and while the child still
/// holds the cascade open. Every "already stopped" probe must agree from the
/// moment the supervisor has narrowed its stop lane and closed its mailbox,
/// not only once its task has fully exited. The supervisor publishes `Stopping`
/// earlier, before its final deliveries and `on_stopped`, and a non-`Kill`
/// request accepted in that window has no effect. The probes therefore order on
/// the child's `pre_stop` being entered, which happens only after the lane is
/// narrowed and the cascade has begun. The test releases the child only after
/// the last probe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sends_are_refused_from_stop_acceptance_through_a_slow_child_cascade() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let handle = SlowCascadeSupervisor {
        entered: entered.clone(),
        release: release.clone(),
    }
    .spawn()
    .supervisor()
    .await
    .unwrap();

    // Let the child actually finish spawning before stopping the supervisor.
    let child = ActorSystem::default()
        .get::<HeldChild>("slow-cascade-child")
        .expect("the child is registered once the supervisor started");
    eventually("the child to run", || {
        child.status() == ActorStatus::Running
    })
    .await;

    handle.stop(StopReason::Graceful).await.unwrap();

    // The child holds the cascade open. Its `pre_stop` is entered only after
    // the supervisor narrowed its stop lane and closed its mailbox, and the
    // supervisor reads `Stopping` until released.
    timeout(HANG_BOUND, entered.notified())
        .await
        .expect("the child to enter pre_stop");

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

    assert_eq!(
        handle.status(),
        ActorStatus::Stopping,
        "the cascade was still open for every probe"
    );
    release.notify_one();
    eventually("the supervisor to stop", || {
        handle.status() == ActorStatus::Stopped
    })
    .await;
}
