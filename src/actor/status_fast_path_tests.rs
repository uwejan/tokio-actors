//! The turn boundary fast path is off for the 32 turns after a status reply
//! to a requester outside every runtime task (the body of `block_on`, a
//! thread of its own). After such a reply the next 32 turns read every branch
//! in order, the 33rd may take the fast path again, and a reply inside the
//! window restarts the count. A reply to a requester that is a runtime task
//! opens no window.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::actor::context::ActorContext;
use crate::actor::runtime::{fast_path_turns, ActorConfig};
use crate::actor::{Actor, ActorExt};
use crate::error::ActorResult;
use crate::types::StopReason;

use super::{fp_skip_step, STATUS_FAST_PATH_OFF_TURNS};

#[test]
fn the_window_is_32_turns_and_a_reply_restarts_it() {
    assert_eq!(STATUS_FAST_PATH_OFF_TURNS, 32);
    let mut skip = 0u32;
    assert!(!fp_skip_step(&mut skip), "no reply, no window");
    skip = STATUS_FAST_PATH_OFF_TURNS;
    for turn in 1..=32 {
        assert!(fp_skip_step(&mut skip), "turn {turn} after a reply is off");
    }
    assert!(!fp_skip_step(&mut skip), "the 33rd turn is not off");
    assert_eq!(skip, 0);

    skip = STATUS_FAST_PATH_OFF_TURNS;
    for _ in 0..10 {
        assert!(fp_skip_step(&mut skip));
    }
    skip = STATUS_FAST_PATH_OFF_TURNS;
    for turn in 1..=32 {
        assert!(
            fp_skip_step(&mut skip),
            "turn {turn} after the second reply"
        );
    }
    assert!(!fp_skip_step(&mut skip));
}

enum Msg {
    /// Signals `entered`, then waits for `release`.
    Block,
    /// Records how many fast path turns the thread had taken when it ran.
    Mark,
}

struct Marker {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    seen: Arc<Mutex<Vec<u32>>>,
}

impl Actor for Marker {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Block => {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Msg::Mark => self.seen.lock().unwrap().push(fast_path_turns::read()),
        }
        Ok(())
    }
}

/// Which of the marked turns took the fast path, in order: a mark is a turn
/// that took it when the thread's count rose since the mark before.
fn taken(seen: &[u32]) -> Vec<bool> {
    seen.windows(2).map(|w| w[1] > w[0]).collect()
}

type Query = std::pin::Pin<Box<dyn Future<Output = crate::types::ActorStatusInfo>>>;

/// Queues `marks` marked messages behind a held turn and a status query.
/// With `hold_after`, a second held turn follows that many marks and a
/// second query is queued while it is held. The queries are made from the
/// test body, which is not a runtime task, or from a spawned task. Returns the
/// marks' fast path record.
async fn run(marks: usize, hold_after: Option<usize>, from_task: bool) -> Vec<bool> {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handle = Marker {
        entered: entered.clone(),
        release: release.clone(),
        seen: seen.clone(),
    }
    .spawn()
    .with_config(ActorConfig::default().with_mailbox_capacity(marks + 8))
    .await
    .unwrap();

    // A body query is polled once here to queue it and awaited later from the
    // same body, so its requester is not a runtime task.
    let query = |handle: &crate::actor::handle::ActorHandle<Marker>| -> Query {
        let handle = handle.clone();
        let query = async move { handle.get_status().await.unwrap() };
        if from_task {
            let task = tokio::spawn(query);
            return Box::pin(async move { task.await.unwrap() });
        }
        let mut query = Box::pin(query);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(query.as_mut().poll(&mut cx).is_pending());
        query
    };

    handle.notify(Msg::Block).await.unwrap();
    entered.notified().await;
    let first = query(&handle);
    // A spawned query pushes itself on its first poll.
    tokio::task::yield_now().await;
    let mut second = None;
    let mut queued = 0;
    while queued < marks - 1 {
        handle.notify(Msg::Mark).await.unwrap();
        queued += 1;
        if hold_after == Some(queued) {
            handle.notify(Msg::Block).await.unwrap();
        }
    }
    release.notify_one();
    first.await;
    if hold_after.is_some() {
        entered.notified().await;
        second = Some(query(&handle));
        tokio::task::yield_now().await;
        release.notify_one();
    }
    if let Some(second) = second {
        second.await;
    }
    // The last mark is awaited: the backlog ahead of it has run.
    handle.send(Msg::Mark).await.unwrap();
    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;
    let seen = seen.lock().unwrap().clone();
    taken(&seen)
}

#[tokio::test(flavor = "current_thread")]
async fn the_32_turns_after_a_status_reply_skip_the_fast_path() {
    let record = run(40, None, false).await;
    assert_eq!(record.len(), 39);
    // Mark k (1-based) is turn k after the reply; record[k - 2] says whether
    // mark k took the fast path.
    for k in 2..=32 {
        assert!(
            !record[k - 2],
            "turn {k} after the reply took the fast path"
        );
    }
    assert!(
        record[32 - 1],
        "the 33rd turn after the reply takes the fast path"
    );
    assert!(record[32..].iter().all(|taken| *taken));
}

#[tokio::test(flavor = "current_thread")]
async fn a_reply_inside_the_window_restarts_the_count() {
    // The second query is answered after mark 10 and the held turn: turns 1
    // to 10 after the first reply, then the second reply, then 32 off turns.
    let record = run(60, Some(10), false).await;
    // Marks 1..=10 precede the second hold; marks 11.. follow its reply.
    // record[i] describes mark i + 2.
    for k in 2..=10 {
        assert!(!record[k - 2], "mark {k} is inside the first window");
    }
    for k in 11..=(10 + 32) {
        assert!(!record[k - 2], "mark {k} is inside the restarted window");
    }
    assert!(
        record[10 + 33 - 2],
        "the 33rd turn after the second reply takes the fast path"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_reply_to_a_task_requester_opens_no_window() {
    let record = run(40, None, true).await;
    assert_eq!(record.len(), 39);
    assert!(
        record.iter().all(|taken| *taken),
        "a turn after a reply to a task requester skipped the fast path: {record:?}"
    );
}
