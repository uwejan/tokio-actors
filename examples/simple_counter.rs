use async_trait::async_trait;
use tokio_actors::{
    actor::context::ActorContext,
    actor::{Actor, ActorExt},
    ActorConfig, ActorResult, StopReason,
};

#[derive(Default)]
struct Counter {
    value: i64,
}

#[derive(Clone)]
enum Msg {
    Increment(i64),
    Get,
}

enum Resp {
    Ack,
    Value(i64),
}

#[async_trait]
impl Actor for Counter {
    type Message = Msg;
    type Response = Resp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            Msg::Increment(delta) => {
                self.value += delta;
                Ok(Resp::Ack)
            }
            Msg::Get => Ok(Resp::Value(self.value)),
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let handle = Counter::default()
        .spawn_actor("counter", ActorConfig::default())
        .await?;
    handle.notify(Msg::Increment(2)).await?;
    let _ = handle.send(Msg::Increment(2)).await?;
    match handle.send(Msg::Get).await? {
        Resp::Value(count) => println!("counter value: {count}"),
        Resp::Ack => unreachable!("request expects a value"),
    }
    handle.stop(StopReason::Graceful).await?;
    Ok(())
}
