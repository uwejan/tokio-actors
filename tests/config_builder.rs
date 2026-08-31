use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, SpawnError,
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
async fn config_defaults_when_not_specified() {
    let actor = DummyActor.spawn().named("cfg-defaults").await.unwrap();
    assert!(actor.is_alive());
    assert_eq!(actor.mailbox_capacity(), 64); // Default capacity
}

#[tokio::test]
async fn config_can_be_passed_via_builder() {
    let config = ActorConfig::default();
    let actor = DummyActor
        .spawn()
        .named("cfg-via-builder")
        .with_config(config)
        .await
        .unwrap();
    assert!(actor.is_alive());
}

#[tokio::test]
async fn config_with_custom_mailbox_via_builder() {
    let config = ActorConfig::default().with_mailbox_capacity(128);
    let actor = DummyActor
        .spawn()
        .named("cfg-custom-mailbox")
        .with_config(config)
        .await
        .unwrap();
    assert_eq!(actor.mailbox_capacity(), 128);
}

#[tokio::test]
async fn zero_mailbox_capacity_returns_spawn_error_instead_of_panicking() {
    let config = ActorConfig::default().with_mailbox_capacity(0);
    let result = DummyActor
        .spawn()
        .named("cfg-zero-mailbox")
        .with_config(config)
        .await;

    match result {
        Err(SpawnError::ZeroMailboxCapacity) => {}
        Ok(_) => panic!("spawn must fail on a zero-capacity mailbox"),
        Err(other) => panic!("expected SpawnError::ZeroMailboxCapacity, got: {other}"),
    }
}
