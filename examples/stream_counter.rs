use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason, StreamEvent,
};
use tokio_stream::wrappers::ReceiverStream;

struct Summer {
    rx: Option<tokio::sync::mpsc::Receiver<i32>>,
    total: i64,
}

enum Msg {
    Stream(StreamEvent<i32>),
    GetTotal,
}

impl From<StreamEvent<i32>> for Msg {
    fn from(ev: StreamEvent<i32>) -> Self {
        Msg::Stream(ev)
    }
}

enum Resp {
    Ack,
    Total(i64),
}

impl Actor for Summer {
    type Message = Msg;
    type Response = Resp;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(rx) = self.rx.take() {
            ctx.add_stream(ReceiverStream::new(rx));
        }
        Ok(())
    }

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        Ok(match msg {
            Msg::Stream(StreamEvent::Data(n)) => {
                self.total += n as i64;
                println!("  received {n}, running total: {}", self.total);
                Resp::Ack
            }
            Msg::Stream(StreamEvent::Finished) => {
                println!("  stream finished");
                Resp::Ack
            }
            Msg::GetTotal => Resp::Total(self.total),
        })
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);

    let actor = Summer {
        rx: Some(rx),
        total: 0,
    }
    .spawn_actor("summer", ActorConfig::default())
    .await?;

    // Feed numbers through the channel
    for n in 1..=5 {
        tx.send(n).await?;
    }
    drop(tx); // close channel → stream sends Finished

    // Wait for all items to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    match actor.send(Msg::GetTotal).await? {
        Resp::Total(total) => println!("final total: {total}"),
        Resp::Ack => unreachable!(),
    }

    actor.stop(StopReason::Graceful).await?;
    Ok(())
}
