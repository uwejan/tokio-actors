//! On a `current_thread` runtime that also runs the target's callback, a
//! bounded stop makes no progress until the callback returns: its stop task
//! and the call both need the one thread the callback holds, so the call
//! cannot report `Unresponsive` or abort the target meanwhile.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, StopOutcome, StopReason,
};

const BOUND: Duration = Duration::from_secs(30);

/// Holds its thread in `pre_stop` for longer than the window of the call.
struct Blocker {
    returned: Arc<AtomicBool>,
}

impl Actor for Blocker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        std::thread::sleep(Duration::from_millis(300));
        self.returned.store(true, Ordering::SeqCst);
        true
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_blocking_pre_stop_on_the_callers_own_thread_stalls_the_call() {
    let returned = Arc::new(AtomicBool::new(false));
    let h = Blocker {
        returned: returned.clone(),
    }
    .spawn()
    .await
    .unwrap();

    let outcome = timeout(
        BOUND,
        h.stop_within(StopReason::Graceful, Duration::from_millis(20)),
    )
    .await
    .expect("the call returns once the callback has")
    .unwrap();

    assert!(
        returned.load(Ordering::SeqCst),
        "the call returned while pre_stop still held the thread"
    );
    assert!(
        matches!(outcome, StopOutcome::Graceful | StopOutcome::Killed),
        "no escalation step could be taken while the thread was held: {outcome:?}"
    );
}
