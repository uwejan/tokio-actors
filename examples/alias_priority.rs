//! A priority alias with the priority gate: while the owner's turn is held, a
//! plain message, a handle's priority message and an alias's priority message
//! arrive. The gate sends the handle's on the ordinary mailbox, so only the
//! alias's reaches the priority lane and is handled first.

use tokio::sync::oneshot;
use tokio_actors::actor::{context::ActorContext, Actor, ActorExt};
use tokio_actors::{ActorConfig, ActorResult, Alias, AliasOptions};

enum Msg {
    /// Makes a priority alias and hands it to the caller.
    Mint(oneshot::Sender<Alias<Owner>>),
    /// Holds the turn: tells `entered`, then waits for `release`.
    Hold(oneshot::Sender<()>, oneshot::Receiver<()>),
    Note(&'static str),
    /// Answers the notes in the order they were handled.
    Order,
}

#[derive(Default)]
struct Owner {
    handled: Vec<&'static str>,
}

impl Actor for Owner {
    type Message = Msg;
    type Response = Vec<&'static str>;

    async fn handle(
        &mut self,
        msg: Msg,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Vec<&'static str>> {
        match msg {
            Msg::Mint(reply) => {
                let options = AliasOptions {
                    priority: true,
                    ..AliasOptions::default()
                };
                let _ = reply.send(ctx.alias(options));
            }
            Msg::Hold(entered, release) => {
                let _ = entered.send(());
                let _ = release.await;
            }
            Msg::Note(text) => self.handled.push(text),
            Msg::Order => return Ok(self.handled.clone()),
        }
        Ok(Vec::new())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ActorConfig::default()
        .with_priority_lane(8)
        .with_priority_by_alias();
    let owner = Owner::default().spawn().with_config(config).await?;

    let (minted, alias) = oneshot::channel();
    owner.notify(Msg::Mint(minted)).await?;
    let alias = alias.await?;

    let (entered, held) = oneshot::channel();
    let (release, released) = oneshot::channel();
    owner.notify(Msg::Hold(entered, released)).await?;
    held.await?;

    owner.notify(Msg::Note("plain notify")).await?;
    owner.notify_priority(Msg::Note("handle priority")).await?;
    alias.notify_priority(Msg::Note("alias priority")).await?;
    println!("lane holds {} message", owner.priority_len());

    let _ = release.send(());
    for (place, note) in owner.send(Msg::Order).await?.iter().enumerate() {
        println!("{}: {note}", place + 1);
    }
    Ok(())
}
