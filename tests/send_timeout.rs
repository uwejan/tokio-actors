//! Behavioral suite for `ActorHandle::send_timeout`.
//!
//! `gen_server:call/3` parity: one deadline spans both enqueueing the request
//! and awaiting the reply, armed before the mailbox send so a request parked
//! on a full mailbox still times out.

use std::time::Duration;

use tokio::time::Instant;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, AskError,
};

/// A worker whose handler can be told to hang forever, sleep for a given
/// duration before replying, or echo a value back immediately.
#[derive(Default)]
struct Worker;

#[derive(Clone)]
enum WorkerMsg {
    /// Parks the handler on a never-ready future, so the mailbox slot it
    /// occupied is never returned and the actor never revisits its loop.
    Hang,
    /// Sleeps for the given duration, then replies with `7`.
    Sleep(Duration),
    /// Replies immediately with the given value.
    Echo(u32),
}

impl Actor for Worker {
    type Message = WorkerMsg;
    type Response = u32;

    async fn handle(&mut self, msg: WorkerMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        match msg {
            WorkerMsg::Hang => {
                std::future::pending::<()>().await;
                unreachable!("pending() never resolves")
            }
            WorkerMsg::Sleep(dur) => {
                tokio::time::sleep(dur).await;
                Ok(7)
            }
            WorkerMsg::Echo(value) => Ok(value),
        }
    }
}

// ---------------------------------------------------------------------------
// Full mailbox + hung handler
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn full_mailbox_with_hung_handler_times_out_unenqueued() {
    let handle = Worker
        .spawn()
        .with_config(ActorConfig::default().with_mailbox_capacity(1))
        .await
        .unwrap();

    // Dequeued almost immediately (the mailbox starts empty with one free
    // slot); parks the handler on `pending()` forever.
    handle.notify(WorkerMsg::Hang).await.unwrap();
    // Give the actor task a chance to actually pull `Hang` out of the
    // mailbox and start awaiting it before we refill the single slot.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Takes the only mailbox slot. It can never be dequeued: the handler is
    // wedged inside the first message's `handle()` call for good.
    handle.try_notify(WorkerMsg::Echo(0)).unwrap();

    // The mailbox is full and will stay that way, so the enqueue phase
    // itself must time out - the message was never sent.
    let deadline = Duration::from_millis(100);
    let t0 = Instant::now();
    let result = handle.send_timeout(WorkerMsg::Echo(1), deadline).await;
    let elapsed = t0.elapsed();

    assert!(
        matches!(result, Err(AskError::Timeout { enqueued: false })),
        "expected Timeout {{ enqueued: false }}, got {result:?}"
    );
    assert!(
        elapsed < deadline + Duration::from_secs(2),
        "send_timeout must return from the deadline, not the forever-hung \
         handler (bound is loose for CI scheduling stalls), took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Slow handler, no cancellation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn slow_handler_times_out_enqueued_then_actor_survives() {
    let handle = Worker.spawn().await.unwrap();

    // Plenty of mailbox room, so the enqueue phase succeeds immediately and
    // the deadline is spent entirely waiting on the reply.
    let result = handle
        .send_timeout(
            WorkerMsg::Sleep(Duration::from_millis(200)),
            Duration::from_millis(30),
        )
        .await;

    assert!(
        matches!(result, Err(AskError::Timeout { enqueued: true })),
        "expected Timeout {{ enqueued: true }}, got {result:?}"
    );

    // Let the slow handler actually finish; it must not have been cancelled
    // by the deadline. Its late reply lands in a dropped oneshot and is
    // silently discarded.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        handle.is_alive(),
        "actor must survive a send_timeout deadline"
    );

    // A subsequent plain send must succeed, proving the handler ran to
    // completion and the actor returned to its message loop instead of
    // crashing.
    let value = handle.send(WorkerMsg::Echo(9)).await.unwrap();
    assert_eq!(value, 9);
}

// ---------------------------------------------------------------------------
// Healthy fast actor, fast path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn healthy_fast_actor_send_timeout_matches_send() {
    let handle = Worker.spawn().await.unwrap();

    let via_send = handle.send(WorkerMsg::Echo(42)).await.unwrap();
    let via_send_timeout = handle
        .send_timeout(WorkerMsg::Echo(42), Duration::from_secs(5))
        .await
        .unwrap();

    assert_eq!(via_send, 42);
    assert_eq!(via_send_timeout, 42);
    assert_eq!(via_send, via_send_timeout);
}

// ---------------------------------------------------------------------------
// Pathological deadline duration
// ---------------------------------------------------------------------------

// A plain `Instant::now() + Duration::MAX` panics on overflow; `send_timeout`
// must saturate the deadline instead, so a caller can pass an effectively
// "no timeout" duration without risking a panic.
#[tokio::test]
async fn send_timeout_with_huge_duration_does_not_panic() {
    let handle = Worker.spawn().await.unwrap();

    let result = handle.send_timeout(WorkerMsg::Echo(7), Duration::MAX).await;

    assert!(matches!(result, Ok(7)), "expected Ok(7), got {result:?}");
}
