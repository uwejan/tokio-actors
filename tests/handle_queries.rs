use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, ActorSystem,
};

#[derive(Default, Debug)]
struct TestActor {
    value: i64,
}

#[derive(Clone)]
#[allow(dead_code)]
enum Msg {
    Inc,
    Get,
}

#[allow(dead_code)]
enum Resp {
    Ack,
    Value(i64),
}

impl Actor for TestActor {
    type Message = Msg;
    type Response = Resp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            Msg::Inc => {
                self.value += 1;
                Ok(Resp::Ack)
            }
            Msg::Get => Ok(Resp::Value(self.value)),
        }
    }
}

#[tokio::test]
async fn mailbox_len_and_available_sum_to_capacity() {
    let config = ActorConfig::default().with_mailbox_capacity(10);
    let actor = TestActor::default()
        .spawn()
        .named("hq-mailbox-sum")
        .with_config(config)
        .await
        .unwrap();

    // Fill mailbox with messages
    for _ in 0..5 {
        actor.try_notify(Msg::Inc).unwrap();
    }

    let len = actor.mailbox_len();
    let available = actor.mailbox_available();
    assert!(len <= 5, "Mailbox should have at most 5 messages");
    assert_eq!(len + available, 10, "Len + available should equal capacity");
}

#[tokio::test]
async fn handle_inequality_across_systems() {
    let sys_a = ActorSystem::create(format!("hq-sys-a-{}", uuid::Uuid::new_v4())).unwrap();
    let sys_b = ActorSystem::create(format!("hq-sys-b-{}", uuid::Uuid::new_v4())).unwrap();

    let actor_a = TestActor::default()
        .spawn()
        .named("shared-name")
        .on_system(&sys_a)
        .await
        .unwrap();
    let actor_b = TestActor::default()
        .spawn()
        .named("shared-name")
        .on_system(&sys_b)
        .await
        .unwrap();

    assert_eq!(
        actor_a.id(),
        actor_b.id(),
        "the same name yields the same ActorId regardless of system"
    );
    assert_ne!(
        actor_a, actor_b,
        "identical ActorId in different systems must NOT compare equal"
    );
}
