use async_trait::async_trait;
use tokio_actors::{
    actor::{context::ActorContext, handle::ActorHandle, Actor, ActorExt},
    ActorConfig, ActorError, ActorResult, StopReason,
};

#[derive(Default)]
struct Responder {
    received: usize,
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
                let _ = payload;
                self.received += 1;
                Ok(ResponderResp::Ack)
            }
            ResponderMsg::GetCount => Ok(ResponderResp::Count(self.received)),
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
    Relay(String),
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
            SenderMsg::Relay(payload) => {
                self.target
                    .send(ResponderMsg::Deliver(payload))
                    .await
                    .map_err(|err| ActorError::user(err.to_string()))?;
                Ok(SenderResp::Ack)
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actors_can_forward_messages_between_each_other() {
    let responder = Responder::default()
        .spawn_actor("responder", ActorConfig::default())
        .await
        .unwrap();
    let sender = Sender::new(responder.clone())
        .spawn_actor("sender", ActorConfig::default())
        .await
        .unwrap();

    sender
        .send(SenderMsg::Relay("hello world".into()))
        .await
        .unwrap();

    let count = match responder.send(ResponderMsg::GetCount).await.unwrap() {
        ResponderResp::Count(c) => c,
        ResponderResp::Ack => unreachable!(),
    };
    assert_eq!(count, 1);
    println!("cross-comm test count={count}");

    sender.stop(StopReason::Graceful).await.unwrap();
    responder.stop(StopReason::Graceful).await.unwrap();
}
