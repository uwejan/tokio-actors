//! An owner actor hands its table to an heir actor when it stops, so the table outlives the owner.

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::timeout;

use tokio_actors::actor::context::ActorContext;
use tokio_actors::actor::{Actor, ActorExt};
use tokio_actors::{ActorHandle, ActorResult, StopReason};

type Table = HashMap<u32, u64>;

/// How long the example waits for any one notice before it gives up.
const BOUND: Duration = Duration::from_secs(20);

pub enum OwnerMsg {
    Put(u32, u64),
    Len,
}

/// Keeps the table in its fields and gives it to `heir` in `on_stopped`.
struct Owner {
    table: Table,
    heir: ActorHandle<Heir>,
}

impl Actor for Owner {
    type Message = OwnerMsg;
    type Response = usize;

    async fn handle(&mut self, msg: OwnerMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<usize> {
        if let OwnerMsg::Put(key, value) = msg {
            self.table.insert(key, value);
        }
        Ok(self.table.len())
    }

    // Runs at every stop but a `Kill`; a `Kill` ends the actor without running it,
    // so a table that must survive a kill needs a copy kept elsewhere.
    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let table = std::mem::take(&mut self.table);
        self.heir.notify(HeirMsg::Inherit(table)).await?;
        Ok(())
    }
}

pub enum HeirMsg {
    Inherit(Table),
    Len,
    Get(u32),
}

/// Starts empty and answers from whatever table it was handed.
#[derive(Default)]
struct Heir {
    table: Table,
}

impl Actor for Heir {
    type Message = HeirMsg;
    type Response = Option<u64>;

    async fn handle(
        &mut self,
        msg: HeirMsg,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Option<u64>> {
        Ok(match msg {
            HeirMsg::Inherit(table) => {
                self.table = table;
                None
            }
            HeirMsg::Len => Some(self.table.len() as u64),
            HeirMsg::Get(key) => self.table.get(&key).copied(),
        })
    }
}

/// What the heir answered after the owner stopped.
pub struct Outcome {
    pub inherited: usize,
    pub value: Option<u64>,
}

pub async fn run() -> Result<Outcome, Box<dyn std::error::Error>> {
    let heir = Heir::default().spawn().await?;
    let owner = Owner {
        table: Table::new(),
        heir: heir.clone(),
    }
    .spawn()
    .await?;

    owner.send(OwnerMsg::Put(1, 10)).await?;
    owner.send(OwnerMsg::Put(2, 20)).await?;
    println!("owner holds {} entries", owner.send(OwnerMsg::Len).await?);

    owner.stop(StopReason::Graceful).await?;
    timeout(BOUND, owner.wait_stopped()).await?;
    // `on_stopped` has returned, so the heir's `Inherit` is already queued.

    let inherited = heir.send(HeirMsg::Len).await?.unwrap_or(0) as usize;
    let value = heir.send(HeirMsg::Get(2)).await?;
    println!("heir inherited {inherited} entries");
    println!("key 2 -> {value:?}");
    Ok(Outcome { inherited, value })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run().await?;
    Ok(())
}
