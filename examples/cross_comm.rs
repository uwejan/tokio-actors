use async_trait::async_trait;
use tokio_actors::{
    actor::{context::ActorContext, handle::ActorHandle, Actor, ActorExt},
    ActorConfig, ActorError, ActorResult, StopReason,
};

#[derive(Default)]
struct Responder {
    received: Vec<String>,
}

#[derive(Clone)]
enum ResponderMsg {
    Deliver(String),
    GetCount,
}

enum ResponderResp {
    Ack,
    Count(usize),
}

#[async_trait]
impl Actor for Responder {
    type Message = ResponderMsg;
    type Response = ResponderResp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            ResponderMsg::Deliver(payload) => {
                self.received.push(payload);
                Ok(ResponderResp::Ack)
            }
            ResponderMsg::GetCount => Ok(ResponderResp::Count(self.received.len())),
        }
    }
}

struct Sender {
    target: ActorHandle<Responder>,
}

impl Sender {
    fn new(target: ActorHandle<Responder>) -> Self {
        Self { target }
    }
}

#[derive(Clone)]
enum SenderMsg {
    Forward(String),
}

enum SenderResp {
    Ack,
}

#[async_trait]
impl Actor for Sender {
    type Message = SenderMsg;
    type Response = SenderResp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            SenderMsg::Forward(payload) => {
                self.target
                    .send(ResponderMsg::Deliver(payload))
                    .await
                    .map_err(|err| ActorError::user(err.to_string()))?;
                Ok(SenderResp::Ack)
            }
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let actor_config = ActorConfig::default();

    let responder = Responder::default()
        .spawn_actor("responder", &actor_config)
        .await?;
    let sender = Sender::new(responder.clone())
        .spawn_actor("sender", &actor_config)
        .await?;

    sender.send(SenderMsg::Forward("hello".into())).await?;
    if let ResponderResp::Count(count) = responder.send(ResponderMsg::GetCount).await? {
        println!("responder received {count} messages");
    }

    sender.stop(StopReason::Graceful).await?;
    responder.stop(StopReason::Graceful).await?;
    Ok(())
}
