#![allow(dead_code)]
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult,
};

#[derive(Default)]
struct DummyActor;

#[derive(Clone)]
#[allow(dead_code)]
enum Msg {
    Noop,
}

enum Resp {
    Ack,
}

impl Actor for DummyActor {
    type Message = Msg;
    type Response = Resp;

    async fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        Ok(Resp::Ack)
    }
}

#[tokio::test]
async fn builder_pattern_for_mailbox_capacity() {
    let config = ActorConfig::default().with_mailbox_capacity(256);
    assert_eq!(config.mailbox.capacity, 256);

    let actor = DummyActor
        .spawn()
        .named("cfg-mailbox-cap")
        .with_config(config)
        .await
        .unwrap();
    assert_eq!(actor.mailbox_capacity(), 256);
}

#[tokio::test]
async fn oversized_capacities_are_clamped_not_panicking() {
    let config = ActorConfig::default()
        .with_mailbox_capacity(usize::MAX)
        .with_priority_lane(usize::MAX);
    let actor = DummyActor
        .spawn()
        .named("cfg-oversized-capacities")
        .with_config(config)
        .await
        .unwrap();

    assert!(actor.mailbox_capacity() <= tokio::sync::Semaphore::MAX_PERMITS);
    assert!(actor.priority_capacity() <= tokio::sync::Semaphore::MAX_PERMITS);
}
