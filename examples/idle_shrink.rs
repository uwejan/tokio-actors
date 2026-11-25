//! A buffer actor that gives its memory back when it has been idle, using the idle hook.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_actors::actor::context::ActorContext;
use tokio_actors::actor::{Actor, ActorExt};
use tokio_actors::ActorResult;

/// How long the example waits for any one notice before it gives up.
const BOUND: Duration = Duration::from_secs(20);

pub enum Msg {
    /// Fills the buffer with a batch and empties it again, in one turn.
    Batch(usize),
    Capacity,
}

/// Keeps its capacity across batches until an idle spell replaces the buffer.
struct Buffer {
    items: Vec<u64>,
    shrunk: mpsc::UnboundedSender<()>,
}

impl Actor for Buffer {
    type Message = Msg;
    type Response = usize;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.idle_after(Duration::from_millis(50));
        Ok(())
    }

    // Runs once per idle spell; a status read does not restart the spell.
    async fn on_idle(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.items.is_empty() {
            self.items = Vec::new();
            let _ = self.shrunk.send(());
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<usize> {
        match msg {
            Msg::Batch(n) => {
                self.items.extend(0..n as u64);
                self.items.clear();
            }
            Msg::Capacity => {}
        }
        Ok(self.items.capacity())
    }
}

/// The buffer's capacity as it answered, while busy and after the idle hook.
pub struct Outcome {
    pub busy: usize,
    pub idle: usize,
}

pub async fn run() -> Result<Outcome, Box<dyn std::error::Error>> {
    let (shrunk, mut shrunk_rx) = mpsc::unbounded_channel();
    let buffer = Buffer {
        items: Vec::new(),
        shrunk,
    }
    .spawn()
    .await?;

    // Read in the turn that filled the buffer, before any idle spell.
    let busy = buffer.send(Msg::Batch(1000)).await?;
    println!("capacity while busy: {busy}");

    // The hook reports on a channel, so nothing here sleeps.
    timeout(BOUND, shrunk_rx.recv()).await?;
    let idle = buffer.send(Msg::Capacity).await?;
    println!("capacity after idle: {idle}");
    Ok(Outcome { busy, idle })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run().await?;
    Ok(())
}
