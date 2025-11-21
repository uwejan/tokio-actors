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
enum CounterMsg {
    Increment(i64),
    Get,
}

enum CounterResp {
    Ack,
    Value(i64),
}

#[async_trait]
impl Actor for Counter {
    type Message = CounterMsg;
    type Response = CounterResp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            CounterMsg::Increment(delta) => {
                self.value += delta;
                Ok(CounterResp::Ack)
            }
            CounterMsg::Get => Ok(CounterResp::Value(self.value)),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simple_counter_handles_messages() {
    let handle = Counter::default()
        .spawn_actor("counter", ActorConfig::default())
        .await
        .unwrap();

    handle.notify(CounterMsg::Increment(5)).await.unwrap();
    let _ = handle.send(CounterMsg::Increment(5)).await.unwrap();
    let value = match handle.send(CounterMsg::Get).await.unwrap() {
        CounterResp::Value(v) => v,
        CounterResp::Ack => 0,
    };
    assert_eq!(value, 10);
    println!("counter test final value={value}");

    handle.stop(StopReason::Graceful).await.unwrap();
}
