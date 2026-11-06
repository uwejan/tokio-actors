//! A spawn polled with no Tokio runtime current answers `MissingRuntime`, and a
//! spawn with a mailbox capacity of zero is refused as `ZeroMailboxCapacity`,
//! with a runtime current and with none.

use std::future::{Future, IntoFuture};
use std::task::{Context, Poll, Waker};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, SpawnError,
};

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn a_spawn_polled_with_no_runtime_current_answers_missing_runtime() {
    assert!(tokio::runtime::Handle::try_current().is_err());
    let mut call = Box::pin(Idle.spawn().into_future());
    match call.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(Err(SpawnError::MissingRuntime)) => {}
        other => panic!(
            "expected MissingRuntime, got {:?}",
            other.map(|r| r.map(|_| ()))
        ),
    }
}

#[test]
fn a_spawn_with_zero_mailbox_capacity_is_refused_with_and_without_a_runtime() {
    let config = || ActorConfig::default().with_mailbox_capacity(0);

    let mut call = Box::pin(Idle.spawn().with_config(config()).into_future());
    let without = call.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    assert!(
        matches!(without, Poll::Ready(Err(SpawnError::ZeroMailboxCapacity))),
        "no runtime current: {:?}",
        without.map(|r| r.map(|_| ()))
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let with = runtime.block_on(Idle.spawn().with_config(config()).into_future());
    assert!(
        matches!(with, Err(SpawnError::ZeroMailboxCapacity)),
        "a runtime current: {:?}",
        with.map(|_| ())
    );
}
