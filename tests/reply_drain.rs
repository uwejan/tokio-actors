//! A requester that a reply wakes is not left behind a long backlog: after
//! answering a status query the run loop runs a few more turns and then
//! hands the thread back once, so the requester runs while most of the
//! backlog is still queued.
//!
//! Every ordering below is built on a gate: a `Block` message whose handler
//! signals "entered" and then awaits "release", so the backlog is queued
//! while the actor is known to be between two `handle` invocations. No sleep
//! is used as synchronization.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{oneshot, Notify};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason,
};

enum Msg {
    /// Signals `entered`, then waits for `release`.
    Block,
    /// Counts one handled message.
    Add,
}

struct Counter {
    handled: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for Counter {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Block => {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Msg::Add => {
                self.handled.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }
}

/// How many messages the actor handled between answering the first status
/// query behind a backlog of `backlog` casts and the requester resuming.
async fn handled_when_the_requester_resumes(backlog: usize) -> usize {
    let handled = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let handle = Counter {
        handled: handled.clone(),
        entered: entered.clone(),
        release: release.clone(),
    }
    .spawn()
    .with_config(ActorConfig::default().with_mailbox_capacity(backlog + 8))
    .await
    .unwrap();

    handle.notify(Msg::Block).await.unwrap();
    entered.notified().await;
    for _ in 0..backlog {
        handle.notify(Msg::Add).await.unwrap();
    }

    let (seen_tx, seen_rx) = oneshot::channel();
    let requester = {
        let handle = handle.clone();
        let handled = handled.clone();
        tokio::spawn(async move {
            let info = handle.get_status().await.unwrap();
            let _ = seen_tx.send((handled.load(Ordering::SeqCst), info.mailbox_len));
        })
    };
    // The requester pushes its query on its first poll and waits for the reply.
    tokio::task::yield_now().await;

    release.notify_one();
    let (seen, len_at_reply) = seen_rx.await.unwrap();
    requester.await.unwrap();
    assert!(
        len_at_reply >= backlog - 1,
        "the query is answered ahead of the backlog (mailbox_len {len_at_reply} of {backlog})"
    );

    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;
    seen
}

#[tokio::test(flavor = "current_thread")]
async fn a_requester_resumes_before_the_backlog_drains() {
    let backlog = 300;
    let seen = handled_when_the_requester_resumes(backlog).await;
    assert!(
        seen <= 8,
        "the requester resumed after {seen} of {backlog} queued messages were handled"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_whole_backlog_is_still_handled() {
    let handled = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let handle = Counter {
        handled: handled.clone(),
        entered: entered.clone(),
        release: release.clone(),
    }
    .spawn()
    .with_config(ActorConfig::default().with_mailbox_capacity(512))
    .await
    .unwrap();

    handle.notify(Msg::Block).await.unwrap();
    entered.notified().await;
    for _ in 0..300 {
        handle.notify(Msg::Add).await.unwrap();
    }
    release.notify_one();
    // Queries all through the drain: each is answered, and none loses a message.
    for _ in 0..50 {
        handle.get_status().await.unwrap();
    }
    // A call is handled after everything queued before it.
    handle.send(Msg::Add).await.unwrap();
    assert_eq!(handled.load(Ordering::SeqCst), 301);
    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;
}
