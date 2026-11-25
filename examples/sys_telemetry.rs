//! Debug statistics and a telemetry handler: an actor spawned with statistics
//! on handles three messages and `sys()` reads the counts; a handler attached
//! for `ActorEnded` reports the actor's end on a channel.

use std::time::Duration;

use tokio::sync::mpsc::unbounded_channel;
use tokio::time::timeout;
use tokio_actors::actor::{context::ActorContext, Actor, ActorExt};
use tokio_actors::sys::DebugOptions;
use tokio_actors::telemetry::{EventKind, KindSet, TelemetryEvent};
use tokio_actors::{ActorResult, ActorSystem, StopReason};

/// How long the example waits for any one notice before it gives up.
const BOUND: Duration = Duration::from_secs(20);

struct Counter(u32);

impl Actor for Counter {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, add: u32, _: &mut ActorContext<Self>) -> ActorResult<u32> {
        self.0 += add;
        Ok(self.0)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let system = ActorSystem::create("sys-telemetry-example")?;

    // A handler is a plain closure that must not block: it hands the end to a
    // channel and `main` reads it.
    let (ends, mut ended) = unbounded_channel();
    system.attach_telemetry(
        "ends",
        KindSet::of(&[EventKind::ActorEnded]),
        move |event| {
            if let TelemetryEvent::ActorEnded { reason, .. } = event {
                let _ = ends.send(reason.clone());
            }
        },
    );

    let counter = Counter(0)
        .spawn()
        .on_system(&system)
        .debug(DebugOptions::default().statistics(true))
        .await?;
    for add in [1, 2, 3] {
        counter.send(add).await?;
    }

    let stats = counter
        .sys()
        .statistics()
        .await?
        .expect("statistics were turned on at spawn");
    println!(
        "messages in {}, replies out {}, turns {}",
        stats.messages_in, stats.messages_out, stats.turns
    );

    counter.stop(StopReason::Graceful).await?;
    let reason = timeout(BOUND, ended.recv())
        .await?
        .expect("the handler reports the end");
    println!("ended: {reason}");
    Ok(())
}
