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
    ActorResult, AskError, ChildEvent, RestartType, SendError, StopReason, SupervisionAction,
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

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

enum Order {
    /// Starts a child and stops it, then holds the handler at a gate.
    Start,
    Probe(u32),
}

/// Holds its handler at a gate with one child death recorded and not
/// delivered, so a stop requested then ends its loop with that death left for
/// the final deliveries, which it holds at a second gate.
struct Boss {
    in_handler: Arc<Notify>,
    handler_gate: Arc<Notify>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    handled: Seen,
    saw: Reason,
}

impl Actor for Boss {
    type Message = Order;
    type Response = ();

    async fn handle(&mut self, msg: Order, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Order::Start => {
                let child = ctx
                    .spawn_child(|| Idle)
                    .restart_type(RestartType::Temporary)
                    .await?;
                child.stop(StopReason::Cancelled).await.unwrap();
                child.wait_stopped().await;
                self.in_handler.notify_one();
                self.handler_gate.notified().await;
            }
            Order::Probe(n) => self.handled.lock().unwrap().push(n),
        }
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if event.action == SupervisionAction::SupervisorStopping {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.saw.lock().unwrap() = Some(reason.clone());
        Ok(())
    }
}

#[test]
fn a_message_and_a_stop_accepted_during_a_final_delivery_change_nothing() {
    common::run(2, async {
        let in_handler = Arc::new(Notify::new());
        let handler_gate = Arc::new(Notify::new());
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let handled: Seen = Arc::default();
        let saw: Reason = Arc::default();
        let sup = Boss {
            in_handler: in_handler.clone(),
            handler_gate: handler_gate.clone(),
            entered: entered.clone(),
            release: release.clone(),
            handled: handled.clone(),
            saw: saw.clone(),
        }
        .spawn()
        .supervisor()
        .await
        .unwrap();

        sup.notify(Order::Start).await.unwrap();
        timeout(BOUND, in_handler.notified())
            .await
            .expect("the handler holds the loop");
        sup.stop(StopReason::Graceful).await.unwrap();
        handler_gate.notify_one();
        timeout(BOUND, entered.notified())
            .await
            .expect("the final delivery of the child's death is entered");

        sup.notify(Order::Probe(7))
            .await
            .expect("a message is accepted during a final delivery");
        let mut ask = Box::pin(sup.send(Order::Probe(8)));
        assert!(
            poll_once(ask.as_mut()),
            "the ask is accepted during a final delivery"
        );
        sup.stop(StopReason::Cancelled)
            .await
            .expect("a non-Kill request is accepted during a final delivery");

        release.notify_one();
        let reason = timeout(BOUND, sup.wait_stopped())
            .await
            .expect("the supervisor ends");
        assert_eq!(
            reason,
            StopReason::Graceful,
            "the later request had no effect"
        );
        assert_eq!(*saw.lock().unwrap(), Some(StopReason::Graceful));
        assert!(
            handled.lock().unwrap().is_empty(),
            "a message accepted during a final delivery was handled: {:?}",
            handled.lock().unwrap()
        );
        let answer = timeout(BOUND, ask).await.expect("the ask is answered");
        assert!(
            matches!(answer, Err(AskError::ResponseDropped { .. })),
            "the accepted ask is dropped unhandled: {answer:?}"
        );
    });
}
