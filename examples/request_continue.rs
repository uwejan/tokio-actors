//! Requests answered as messages, then a continuation: an asker sends three
//! requests without waiting, handles each reply as its own message and, on
//! the last one, sets a continuation that prints the squares and their sum
//! right after that turn.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::time::timeout;
use tokio_actors::actor::{context::ActorContext, Actor, ActorExt};
use tokio_actors::{ActorHandle, ActorResult, RequestId, RequestReply};

/// How long the example waits for any one notice before it gives up.
const BOUND: Duration = Duration::from_secs(20);

struct Squarer;

impl Actor for Squarer {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, n: u32, _: &mut ActorContext<Self>) -> ActorResult<u32> {
        Ok(n * n)
    }
}

enum Msg {
    Ask(ActorHandle<Squarer>, Vec<u32>),
    Answer(RequestReply<u32>),
    Summarize,
}

struct Asker {
    /// The value each request in flight asked about.
    asked: HashMap<RequestId, u32>,
    /// Square by value, filled as the replies arrive in any order.
    answers: BTreeMap<u32, u32>,
    lines: UnboundedSender<String>,
}

impl Actor for Asker {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Ask(squarer, values) => {
                for value in values {
                    let id = ctx
                        .request(&squarer, value, Msg::Answer)
                        .expect("the asker is running");
                    self.asked.insert(id, value);
                }
            }
            Msg::Answer(reply) => {
                let square = reply.result.expect("the squarer answers");
                let value = self.asked.remove(&reply.id).expect("an asked request");
                self.answers.insert(value, square);
                if self.asked.is_empty() {
                    ctx.continue_with(Msg::Summarize);
                }
            }
            Msg::Summarize => {
                for (value, square) in &self.answers {
                    let _ = self.lines.send(format!("{value} squared is {square}"));
                }
                let sum: u32 = self.answers.values().sum();
                let _ = self.lines.send(format!("sum {sum}"));
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (lines, mut seen) = unbounded_channel();
    let squarer = Squarer.spawn().await?;
    let asker = Asker {
        asked: HashMap::new(),
        answers: BTreeMap::new(),
        lines,
    }
    .spawn()
    .await?;

    asker.notify(Msg::Ask(squarer, vec![2, 3, 4])).await?;
    for _ in 0..4 {
        println!(
            "{}",
            timeout(BOUND, seen.recv())
                .await?
                .expect("the asker reports")
        );
    }
    Ok(())
}
