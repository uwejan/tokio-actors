//! A message accepted while the actor runs `on_stopped` (or a supervisor's
//! final deliveries) is dropped unhandled, a stop request other than `Kill`
//! made then is accepted and has no effect, and a request made after
//! `on_stopped` has returned is refused.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, AskError, SendError, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(30);

/// Polls an ask once, which enqueues its message, and returns whether it is
/// still waiting for the answer.
fn poll_once<F: std::future::Future + ?Sized>(ask: std::pin::Pin<&mut F>) -> bool {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    ask.poll(&mut cx).is_pending()
}

type Seen = Arc<Mutex<Vec<u32>>>;
type Reason = Arc<Mutex<Option<StopReason>>>;

/// Holds `on_stopped` at a gate and records the reason it received and every
/// message its handler saw.
struct Held {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    handled: Seen,
    saw: Reason,
}

impl Actor for Held {
    type Message = u32;
    type Response = ();

    async fn handle(&mut self, msg: u32, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.handled.lock().unwrap().push(msg);
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.saw.lock().unwrap() = Some(reason.clone());
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[test]
fn a_message_and_a_stop_accepted_during_on_stopped_change_nothing() {
    common::run(2, async {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let handled: Seen = Arc::default();
        let saw: Reason = Arc::default();
        let h = Held {
            entered: entered.clone(),
            release: release.clone(),
            handled: handled.clone(),
            saw: saw.clone(),
        }
        .spawn()
        .await
        .unwrap();

        h.stop(StopReason::Graceful).await.unwrap();
        timeout(BOUND, entered.notified())
            .await
            .expect("on_stopped is entered");

        // The mailbox is open while on_stopped runs: both the notify and the
        // ask are accepted, and so is a stop request that is not a Kill.
        h.notify(7)
            .await
            .expect("a message is accepted while on_stopped runs");
        let mut ask = Box::pin(h.send(8));
        assert!(
            poll_once(ask.as_mut()),
            "the ask is accepted while on_stopped runs"
        );
        h.stop(StopReason::Cancelled)
            .await
            .expect("a non-Kill request is accepted while on_stopped runs");

        release.notify_one();
        let reason = timeout(BOUND, h.wait_stopped())
            .await
            .expect("the actor ends");
        assert_eq!(
            reason,
            StopReason::Graceful,
            "the later request had no effect"
        );
        assert_eq!(*saw.lock().unwrap(), Some(StopReason::Graceful));
        assert!(
            handled.lock().unwrap().is_empty(),
            "a message accepted during on_stopped was handled: {:?}",
            handled.lock().unwrap()
        );
        let answer = timeout(BOUND, ask).await.expect("the ask is answered");
        assert!(
            matches!(answer, Err(AskError::ResponseDropped { .. })),
            "the accepted ask is dropped unhandled: {answer:?}"
        );

        // After on_stopped has returned every request but a Kill is refused.
        assert!(matches!(
            h.stop(StopReason::Graceful).await,
            Err(SendError::Closed)
        ));
        assert!(matches!(h.notify(9).await, Err(SendError::Closed)));
    });
}

// ---------------------------------------------------------------------------
// A supervisor's final deliveries
// ---------------------------------------------------------------------------
