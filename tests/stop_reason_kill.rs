use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason,
};

// ---------------------------------------------------------------------------
// Shared actor that tracks which lifecycle callbacks fire
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StubActor {
    pre_stop_called: Arc<AtomicBool>,
    on_stopped_called: Arc<AtomicBool>,
}

impl Actor for StubActor {
    type Message = ();
    type Response = ();

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.pre_stop_called.store(true, Ordering::SeqCst);
        true
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.on_stopped_called.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Kill bypasses both pre_stop and on_stopped (Tier 3 termination).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_bypasses_all_callbacks() {
    let pre_stop = Arc::new(AtomicBool::new(false));
    let on_stopped = Arc::new(AtomicBool::new(false));

    let actor = StubActor {
        pre_stop_called: Arc::clone(&pre_stop),
        on_stopped_called: Arc::clone(&on_stopped),
    };

    let handle = actor
        .spawn_actor("kill-test", ActorConfig::default())
        .await
        .unwrap();

    handle.stop(StopReason::Kill).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(
        !pre_stop.load(Ordering::SeqCst),
        "pre_stop must NOT be called on Kill"
    );
    assert!(
        !on_stopped.load(Ordering::SeqCst),
        "on_stopped must NOT be called on Kill"
    );
}

/// Graceful stop DOES call pre_stop and on_stopped (sanity check).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_stop_calls_callbacks() {
    let pre_stop = Arc::new(AtomicBool::new(false));
    let on_stopped = Arc::new(AtomicBool::new(false));

    let actor = StubActor {
        pre_stop_called: Arc::clone(&pre_stop),
        on_stopped_called: Arc::clone(&on_stopped),
    };

    let handle = actor
        .spawn_actor("graceful-test", ActorConfig::default())
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(
        pre_stop.load(Ordering::SeqCst),
        "pre_stop must be called on Graceful"
    );
    assert!(
        on_stopped.load(Ordering::SeqCst),
        "on_stopped must be called on Graceful"
    );
}