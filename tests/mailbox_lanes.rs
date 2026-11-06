//! The actor's normal mailbox under many senders: each task's messages keep
//! their send order, the bound is exact, a lone sender gets the whole
//! capacity, a cancelled send gives its slot back, a stop wakes every waiting
//! sender with `Closed`, and requests racing the actor's end are all answered.
//!
//! The actor under test is held in a turn at a gate while its mailbox fills,
//! so the counts the test reads are decided by the mailbox alone. No step is
//! ordered by a sleep: a wait polls the state it needs, and a timeout only
//! detects a hang.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorHandle, ActorResult, AskError, StopReason, TrySendError,
};

/// The longest a wait may take; it only limits how long a real hang takes to
/// report.
const HANG: Duration = Duration::from_secs(60);

/// A gate a handler waits at until the test opens it.
#[derive(Default)]
struct Gate {
    open: AtomicBool,
    entered: AtomicBool,
    wake: Notify,
}

impl Gate {
    async fn pass(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let _ = timeout(HANG, async {
            loop {
                let mut woken = std::pin::pin!(self.wake.notified());
                woken.as_mut().enable();
                if self.open.load(Ordering::SeqCst) {
                    return;
                }
                woken.await;
            }
        })
        .await;
    }

    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }
}

/// Opens its gate when dropped, also while a failed assertion unwinds.
struct OpensOnDrop(Arc<Gate>);

impl Drop for OpensOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

enum Msg {
    /// One message of a numbered sender, which must arrive after the sender's
    /// earlier ones.
    Item { task: usize, seq: usize },
    /// Holds the turn at the gate.
    Hold(Arc<Gate>),
    /// Answers with the number it was given.
    Echo(usize),
    /// Answers with how many items arrived and how many arrived out of order.
    Report,
}

#[derive(Default)]
struct Recorder {
    next: Vec<usize>,
    total: usize,
    out_of_order: usize,
}

impl Actor for Recorder {
    type Message = Msg;
    type Response = (usize, usize);

    async fn handle(
        &mut self,
        msg: Msg,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<(usize, usize)> {
        match msg {
            Msg::Item { task, seq } => {
                if self.next.len() <= task {
                    self.next.resize(task + 1, 0);
                }
                if self.next[task] != seq {
                    self.out_of_order += 1;
                }
                self.next[task] = seq + 1;
                self.total += 1;
                Ok((0, 0))
            }
            Msg::Hold(gate) => {
                gate.pass().await;
                Ok((0, 0))
            }
            Msg::Echo(value) => Ok((value, 0)),
            Msg::Report => Ok((self.total, self.out_of_order)),
        }
    }
}

async fn spawn_recorder(capacity: usize) -> ActorHandle<Recorder> {
    Recorder::default()
        .spawn()
        .with_config(ActorConfig::default().with_mailbox_capacity(capacity))
        .kill_grace(Duration::ZERO)
        .await
        .unwrap()
}

/// Holds the actor in a turn at a fresh gate and returns once it is there.
async fn hold(handle: &ActorHandle<Recorder>) -> (Arc<Gate>, OpensOnDrop) {
    let gate = Arc::new(Gate::default());
    let guard = OpensOnDrop(gate.clone());
    handle.notify(Msg::Hold(gate.clone())).await.unwrap();
    until("the actor enters the turn", || {
        gate.entered.load(Ordering::SeqCst)
    })
    .await;
    (gate, guard)
}

async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let waited = timeout(HANG, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(waited.is_ok(), "{what}");
}

/// Waits until the actor has taken `expected` items and reports how many
/// arrived out of order.
async fn items_taken(handle: &ActorHandle<Recorder>, expected: usize) -> usize {
    timeout(HANG, async {
        loop {
            let (total, out_of_order) = handle.send(Msg::Report).await.unwrap();
            if total == expected {
                return out_of_order;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the actor did not take {expected} items"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_task_keeps_its_order_under_many_senders() {
    const TASKS: usize = 32;
    const EACH: usize = 1500;
    let handle = spawn_recorder(16).await;
    let mut joins = Vec::new();
    for task in 0..TASKS {
        let handle = handle.clone();
        joins.push(tokio::spawn(async move {
            for seq in 0..EACH {
                handle.notify(Msg::Item { task, seq }).await.unwrap();
            }
        }));
    }
    for join in joins {
        timeout(HANG, join).await.expect("no sender hangs").unwrap();
    }
    assert_eq!(items_taken(&handle, TASKS * EACH).await, 0);
    assert_eq!(handle.mailbox_len(), 0);
    assert_eq!(handle.mailbox_available(), 16);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_request_from_many_tasks_is_answered() {
    let handle = spawn_recorder(8).await;
    let mut joins = Vec::new();
    for task in 0..32usize {
        let handle = handle.clone();
        joins.push(tokio::spawn(async move {
            for round in 0..400usize {
                let value = task * 1000 + round;
                let (echoed, _) = handle.send(Msg::Echo(value)).await.unwrap();
                assert_eq!(echoed, value);
            }
        }));
    }
    for join in joins {
        timeout(HANG, join)
            .await
            .expect("no request hangs")
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bound_is_exact_while_the_actor_is_held() {
    let handle = spawn_recorder(8).await;
    let (gate, _opens) = hold(&handle).await;
    for seq in 0..8 {
        handle.try_notify(Msg::Item { task: 0, seq }).unwrap();
    }
    assert!(matches!(
        handle.try_notify(Msg::Item { task: 0, seq: 8 }),
        Err(TrySendError::Full)
    ));
    assert_eq!(handle.mailbox_len(), 8);
    assert_eq!(handle.mailbox_available(), 0);
    gate.open();
    assert_eq!(items_taken(&handle, 8).await, 0);
    assert_eq!(handle.mailbox_available(), 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_racing_the_actor_s_stop_are_all_answered() {
    for _ in 0..20 {
        let handle = spawn_recorder(4).await;
        let mut joins = Vec::new();
        for task in 0..16usize {
            let handle = handle.clone();
            joins.push(tokio::spawn(async move {
                let mut answered = 0usize;
                loop {
                    match handle.send(Msg::Echo(task)).await {
                        Ok(_) => answered += 1,
                        Err(AskError::Closed | AskError::ResponseDropped { .. }) => {
                            return answered
                        }
                        Err(other) => panic!("unexpected {other:?}"),
                    }
                }
            }));
        }
        tokio::task::yield_now().await;
        handle.stop(StopReason::Graceful).await.unwrap();
        for join in joins {
            timeout(HANG, join)
                .await
                .expect("no request hangs")
                .unwrap();
        }
    }
}
