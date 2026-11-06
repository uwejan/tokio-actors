//! The status plane's requests are answered in the order they were made,
//! however many are queued at once, and a query made after the actor ended
//! is refused.
//!
//! A gate message holds the actor between two `handle` invocations while the
//! requests queue, so the order is built, not raced. No sleep is used as
//! synchronization.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, AskError, StopReason,
};

enum Msg {
    /// Signals `entered`, then waits for `release`.
    Block,
}

struct Gate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for Gate {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, _msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

async fn gated() -> (tokio_actors::ActorHandle<Gate>, Arc<Notify>) {
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
    (handle, release)
}

#[tokio::test(flavor = "current_thread")]
async fn queued_status_queries_are_answered_in_the_order_they_were_made() {
    let (handle, release) = gated().await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    for i in 0..5usize {
        let handle = handle.clone();
        let order = order.clone();
        tasks.push(tokio::spawn(async move {
            handle.get_status().await.unwrap();
            order.lock().unwrap().push(i);
        }));
        // The requester pushes its query on its first poll.
        tokio::task::yield_now().await;
    }
    release.notify_one();
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3, 4]);
    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_queries_than_the_plane_holds_are_all_answered() {
    let (handle, release) = gated().await;
    let mut tasks = Vec::new();
    for _ in 0..200 {
        let handle = handle.clone();
        tasks.push(tokio::spawn(
            async move { handle.get_status().await.is_ok() },
        ));
    }
    tokio::task::yield_now().await;
    release.notify_one();
    for t in tasks {
        let ok = tokio::time::timeout(Duration::from_secs(10), t)
            .await
            .expect("a queued query is answered")
            .unwrap();
        assert!(ok);
    }
    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_query_after_the_end_is_refused() {
    let (handle, release) = gated().await;
    release.notify_one();
    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;
    assert!(matches!(handle.get_status().await, Err(AskError::Closed)));
}

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_query_to_an_idle_actor_is_answered_on_one_thread() {
    let handle = Idle.spawn().await.unwrap();
    // Let the actor reach its wait first.
    tokio::task::yield_now().await;
    let info = tokio::time::timeout(Duration::from_secs(5), handle.get_status())
        .await
        .expect("an idle actor answers a status query")
        .unwrap();
    assert_eq!(info.mailbox_len, 0);
    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_query_to_an_idle_actor_is_answered_on_workers() {
    let handle = Idle.spawn().await.unwrap();
    tokio::task::yield_now().await;
    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(5), handle.get_status())
            .await
            .expect("an idle actor answers a status query")
            .unwrap();
    }
    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;
}
