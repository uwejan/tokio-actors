//! A gate actor that stashes jobs while closed, refuses the ones over its cap, and replays the stash in arrival order.

use std::collections::VecDeque;

use tokio_actors::actor::context::ActorContext;
use tokio_actors::actor::{Actor, ActorExt};
use tokio_actors::ActorResult;

/// What a caller sends to the gate.
pub enum Msg {
    Job(u32),
    Open,
}

/// What the gate answers.
pub enum Reply {
    Stashed(usize),
    Refused(u32),
    Handled(Vec<u32>),
}

/// Jobs wait in `stash`, a queue the actor bounds itself, outside the mailbox.
struct Gate {
    open: bool,
    stash: VecDeque<u32>,
    handled: Vec<u32>,
}

impl Gate {
    const CAP: usize = 2;
}

impl Actor for Gate {
    type Message = Msg;
    type Response = Reply;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        Ok(match msg {
            Msg::Job(n) if self.open => {
                self.handled.push(n);
                Reply::Handled(self.handled.clone())
            }
            // A refusal is an ordinary answer: an `Err` would stop the actor.
            Msg::Job(n) if self.stash.len() >= Self::CAP => Reply::Refused(n),
            Msg::Job(n) => {
                self.stash.push_back(n);
                Reply::Stashed(self.stash.len())
            }
            Msg::Open => {
                self.open = true;
                self.handled.extend(self.stash.drain(..));
                Reply::Handled(self.handled.clone())
            }
        })
    }
}

/// What the caller learned from the gate's answers.
pub struct Outcome {
    pub refused: Vec<u32>,
    pub handled: Vec<u32>,
}

pub async fn run() -> Result<Outcome, Box<dyn std::error::Error>> {
    let gate = Gate {
        open: false,
        stash: VecDeque::new(),
        handled: Vec::new(),
    }
    .spawn()
    .await?;

    let mut refused = Vec::new();
    for n in 1..=3 {
        match gate.send(Msg::Job(n)).await? {
            Reply::Stashed(len) => println!("stashed {len}"),
            Reply::Refused(n) => {
                println!("refused {n}");
                refused.push(n);
            }
            Reply::Handled(_) => {}
        }
    }
    gate.send(Msg::Open).await?;
    let handled = match gate.send(Msg::Job(4)).await? {
        Reply::Handled(all) => all,
        _ => Vec::new(),
    };
    println!("handled {handled:?}");
    Ok(Outcome { refused, handled })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run().await?;
    Ok(())
}
