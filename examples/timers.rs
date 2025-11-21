use async_trait::async_trait;
use tokio::time::Duration;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, MissPolicy, StopReason,
};

#[derive(Default)]
struct Heartbeat {
    ticks: u64,
}

#[derive(Clone)]
enum Msg {
    Tick,
    Get,
}

enum Resp {
    Ack,
    Count(u64),
}

#[async_trait]
impl Actor for Heartbeat {
    type Message = Msg;
    type Response = Resp;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.schedule_recurring(Msg::Tick, Duration::from_millis(200), MissPolicy::Skip)?;
        Ok(())
    }

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        Ok(match msg {
            Msg::Tick => {
                self.ticks += 1;
                Resp::Ack
            }
            Msg::Get => Resp::Count(self.ticks),
        })
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let actor = Heartbeat::default()
        .spawn_actor("heartbeat", ActorConfig::default())
        .await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    match actor.send(Msg::Get).await? {
        Resp::Count(count) => println!("heartbeat ticks: {count}"),
        Resp::Ack => unreachable!(),
    }
    actor.stop(StopReason::Graceful).await?;
    Ok(())
}
