//! The walk of a shutdown is a task of the system on the runtime current at
//! the call, so the call needs a runtime that is driven and not only entered:
//! polled under an entered runtime nobody drives, it stays pending and stops
//! nothing, and it completes once that runtime is driven.

use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, ActorSystem, StopOutcome,
};

const BOUND: Duration = Duration::from_secs(60);

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn a_shutdown_polled_under_an_entered_runtime_nobody_drives_waits_until_the_runtime_is_driven() {
    let sys = ActorSystem::create(format!("entered-not-driven-{}", uuid::Uuid::new_v4())).unwrap();

    // The root lives on a runtime that runs on its own.
    let host = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let root = host
        .block_on(async { Idle.spawn().named("root").on_system(&sys).await })
        .unwrap();

    // The runtime the call is made on has the time driver and is entered, but
    // no thread drives it yet.
    let caller = Builder::new_current_thread().enable_all().build().unwrap();
    let entered = caller.enter();
    let mut call = Box::pin(sys.shutdown());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        call.as_mut().poll(&mut cx).is_pending(),
        "the walk is a task of an undriven runtime and has not run"
    );
    drop(entered);

    // Every task of the host gets turns, and the root is still running: the
    // walk asked nothing of it.
    host.block_on(async {
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
    });
    assert_eq!(root.status(), ActorStatus::Running);
    assert!(matches!(call.as_mut().poll(&mut cx), Poll::Pending));

    // Driving the runtime lets the walk run and the call complete.
    let report = caller
        .block_on(async { timeout(BOUND, call.as_mut()).await })
        .expect("the call completes once its runtime is driven");
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(report.outcomes[0].1.outcome, StopOutcome::Graceful);
    assert_eq!(root.status(), ActorStatus::Stopped);
}
