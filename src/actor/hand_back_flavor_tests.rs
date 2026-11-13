//! Which status requesters the run loop hands the thread back for. A
//! requester that is a runtime task is handed back for on any runtime. A
//! requester that is not a task (a body driven by `block_on`) is handed back
//! for on a current-thread runtime, where it runs on the actor's own thread,
//! and not on a multi-thread runtime, where it is woken from another thread.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::runtime::{hand_backs, ActorConfig};
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorResult, AskError};
use crate::types::{ActorStatusInfo, StopReason};

const BACKLOG: usize = 300;

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

struct Rig {
    handle: ActorHandle<Counter>,
    handled: Arc<AtomicUsize>,
    release: Arc<Notify>,
}

/// An actor held in a handler with nothing queued behind it.
async fn held_actor() -> Rig {
    let handled = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let handle = Counter {
        handled: handled.clone(),
        entered: entered.clone(),
        release: release.clone(),
    }
    .spawn()
    .with_config(ActorConfig::default().with_mailbox_capacity(BACKLOG + 8))
    .await
    .unwrap();
    handle.notify(Msg::Block).await.unwrap();
    entered.notified().await;
    Rig {
        handle,
        handled,
        release,
    }
}

/// Queues `BACKLOG` messages behind the held handler.
async fn queue_backlog(rig: &Rig) {
    for _ in 0..BACKLOG {
        rig.handle.notify(Msg::Add).await.unwrap();
    }
}

/// An actor held in a handler with `BACKLOG` messages queued behind it.
async fn rig() -> Rig {
    let rig = held_actor().await;
    queue_backlog(&rig).await;
    rig
}

type Query = Pin<Box<dyn Future<Output = Result<ActorStatusInfo, AskError>> + Send>>;

/// A status query polled once from the calling body, which is not a task, so
/// it is queued on the actor's system channel with the body's own waker.
async fn body_query(handle: &ActorHandle<Counter>) -> Query {
    let handle = handle.clone();
    let mut query: Query = Box::pin(async move { handle.get_status().await });
    std::future::poll_fn(|cx| {
        assert!(query.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    query
}

/// Ends the actor after the whole backlog ran; the hand-back count is final.
async fn drain_and_stop(rig: &Rig) -> u32 {
    rig.handle.send(Msg::Add).await.unwrap();
    rig.handle.stop(StopReason::Graceful).await.unwrap();
    rig.handle.wait_stopped().await;
    hand_backs::read(rig.handle.id())
}

/// A request answered on its own turn, followed by a backlog of messages that
/// answer nobody, makes the loop hand the thread back to the requester. The
/// requester is a body driven by `block_on`, polled once so its request is
/// queued ahead of the backlog.
#[tokio::test(flavor = "current_thread")]
async fn a_lone_reply_before_a_backlog_is_followed_by_a_hand_back() {
    let rig = held_actor().await;
    let handle = rig.handle.clone();
    let mut request: Pin<Box<dyn Future<Output = Result<(), AskError>> + Send>> =
        Box::pin(async move { handle.send(Msg::Add).await });
    std::future::poll_fn(|cx| {
        assert!(request.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(rig.handle.mailbox_len(), 1, "the request is queued");
    queue_backlog(&rig).await;
    rig.release.notify_one();
    request.await.unwrap();
    assert!(
        drain_and_stop(&rig).await >= 1,
        "the loop handed the thread back after the reply stood alone"
    );
}
