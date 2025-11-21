use async_trait::async_trait;
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

#[async_trait]
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

    let actor = DummyActor.spawn_actor("test", config).await.unwrap();
    assert_eq!(actor.mailbox_capacity(), 256);
}

#[tokio::test]
async fn config_can_be_passed_as_unit() {
    let actor = DummyActor.spawn_actor("test", ()).await.unwrap();
    assert!(actor.is_alive());
    assert_eq!(actor.mailbox_capacity(), 64); // Default capacity
}

#[tokio::test]
async fn config_can_be_passed_as_none() {
    let actor = DummyActor.spawn_actor("test", None).await.unwrap();
    assert!(actor.is_alive());
}

#[tokio::test]
async fn config_can_be_passed_as_some() {
    let config = ActorConfig::default();
    let actor = DummyActor.spawn_actor("test", Some(config)).await.unwrap();
    assert!(actor.is_alive());
}

#[tokio::test]
async fn config_can_be_passed_as_reference() {
    let config = ActorConfig::default().with_mailbox_capacity(128);
    let actor = DummyActor.spawn_actor("test", &config).await.unwrap();
    assert_eq!(actor.mailbox_capacity(), 128);
}
