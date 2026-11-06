//! While the run loop has handed the thread back after a lone status reply
//! it waits like a loop in its select: a stop request wakes it at once. A
//! task that wakes itself again and again keeps the worker's run queue
//! non-empty, so the scheduler's deferred wakes are flushed only every
//! `event_interval` polls; a loop that waited for that flush would let the
//! competitor run dozens of times before it saw the stop. The count of the
//! competitor's polls between the stop and the actor's end is the
//! measurement, and no clock is read.
//!
//! The backlog is queued behind a gate (a `Block` handler that signals
//! "entered" and awaits "release"), so the actor is known to be between two
//! `handle` invocations when the query is queued.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Poll;

use tokio::sync::Notify;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason,
};

const BACKLOG: usize = 300;
const ROUNDS: usize = 8;
/// The most polls the competitor may get between the stop and the actor's end
/// when the loop is woken by the stop itself.
const WOKEN_AT_ONCE: usize = 8;

enum Msg {
    /// Signals `entered`, then waits for `release`.
    Block,
    /// Does nothing.
    Add,
}

struct Gate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    /// The competitor's poll count, and the count when `pre_stop` ran.
    ticks: Arc<AtomicUsize>,
    seen: Arc<AtomicUsize>,
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

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.seen
            .store(self.ticks.load(Ordering::SeqCst), Ordering::SeqCst);
        true
    }
}

/// A task that is runnable at every poll: it wakes itself without deferring,
/// and counts its polls.
async fn competitor(ticks: Arc<AtomicUsize>, done: Arc<AtomicBool>) {
    std::future::poll_fn(|cx| {
        if done.load(Ordering::SeqCst) {
            return Poll::Ready(());
        }
        ticks.fetch_add(1, Ordering::SeqCst);
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
}

/// The competitor's polls between a stop sent while the actor is handed back
/// after a lone reply and the moment the actor runs `pre_stop`. `phase` moves the scheduler's tick
/// count, so the rounds meet the flush interval at different points.
async fn polls_until_the_actor_ends(phase: usize) -> usize {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let ticks = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(AtomicUsize::new(0));
    let handle = Gate {
        entered: entered.clone(),
        release: release.clone(),
        ticks: ticks.clone(),
        seen: seen.clone(),
    }
    .spawn()
    .with_config(ActorConfig::default().with_mailbox_capacity(BACKLOG + 8))
    .await
    .unwrap();

    handle.notify(Msg::Block).await.unwrap();
    entered.notified().await;
    for _ in 0..BACKLOG {
        handle.notify(Msg::Add).await.unwrap();
    }

    // Queue the query from a runtime task, which pushes it on its first poll.
    let query = tokio::spawn({
        let handle = handle.clone();
        async move { handle.get_status().await }
    });
    tokio::task::yield_now().await;

    let done = Arc::new(AtomicBool::new(false));
    let rival = tokio::spawn(competitor(ticks.clone(), done.clone()));
    for _ in 0..phase {
        tokio::task::yield_now().await;
    }

    // The stop is sent from a task of its own, not from the test body: the
    // test body runs only between scheduler rounds, after the deferred wakes
    // were flushed, while a task runs inside a round, as a client does.
    let requester = tokio::spawn({
        let handle = handle.clone();
        let ticks = ticks.clone();
        async move {
            // The reply: the actor then runs a few more turns and hands the
            // thread back with most of the backlog still queued.
            let info = query.await.unwrap().unwrap();
            assert!(
                info.mailbox_len >= BACKLOG - 1,
                "answered behind the backlog"
            );
            let before = ticks.load(Ordering::SeqCst);
            handle.stop(StopReason::Graceful).await.unwrap();
            before
        }
    });
    release.notify_one();
    let before = requester.await.unwrap();
    handle.wait_stopped().await;
    let polls = seen.load(Ordering::SeqCst) - before;

    done.store(true, Ordering::SeqCst);
    rival.await.unwrap();
    polls
}

#[tokio::test(flavor = "current_thread")]
async fn a_stop_wakes_a_loop_that_is_handed_back() {
    let mut worst = 0;
    for round in 0..ROUNDS {
        worst = worst.max(polls_until_the_actor_ends(round % 8).await);
    }
    assert!(
        worst <= WOKEN_AT_ONCE,
        "the competitor ran {worst} times before the actor saw the stop"
    );
}
