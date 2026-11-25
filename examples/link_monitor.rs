//! Links, trapped exits and monitors: a watcher that traps exits reads a
//! `LinkExit`, a linked worker that does not trap ends with the same reason,
//! and a monitor delivers a `Down`.

use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::time::timeout;
use tokio_actors::actor::{context::ActorContext, Actor, ActorExt};
use tokio_actors::{ActorError, ActorHandle, ActorResult, Down, LinkExit, StopReason};

/// How long the example waits for any one notice before it gives up.
const BOUND: Duration = Duration::from_secs(20);

enum Msg {
    /// Trap exits, link to the first worker and monitor the second.
    Watch(ActorHandle<Node>, ActorHandle<Node>),
    /// Link to a worker without trapping.
    LinkTo(ActorHandle<Node>),
    Fail,
    Exit(LinkExit),
    Down(Down),
}

fn refused(error: impl std::fmt::Display) -> ActorError {
    ActorError::user(error.to_string())
}

struct Node {
    notices: UnboundedSender<String>,
}

impl Actor for Node {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Watch(linked, monitored) => {
                ctx.trap_exit(Msg::Exit);
                ctx.link(&linked).map_err(refused)?;
                ctx.monitor(&monitored, Msg::Down).map_err(refused)?;
            }
            Msg::LinkTo(peer) => ctx.link(&peer).map_err(refused)?,
            Msg::Fail => return Err(ActorError::user("boom")),
            Msg::Exit(exit) => {
                let _ = self.notices.send(format!("exit {:?}", exit.reason));
            }
            Msg::Down(down) => {
                let _ = self.notices.send(format!("down {:?}", down.reason));
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (notices, mut seen) = unbounded_channel();
    let spawn = || Node {
        notices: notices.clone(),
    };
    let (a, b, c) = (
        spawn().spawn().await?,
        spawn().spawn().await?,
        spawn().spawn().await?,
    );
    let watcher = spawn().spawn().await?;

    // The acknowledgement of `send` means the links and the monitor exist.
    watcher.send(Msg::Watch(a.clone(), b.clone())).await?;
    c.send(Msg::LinkTo(a.clone())).await?;

    a.notify(Msg::Fail).await?;
    let exit = timeout(BOUND, seen.recv()).await?;
    println!("{}", exit.expect("the watcher is told"));
    println!(
        "worker c ended: {}",
        timeout(BOUND, c.wait_stopped()).await?
    );

    b.stop(StopReason::Graceful).await?;
    let down = timeout(BOUND, seen.recv()).await?;
    println!("{}", down.expect("the watcher is told"));
    Ok(())
}
