//! The window of `stop_within` counts from the first poll of the call, not
//! from the moment the future is created: a call that is created, left
//! unpolled for longer than its whole window and then polled still gives the
//! actor the full window before the stop escalates.

mod common;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, StopOutcome, StopReason,
};

/// The longest a wait may take on the paused clock; it only limits how long a
/// real hang takes to report.
const BOUND: Duration = Duration::from_secs(600);

/// How many times the test yields to let every ready task and timer run.
const SETTLE_YIELDS: usize = 200;

/// Declines every graceful stop; a `Kill` is not declinable.
struct Declines;

impl Actor for Declines {
    type Message = ();
    type Response = ();

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

async fn settle() {
    for _ in 0..SETTLE_YIELDS {
        tokio::task::yield_now().await;
    }
}

#[test]
fn the_window_of_stop_within_counts_from_the_first_poll_not_from_the_creation_of_the_future() {
    common::run_paused(async {
        let actor = Declines.spawn().await.unwrap();
        let mut call: Pin<Box<dyn Future<Output = _> + '_>> =
            Box::pin(actor.stop_within(StopReason::Graceful, Duration::from_secs(10)));

        // Left unpolled for six windows.
        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
        assert_ne!(
            actor.status(),
            ActorStatus::Stopped,
            "an unpolled call has taken no step"
        );

        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            matches!(call.as_mut().poll(&mut cx), Poll::Pending),
            "the call waits for its window"
        );

        // Inside the window of the first poll the actor that declines its
        // graceful stop is still there.
        tokio::time::advance(Duration::from_secs(9)).await;
        settle().await;
        assert_ne!(
            actor.status(),
            ActorStatus::Stopped,
            "nothing is forced before the window of the first poll has passed"
        );

        // Past it the stop escalates to a kill.
        tokio::time::advance(Duration::from_secs(2)).await;
        let outcome = timeout(BOUND, call)
            .await
            .expect("the call returns once the window of its first poll has passed")
            .unwrap();
        assert_eq!(outcome, StopOutcome::Killed);
        assert_eq!(actor.status(), ActorStatus::Stopped);
    });
}
