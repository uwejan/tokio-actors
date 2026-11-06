//! A caller of `send`, of `get_status`, or of `send_priority` on a full lane is
//! still waiting when the actor ends, and polls with a waker of its own. The
//! actor's task drops the caller's request, or closes the queue the caller
//! waits on, as the actor ends, and that wakes the caller's waker. A panic of
//! the wake stays out of the task: the actor ends with the reason it was
//! stopped with, and the caller reads the end on its next poll.

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Wake, Waker};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorHandle, ActorResult, AskError, StopReason,
};

const BOUND: Duration = Duration::from_secs(30);

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: AtomicUsize,
}

impl LoudWake {
    fn arm(&self, armed: bool) {
        self.armed.store(armed, Ordering::SeqCst);
    }

    fn woken(&self) -> usize {
        self.woken.load(Ordering::SeqCst)
    }
}

impl Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a waker's wake panics");
        }
    }
}

enum Msg {
    /// Signals `.0` when the handler begins, then waits for `.1`.
    Hold(oneshot::Sender<()>, oneshot::Receiver<()>),
    Echo,
}

struct Probe;

impl Actor for Probe {
    type Message = Msg;
    type Response = u64;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
        match msg {
            Msg::Hold(entered, release) => {
                let _ = entered.send(());
                let _ = release.await;
                Ok(0)
            }
            Msg::Echo => Ok(1),
        }
    }
}

/// An actor whose handler is held, and the gate that releases it. The run loop
/// takes no request from its system channel or its lanes while it is held.
async fn held(config: ActorConfig) -> (ActorHandle<Probe>, oneshot::Sender<()>) {
    let actor = Probe.spawn().with_config(config).await.unwrap();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    actor
        .notify(Msg::Hold(entered_tx, release_rx))
        .await
        .unwrap();
    timeout(BOUND, entered_rx)
        .await
        .expect("the handler is entered")
        .unwrap();
    (actor, release_tx)
}

fn loud() -> (Arc<LoudWake>, Waker) {
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    (loud, waker)
}

/// Polls `future` once with `waker`: it must be pending.
fn poll_pending<T>(future: &mut Pin<Box<impl Future<Output = T>>>, waker: &Waker) {
    assert!(future
        .as_mut()
        .poll(&mut Context::from_waker(waker))
        .is_pending());
}

/// A status request is still waiting when the actor ends, held beside an
/// unopened channel, and its caller's waker panics when the actor's task drops
/// the request.
#[test]
fn a_status_caller_still_waiting_at_the_end_whose_waker_panics_leaves_the_stop_reason() {
    common::run(2, async {
        let (actor, release) = held(ActorConfig::default()).await;
        let (caller, waker) = loud();
        let mut status = Box::pin(actor.get_status());
        poll_pending(&mut status, &waker);

        actor.stop(StopReason::Graceful).await.unwrap();
        caller.arm(true);
        release.send(()).unwrap();
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends");
        caller.arm(false);

        assert_eq!(
            reason,
            StopReason::Graceful,
            "a status caller's waker changed how the actor ended"
        );
        assert_eq!(caller.woken(), 1, "the caller was woken once");
        match timeout(BOUND, status)
            .await
            .expect("the caller reads the end")
        {
            Err(AskError::ResponseDropped { reason }) => {
                assert_eq!(reason, StopReason::Graceful)
            }
            other => panic!("expected the end of the actor, got {other:?}"),
        }
    });
}

/// A request is still queued behind the held handler when the actor ends, and
/// its caller's waker panics when the actor's task drops the request unanswered.
#[test]
fn a_request_still_queued_at_the_end_whose_callers_waker_panics_leaves_the_stop_reason() {
    common::run(2, async {
        let (actor, release) = held(ActorConfig::default()).await;
        let (caller, waker) = loud();
        let mut request = Box::pin(actor.send(Msg::Echo));
        poll_pending(&mut request, &waker);

        actor.stop(StopReason::Graceful).await.unwrap();
        caller.arm(true);
        release.send(()).unwrap();
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends");
        caller.arm(false);

        assert_eq!(
            reason,
            StopReason::Graceful,
            "a queued request's waker changed how the actor ended"
        );
        assert_eq!(caller.woken(), 1, "the caller was woken once");
        match timeout(BOUND, request)
            .await
            .expect("the caller reads the end")
        {
            Err(AskError::ResponseDropped { reason }) => {
                assert_eq!(reason, StopReason::Graceful)
            }
            other => panic!("expected the end of the actor, got {other:?}"),
        }
    });
}

/// Two status requests are still waiting when the actor ends, one held beside
/// the channel the second opened and one queued in it, and both callers' wakers
/// panic. The scenario runs in a child process: a panic that escaped the first
/// drop would meet the second wake during its unwind and end the process.
#[test]
fn two_status_callers_still_waiting_at_the_end_whose_wakers_panic_leave_the_stop_reason() {
    if !common::in_a_child_process(
        "two_status_callers_still_waiting_at_the_end_whose_wakers_panic_leave_the_stop_reason",
    ) {
        return;
    }
    common::run(2, async {
        let (actor, release) = held(ActorConfig::default()).await;
        let (first, first_waker) = loud();
        let (second, second_waker) = loud();
        let mut first_status = Box::pin(actor.get_status());
        let mut second_status = Box::pin(actor.get_status());
        poll_pending(&mut first_status, &first_waker);
        poll_pending(&mut second_status, &second_waker);

        actor.stop(StopReason::Graceful).await.unwrap();
        first.arm(true);
        second.arm(true);
        release.send(()).unwrap();
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends");
        first.arm(false);
        second.arm(false);

        assert_eq!(
            reason,
            StopReason::Graceful,
            "a status caller's waker changed how the actor ended"
        );
        assert_eq!(first.woken(), 1, "the first caller was woken once");
        assert_eq!(second.woken(), 1, "the second caller was woken once");
        assert!(matches!(
            timeout(BOUND, first_status)
                .await
                .expect("the first reads the end"),
            Err(AskError::ResponseDropped { .. })
        ));
        assert!(matches!(
            timeout(BOUND, second_status)
                .await
                .expect("the second reads the end"),
            Err(AskError::ResponseDropped { .. })
        ));
    });
}

/// A status request waits for room in a full system channel, and its caller's
/// waker panics when the actor's task closes the channel at its end.
#[test]
fn a_status_caller_waiting_for_room_whose_waker_panics_at_the_end_leaves_the_stop_reason() {
    common::run(2, async {
        let (actor, release) = held(ActorConfig::default()).await;
        // One request is held beside the channel and 64 fill it.
        let mut queued = Vec::new();
        for _ in 0..65 {
            let mut status = Box::pin(actor.get_status());
            poll_pending(&mut status, Waker::noop());
            queued.push(status);
        }
        let (caller, waker) = loud();
        let mut waiting = Box::pin(actor.get_status());
        poll_pending(&mut waiting, &waker);

        actor.stop(StopReason::Graceful).await.unwrap();
        caller.arm(true);
        release.send(()).unwrap();
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends");
        caller.arm(false);

        assert_eq!(
            reason,
            StopReason::Graceful,
            "a status caller's waker changed how the actor ended"
        );
        assert_eq!(caller.woken(), 1, "the caller was woken once");
        match timeout(BOUND, waiting)
            .await
            .expect("the caller reads the end")
        {
            Err(AskError::Closed | AskError::ResponseDropped { .. }) => {}
            other => panic!("expected the end of the actor, got {other:?}"),
        }
    });
}

/// A priority request waits for room on a full lane, and its sender's waker
/// panics when the actor's task closes the lane at its end.
#[test]
fn a_priority_sender_waiting_for_room_whose_waker_panics_at_the_end_leaves_the_stop_reason() {
    common::run(2, async {
        let (actor, release) = held(ActorConfig::default().with_priority_lane(1)).await;
        // The first request fills the lane of one slot.
        let mut first = Box::pin(actor.send_priority(Msg::Echo));
        poll_pending(&mut first, Waker::noop());
        let (sender, waker) = loud();
        let mut second = Box::pin(actor.send_priority(Msg::Echo));
        poll_pending(&mut second, &waker);

        actor.stop(StopReason::Graceful).await.unwrap();
        sender.arm(true);
        release.send(()).unwrap();
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends");
        sender.arm(false);

        assert_eq!(
            reason,
            StopReason::Graceful,
            "a priority sender's waker changed how the actor ended"
        );
        assert_eq!(sender.woken(), 1, "the sender was woken once");
        assert!(matches!(
            timeout(BOUND, second)
                .await
                .expect("the sender reads the end"),
            Err(AskError::Closed)
        ));
    });
}
