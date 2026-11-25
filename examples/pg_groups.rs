//! A process group with a group monitor: the monitor returns the current
//! members, then reports the one that leaves.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::time::timeout;
use tokio_actors::actor::{context::ActorContext, Actor, ActorExt};
use tokio_actors::{ActorResult, ActorSystem, GroupChange, StopReason};

/// How long the example waits for any one notice before it gives up.
const BOUND: Duration = Duration::from_secs(20);

struct Worker;

impl Actor for Worker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

enum Msg {
    Begin,
    Change(GroupChange),
}

struct Watcher {
    system: Arc<ActorSystem>,
    notices: UnboundedSender<String>,
}

impl Actor for Watcher {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Begin => {
                let scope = self.system.pg();
                let (_, members) = ctx
                    .monitor_group(&scope, "workers", Msg::Change)
                    .expect("the watcher is running");
                let _ = self
                    .notices
                    .send(format!("members at start: {}", members.len()));
            }
            Msg::Change(GroupChange::Leave { group, members, .. }) => {
                let _ = self.notices.send(format!("{} left {group}", members.len()));
            }
            Msg::Change(other) => {
                let _ = self.notices.send(format!("unexpected change: {other:?}"));
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let system = ActorSystem::create("pg-groups-example")?;
    let scope = system.pg();
    let first = Worker.spawn().on_system(&system).await?;
    let second = Worker.spawn().on_system(&system).await?;
    scope.join("workers", &first)?;
    scope.join("workers", &second)?;

    let (notices, mut seen) = unbounded_channel();
    let watcher = Watcher {
        system: system.clone(),
        notices,
    }
    .spawn()
    .on_system(&system)
    .await?;
    watcher.send(Msg::Begin).await?;
    println!(
        "{}",
        timeout(BOUND, seen.recv())
            .await?
            .expect("the watcher answers")
    );

    // A member that ends leaves its groups before its end is published.
    first.stop(StopReason::Graceful).await?;
    println!(
        "{}",
        timeout(BOUND, seen.recv())
            .await?
            .expect("the watcher is told")
    );
    println!(
        "members after: {}",
        scope.members::<Worker>("workers").len()
    );
    Ok(())
}
