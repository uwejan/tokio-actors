//! A bounded stop whose task is lost after its window has passed still ends a
//! target that runs on a runtime without the time driver: the `Kill` the
//! hand-off raises aborts the target at once, as every `Kill` on such a
//! runtime does.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::Notify;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, StopReason,
};

/// The longest the end of the target may take: only a hang reaches it.
const HANG_BOUND: Duration = Duration::from_secs(10);

/// Parks in `pre_stop` at an `.await` that only an abort or the release ends.
struct Stuck {
    release: Arc<Notify>,
}

impl Actor for Stuck {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.release.notified().await;
        false
    }
}

#[test]
fn a_lost_stop_task_aborts_a_target_parked_in_a_hook_on_a_runtime_without_timers() {
    // The target's runtime has no time driver.
    let target_rt = Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    // The caller's runtime has it, and is driven only by `block_on`.
    let caller_rt = Builder::new_current_thread().enable_all().build().unwrap();
    let watcher = Builder::new_current_thread().enable_all().build().unwrap();

    let release = Arc::new(Notify::new());
    let h = target_rt.block_on({
        let release = release.clone();
        async move { Stuck { release }.spawn().await.unwrap() }
    });

    // One poll of the call raises the request and spawns the stop task on the
    // caller's runtime; the future is dropped and the runtime is not run
    // again, so the task never starts.
    let window = Duration::from_millis(50);
    caller_rt.block_on({
        let h = h.clone();
        async move {
            let mut call = Box::pin(h.stop_within(StopReason::Graceful, window));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(
                matches!(call.as_mut().poll(&mut cx), Poll::Pending),
                "the call waits for its task"
            );
        }
    });
    // The window passes while the task has not run; then the caller's runtime
    // ends, and the lost task hands its steps to the target's runtime.
    std::thread::sleep(window + Duration::from_millis(100));
    drop(caller_rt);

    let ended =
        watcher.block_on(async { tokio::time::timeout(HANG_BOUND, h.wait_stopped()).await });
    // Release the parked hook either way, so a failure ends the test too.
    release.notify_one();
    let reason = ended.expect("the Kill of the hand-off aborted the target");
    assert_eq!(reason, StopReason::Kill);
    target_rt.shutdown_background();
}
