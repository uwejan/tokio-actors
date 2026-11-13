//! The request table and the state cell its entries share, in every order the
//! task, the asker and the stop-time stage can touch them.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::monitor::MonitorShared;
use super::request::RequestTable;
use crate::actor::Actor;
use crate::error::ActorResult;
use crate::types::MonitorId;

struct Probe;

impl Actor for Probe {
    type Message = u32;
    type Response = ();

    async fn handle(
        &mut self,
        _: u32,
        _ctx: &mut super::context::ActorContext<Self>,
    ) -> ActorResult<()> {
        Ok(())
    }
}

const PENDING: u8 = 0;
const SENT: u8 = 1;
const CANCELLED: u8 = 2;
const FLUSHED: u8 = 3;
const CLOSED: u8 = 4;
const SPENT: u8 = 5;

/// What the task, the asker and the stage do to one request.
#[derive(Debug, Clone, Copy)]
enum Op {
    /// The task takes the reply.
    Take,
    /// The reply did not land: the task puts it back.
    Restore,
    /// The task's map panicked.
    Spend,
    /// The asker cancels the request.
    Cancel,
    /// The stage closes every request nobody has taken.
    Close,
}

const OPS: [Op; 5] = [Op::Take, Op::Restore, Op::Spend, Op::Cancel, Op::Close];

fn table_of(count: usize) -> (RequestTable<Probe>, Vec<(MonitorId, Arc<MonitorShared>)>) {
    let mut table = RequestTable::<Probe>::default();
    let mut made = Vec::new();
    for _ in 0..count {
        let id = MonitorId::next();
        let shared = MonitorShared::for_request(id);
        table.insert(id, shared.clone(), CancellationToken::new());
        made.push((id, shared));
    }
    (table, made)
}

fn orders(length: usize, prefix: &mut Vec<Op>, all: &mut Vec<Vec<Op>>) {
    if !prefix.is_empty() {
        all.push(prefix.clone());
    }
    if prefix.len() == length {
        return;
    }
    for op in OPS {
        prefix.push(op);
        orders(length, prefix, all);
        prefix.pop();
    }
}
