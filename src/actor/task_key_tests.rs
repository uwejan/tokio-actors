//! The record of the incarnation a thread is polling: set for the length of a
//! poll, put back on every path out of it, matched only by the polling task.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::Id;

use super::*;
use crate::actor::context::ActorContext;
use crate::actor::task_key::{current_for_test, is_self_call};
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorResult, AskError};

/// What a thread records while it polls an incarnation: its key and the id
/// of the task that polls it.
type Record = (u64, Option<Id>);

/// What a thread that polls nothing reads.
const NOTHING: Record = (0, None);

/// The record a poll of `incarnation` by the current task makes.
fn record_of(incarnation: Incarnation) -> Record {
    (incarnation.number(), tokio::task::try_id())
}

/// A future that is ready at its second poll and reports what it read at
/// each.
struct ReadsTwice {
    seen: Arc<Mutex<Vec<Record>>>,
    polled: bool,
}

impl std::future::Future for ReadsTwice {
    type Output = StopReason;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<StopReason> {
        self.seen.lock().unwrap().push(current_for_test());
        if self.polled {
            return Poll::Ready(StopReason::Graceful);
        }
        self.polled = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// A future that panics in its poll unwinds out of the poll; the record is
/// put back all the same.
#[tokio::test(flavor = "current_thread")]
async fn the_record_is_put_back_when_the_poll_unwinds() {
    let record = AbortRecord::new();
    let join = tokio::spawn(MarkedRun::new(
        TaskMarker::default(),
        record.incarnation(),
        async {
            let _ = tokio::task::yield_now().await;
            if true {
                panic!("the poll unwinds");
            }
            StopReason::Graceful
        },
    ));
    assert!(join.await.unwrap_err().is_panic());
    assert_eq!(
        current_for_test(),
        NOTHING,
        "the thread is not left in the poll"
    );
    // A task that polls next on this thread finds nothing recorded.
    let seen =
        tokio::spawn(async move { (current_for_test(), is_self_call(record.incarnation())) })
            .await
            .unwrap();
    assert_eq!(seen, (NOTHING, false));
}

/// A poll nested inside another one puts the outer record back when it ends.
#[tokio::test(flavor = "current_thread")]
async fn a_nested_poll_puts_the_outer_record_back() {
    tokio::spawn(async {
        let outer = AbortRecord::new();
        let inner = AbortRecord::new();
        let _outer_scope = PollScope::enter(outer.incarnation(), tokio::task::try_id());
        let outer_record = current_for_test();
        assert_eq!(outer_record, record_of(outer.incarnation()));
        {
            let _inner_scope = PollScope::enter(inner.incarnation(), tokio::task::try_id());
            assert_eq!(current_for_test(), record_of(inner.incarnation()));
            assert!(inner.is_self_call());
            assert!(
                !outer.is_self_call(),
                "the inner poll is not the outer actor"
            );
        }
        assert_eq!(current_for_test(), outer_record);
        assert!(outer.is_self_call());
        assert!(!inner.is_self_call());
    })
    .await
    .unwrap();
}

/// A task polled inside the actor's poll under an id of its own is not the
/// actor's task, whatever the thread runs; the future the poll drives
/// itself is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_polled_inside_the_poll_under_its_own_id_is_not_it() {
    let record = AbortRecord::new();
    let incarnation = record.incarnation();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(MarkedRun::new(
        TaskMarker::default(),
        incarnation,
        async move {
            let inner = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let report = tokio::task::block_in_place(move || {
                inner.block_on(async {
                    let driven = is_self_call(incarnation);
                    let spawned = tokio::spawn(async move { is_self_call(incarnation) })
                        .await
                        .unwrap();
                    (driven, spawned)
                })
            });
            tx.send(report).unwrap();
            StopReason::Graceful
        },
    ));
    join.await.unwrap();
    let (driven, spawned) = rx.await.unwrap();
    assert!(driven, "the future the actor drives itself is its own task");
    assert!(
        !spawned,
        "a task of another runtime, polled on the actor's thread inside its poll"
    );
}

/// Dropped state of the future that checks the key while it drops.
struct ChecksOnDrop {
    record: AbortRecord,
    seen: Arc<Mutex<Option<bool>>>,
}

impl Drop for ChecksOnDrop {
    fn drop(&mut self) {
        *self.seen.lock().unwrap() = Some(self.record.is_self_call());
    }
}

/// A future that is dropped before it ends drops its state inside the record,
/// so a call made from that drop is refused, not parked, and the thread holds
/// no record afterwards. Rows: cut short by an abort, dropped with the runtime
/// that holds it, and dropped before its first poll.
#[test]
fn a_future_dropped_before_it_ends_drops_its_state_in_the_record() {
    #[derive(Clone, Copy, Debug)]
    enum Row {
        Abort,
        RuntimeShutdown,
        NeverPolled,
    }
    for row in [Row::Abort, Row::RuntimeShutdown, Row::NeverPolled] {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let record = AbortRecord::new();
        let seen = Arc::new(Mutex::new(None));
        let held = ChecksOnDrop {
            record: record.clone(),
            seen: seen.clone(),
        };
        let (up_tx, up_rx) = tokio::sync::oneshot::channel();
        let run = MarkedRun::new(TaskMarker::default(), record.incarnation(), async move {
            let _held = held;
            let _ = up_tx.send(());
            std::future::pending::<()>().await;
        });
        match row {
            Row::Abort => rt.block_on(async {
                let join = tokio::spawn(run);
                up_rx.await.unwrap();
                join.abort();
                assert!(join.await.unwrap_err().is_cancelled());
            }),
            Row::RuntimeShutdown => {
                rt.block_on(async {
                    tokio::spawn(run);
                    up_rx.await.unwrap();
                });
                assert_eq!(*seen.lock().unwrap(), None, "{row:?}: still held");
                drop(rt);
            }
            Row::NeverPolled => rt.block_on(async {
                tokio::spawn(async move { drop(run) }).await.unwrap();
            }),
        }
        assert_eq!(
            *seen.lock().unwrap(),
            Some(true),
            "{row:?}: the record is in place while the future's state drops"
        );
        assert_eq!(
            current_for_test(),
            NOTHING,
            "{row:?}: the thread is left holding a record"
        );
    }
}

/// Sends to itself at its start, then reports whether it was refused.
struct StartsWithSelfSend {
    refused: Arc<Mutex<Option<bool>>>,
    stage: Arc<Notify>,
}

impl Actor for StartsWithSelfSend {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let answer = ctx
            .self_handle()
            .send_timeout((), Duration::from_millis(50))
            .await;
        *self.refused.lock().unwrap() = Some(matches!(answer, Err(AskError::CallingSelf)));
        self.stage.notify_one();
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// The record is in place from the first poll, so a self-send in `pre_start`
/// is refused, not enqueued.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_is_set_before_the_first_hook() {
    let refused = Arc::new(Mutex::new(None));
    let stage = Arc::new(Notify::new());
    let _actor = StartsWithSelfSend {
        refused: refused.clone(),
        stage: stage.clone(),
    }
    .spawn()
    .await
    .unwrap();
    stage.notified().await;
    assert_eq!(*refused.lock().unwrap(), Some(true));
}
