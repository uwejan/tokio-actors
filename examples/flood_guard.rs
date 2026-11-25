//! A guard actor that polls the system snapshot and kills an actor whose mailbox stays long across two looks.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::time::timeout;
use tokio_actors::actor::context::ActorContext;
use tokio_actors::actor::{Actor, ActorExt};
use tokio_actors::{ActorId, ActorResult, ActorSystem, StopReason};

/// How long the example waits for any one notice before it gives up.
const BOUND: Duration = Duration::from_secs(20);
/// The guard's look interval: an actor long at one look and at the next is killed.
const LOOK: Duration = Duration::from_millis(100);
/// A mailbox at or over this length is long.
const LONG: usize = 3;

#[derive(Clone)]
pub struct Tick;

/// Looks at every actor's mailbox length on each tick.
struct Guard {
    system: Arc<ActorSystem>,
    long: HashSet<ActorId>,
    killed: mpsc::UnboundedSender<ActorId>,
}

impl Actor for Guard {
    type Message = Tick;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.schedule(Tick).every(LOOK).await?;
        Ok(())
    }

    async fn handle(&mut self, _tick: Tick, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let mut still_long = HashSet::new();
        for record in self.system.snapshot() {
            let id = record.actor.id();
            if id == ctx.actor_id() || record.mailbox_len < LONG {
                continue;
            }
            if self.long.contains(id) {
                if self.system.kill_by_id(id).await.is_some() {
                    let _ = self.killed.send(id.clone());
                }
            } else {
                println!("mailbox long: {id}");
                still_long.insert(id.clone());
            }
        }
        // An id that was not long at this look is forgotten.
        self.long = still_long;
        Ok(())
    }
}

/// Never drains: its turn waits on a semaphore that has no permits.
struct Flooded {
    gate: Arc<Semaphore>,
}

impl Actor for Flooded {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.gate.acquire().await.map(|permit| permit.forget()).ok();
        Ok(())
    }
}

/// The flooded actor's id, the actors the guard killed and the reason the flooded actor ended with.
pub struct Outcome {
    pub flooded: ActorId,
    pub killed: Vec<ActorId>,
    pub reason: StopReason,
}

pub async fn run() -> Result<Outcome, Box<dyn std::error::Error>> {
    let system = ActorSystem::create("flood-guard")?;
    let (killed_tx, mut killed_rx) = mpsc::unbounded_channel();
    let _guard = Guard {
        system: system.clone(),
        long: HashSet::new(),
        killed: killed_tx,
    }
    .spawn()
    .on_system(&system)
    .await?;

    let flooded = Flooded {
        gate: Arc::new(Semaphore::new(0)),
    }
    .spawn()
    .on_system(&system)
    .await?;
    for _ in 0..10 {
        flooded.notify(()).await?;
    }

    let reason = timeout(BOUND, flooded.wait_stopped()).await?;
    let mut killed = Vec::new();
    if let Some(id) = timeout(BOUND, killed_rx.recv()).await? {
        println!("stopped {id}: {reason:?}");
        killed.push(id);
    }
    Ok(Outcome {
        flooded: flooded.id().clone(),
        killed,
        reason,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run().await?;
    Ok(())
}
