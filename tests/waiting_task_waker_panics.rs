//! A task that waits for an actor's end polls with a waker of its own. The
//! crate wakes that waker on the task's behalf, from the actor's own task when
//! the actor publishes its status and from the task of a caller that raises a
//! stop. A panic of the wake stays where the waker is woken: the actor ends
//! with its own reason, the caller of the stop returns, and the waiting task
//! reads the end on its next poll.

mod common;

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, StopReason,
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

/// Fails on message 0 and answers every other one.
struct Fails;

impl Actor for Fails {
    type Message = u64;
    type Response = u64;

    async fn handle(&mut self, msg: u64, _ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
        if msg == 0 {
            return Err(ActorError::user("boom"));
        }
        Ok(msg)
    }
}

/// An actor that never fails by itself.
struct Idle;

impl Actor for Idle {
    type Message = u64;
    type Response = u64;

    async fn handle(&mut self, msg: u64, _ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
        Ok(msg)
    }
}

/// The actor's own task publishes its status, which wakes a waiter whose
/// waker panics. The panic stays out of the actor's task, so the actor ends
/// with the reason its handler failed with, and the waiter reads it.
#[test]
fn a_wait_stopped_waker_that_panics_when_the_actor_fails_does_not_change_its_reason() {
    common::run(2, async {
        let actor = Fails.spawn().await.unwrap();
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut cx = Context::from_waker(&waker);
        let mut waiter = Box::pin(actor.wait_stopped());
        assert!(waiter.as_mut().poll(&mut cx).is_pending());
        loud.arm(true);

        let _ = timeout(BOUND, actor.send(0)).await.expect("the request");
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends");
        loud.arm(false);

        assert_eq!(
            reason,
            StopReason::Failure(ActorError::user("boom")),
            "a waiter's waker that panicked changed how the actor ended"
        );
        assert_eq!(loud.woken(), 1, "the waiter's waker was woken once");
        let mut quiet = Context::from_waker(Waker::noop());
        assert_eq!(
            waiter.as_mut().poll(&mut quiet),
            Poll::Ready(reason),
            "the waiter reads the end on its next poll"
        );
    });
}

/// A caller stops the actor, and the raise wakes a waiter whose waker panics:
/// the panic stays out of the caller's task, and the actor ends with the
/// reason of the stop.
#[test]
fn a_wait_stopped_waker_that_panics_does_not_unwind_into_the_task_that_stops_the_actor() {
    common::run(2, async {
        for reason in [StopReason::Graceful, StopReason::Kill] {
            let actor = Idle.spawn().await.unwrap();
            let loud = Arc::new(LoudWake::default());
            let waker = Waker::from(loud.clone());
            let mut cx = Context::from_waker(&waker);
            let mut waiter = Box::pin(actor.wait_stopped());
            assert!(waiter.as_mut().poll(&mut cx).is_pending());
            loud.arm(true);

            let stopped = tokio::spawn({
                let actor = actor.clone();
                let reason = reason.clone();
                async move { actor.stop(reason).await }
            })
            .await;
            loud.arm(false);

            let stopped = stopped.unwrap_or_else(|err| {
                panic!("{reason:?}: the stopper's task panicked in a waker it does not own: {err}")
            });
            assert!(stopped.is_ok(), "{reason:?}: the stop was accepted");
            assert_eq!(loud.woken(), 1, "{reason:?}: the waker was woken once");
            assert_eq!(
                timeout(BOUND, actor.wait_stopped())
                    .await
                    .expect("the actor ends"),
                reason
            );
        }
    });
}
