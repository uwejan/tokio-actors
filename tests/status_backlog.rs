//! A status query is answered ahead of the mailbox backlog: the run loop
//! takes it at the first turn boundary after the handler it was queued
//! behind, so the mailbox still holds the whole backlog when it is answered.
//!
//! The order is built on a gate, not on time: a `Block` message whose handler
//! signals "entered" and then awaits "release", so the backlog is queued
//! while the actor is known to be between two `handle` invocations. A query
//! is queued by polling its future once, which queues the request before it
//! waits for the reply.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Waker};

use tokio::sync::Notify;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, StopReason,
};

const BACKLOG: usize = 40;

enum Msg {
    /// Signals `entered`, then waits for `release`.
    Block,
    /// Does nothing.
    Add,
}

struct Gate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for Gate {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Msg::Block = msg {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}

/// The mailbox length each of `queries` queries reads when it is answered,
/// queued behind a handler that is running and a backlog of `BACKLOG`.
async fn lengths_at_serve(queries: usize) -> Vec<usize> {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let handle = Gate {
        entered: entered.clone(),
        release: release.clone(),
    }
    .spawn()
    .await
    .unwrap();

    handle.notify(Msg::Block).await.unwrap();
    entered.notified().await;
    for _ in 0..BACKLOG {
        handle.notify(Msg::Add).await.unwrap();
    }

    let mut pending = Vec::new();
    for _ in 0..queries {
        let handle = handle.clone();
        let mut query = Box::pin(async move { handle.get_status().await });
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            query.as_mut().poll(&mut cx).is_pending(),
            "the query waits for its reply"
        );
        pending.push(query);
    }

    release.notify_one();
    let mut lengths = Vec::new();
    for query in pending {
        lengths.push(query.await.unwrap().mailbox_len);
    }

    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;
    lengths
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_queries_are_answered_ahead_of_the_backlog_multi_thread() {
    assert_eq!(lengths_at_serve(2).await, vec![BACKLOG, BACKLOG]);
}
