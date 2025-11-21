use async_trait::async_trait;
use tokio_actors::{
    actor::{context::ActorContext, handle::ActorHandle, Actor, ActorExt},
    ActorResult,
};

/// Simple Ping-Pong example demonstrating bidirectional actor communication.
///
/// This example shows:
/// - Two actors communicating with each other
/// - Holding actor handles in state
/// - Type-safe message passing

// Pong actor - simply responds to pings
#[derive(Default)]
struct PongActor {
    pings_received: u64,
}

#[derive(Clone)]
enum PongMsg {
    Ping, // ← No manual reply_to needed!
    GetCount,
}

#[derive(Clone)]
enum PongResp {
    Pong, // ← Response goes through send() automatically
    Count(u64),
}

#[async_trait]
impl Actor for PongActor {
    type Message = PongMsg;
    type Response = PongResp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            PongMsg::Ping => {
                self.pings_received += 1;
                Ok(PongResp::Pong) // ← Just return the response!
            }
            PongMsg::GetCount => Ok(PongResp::Count(self.pings_received)),
        }
    }
}

// Ping actor
struct PingActor {
    pong: ActorHandle<PongActor>,
    pongs_received: u64,
}

#[derive(Clone)]
enum PingMsg {
    SendPing,
    GetCount,
}

#[derive(Clone)]
enum PingResp {
    Ack,
    Count(u64),
}

#[async_trait]
impl Actor for PingActor {
    type Message = PingMsg;
    type Response = PingResp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            PingMsg::SendPing => {
                // Send Ping, get Pong back automatically through send()
                let resp = self
                    .pong
                    .send(PongMsg::Ping)
                    .await
                    .map_err(|e| tokio_actors::ActorError::user(format!("{}", e)))?;

                if matches!(resp, PongResp::Pong) {
                    self.pongs_received += 1;
                }
                Ok(PingResp::Ack)
            }
            PingMsg::GetCount => Ok(PingResp::Count(self.pongs_received)),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Spawn pong first
    let pong = PongActor::default().spawn_actor("pong", ()).await?;

    // Spawn ping with reference to pong
    let ping = PingActor {
        pong: pong.clone(),
        pongs_received: 0,
    }
    .spawn_actor("ping", ())
    .await?;

    // Send 10 pings
    for _ in 0..10 {
        ping.send(PingMsg::SendPing).await?;
    }

    // Check counts
    if let PingResp::Count(count) = ping.send(PingMsg::GetCount).await? {
        println!("✅ Ping actor received {} pongs", count);
    }

    if let PongResp::Count(count) = pong.send(PongMsg::GetCount).await? {
        println!("✅ Pong actor received {} pings", count);
    }

    Ok(())
}
