use async_trait::async_trait;
use tokio::time::Duration;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason,
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

#[async_trait]
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
async fn is_alive_returns_true_when_actor_running() {
    let actor = TestActor::default().spawn_actor("test", ()).await.unwrap();
    assert!(actor.is_alive(), "Actor should be alive after spawning");
}

#[tokio::test]
async fn is_alive_returns_false_after_stop() {
    let actor = TestActor::default().spawn_actor("test", ()).await.unwrap();
    actor.stop(StopReason::Graceful).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        !actor.is_alive(),
        "Actor should not be alive after stopping"
    );
}

#[tokio::test]
async fn mailbox_queries_show_correct_capacity() {
    let config = ActorConfig::default().with_mailbox_capacity(10);
    let actor = TestActor::default()
        .spawn_actor("test", config)
        .await
        .unwrap();

    assert_eq!(actor.mailbox_capacity(), 10);
    assert_eq!(actor.mailbox_available(), 10);
    assert_eq!(actor.mailbox_len(), 0);
}

#[tokio::test]
async fn mailbox_len_and_available_sum_to_capacity() {
    let config = ActorConfig::default().with_mailbox_capacity(10);
    let actor = TestActor::default()
        .spawn_actor("test", config)
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
async fn handle_equality_based_on_actor_id() {
    let actor1 = TestActor::default()
        .spawn_actor("actor1", ())
        .await
        .unwrap();
    let actor2 = TestActor::default()
        .spawn_actor("actor2", ())
        .await
        .unwrap();
    let actor1_clone = actor1.clone();

    assert_eq!(actor1, actor1_clone, "Handle should equal its clone");
    assert_ne!(actor1, actor2, "Different actors should not be equal");
}

#[tokio::test]
async fn handle_can_be_used_in_hashset() {
    use std::collections::HashSet;

    let actor1 = TestActor::default()
        .spawn_actor("actor1", ())
        .await
        .unwrap();
    let actor2 = TestActor::default()
        .spawn_actor("actor2", ())
        .await
        .unwrap();
    let actor1_clone = actor1.clone();

    let mut set = HashSet::new();
    set.insert(actor1.clone());
    set.insert(actor2.clone());
    set.insert(actor1_clone);

    assert_eq!(set.len(), 2, "HashSet should deduplicate by actor ID");
    assert!(set.contains(&actor1));
    assert!(set.contains(&actor2));
}
