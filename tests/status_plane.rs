//! Behavioral suite for the runtime status plane:
//! `ActorHandle::status()` and `ActorHandle::wait_stopped()`.
//!
//! - `status()` reads a `watch` cell maintained directly by the runtime, so it
//!   answers instantly even while the actor is stuck inside a callback and not
//!   servicing its mailbox or system channel (contrast with the queue-plane
//!   `get_status()`, which hangs in that same scenario).
//! - `wait_stopped()` resolves for every way an actor can die: a graceful
//!   stop, a kill, and even a task abort (where the status sender is dropped
//!   without ever writing `Stopped`).

use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::sleep;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, ActorSystem, SpawnError, StopReason,
};

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

/// A trivial actor for the status()/wait_stopped() happy paths.
#[derive(Default)]
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// An actor whose handler can be parked on a test-controlled oneshot: while
/// `Hang` is in flight, the actor's single task is fully consumed, so neither
/// its mailbox nor its system channel can be serviced.
#[derive(Default)]
struct Prober;

#[derive(Debug)]
enum ProbeMsg {
    /// Awaits a receiver the test holds the sender for; never resolves until
    /// the test drops or fires that sender.
    Hang(oneshot::Receiver<()>),
    /// A normal message, used to prove the actor is still alive and serving
    /// once a hang is released.
    Ping,
}

impl Actor for Prober {
    type Message = ProbeMsg;
    type Response = ();

    async fn handle(&mut self, msg: ProbeMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ProbeMsg::Hang(rx) => {
                let _ = rx.await;
                Ok(())
            }
            ProbeMsg::Ping => Ok(()),
        }
    }
}

/// An actor whose `pre_start` never returns: the only way out is an abort.
#[derive(Default)]
struct HangingPreStart;

impl Actor for HangingPreStart {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        std::future::pending::<()>().await;
        unreachable!("pending() never resolves")
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_is_running_after_spawn_ack() {
    let handle = Idle.spawn().await.unwrap();

    // The default spawn ack fires right after the Running transition, so by
    // the time `.await` resolves, status() must already agree - no sleep.
    assert_eq!(handle.status(), ActorStatus::Running);

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_answers_running_for_hung_actor() {
    let handle = Prober.spawn().await.unwrap();

    let (tx, rx) = oneshot::channel::<()>();
    handle.notify(ProbeMsg::Hang(rx)).await.unwrap();
    sleep(Duration::from_millis(30)).await;

    // Prove the actor is genuinely stuck first: the queue plane cannot even
    // answer get_status while the handler holds the actor's one task.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), handle.get_status())
            .await
            .is_err(),
        "get_status must hang on a genuinely stuck actor (queue plane)"
    );

    // The runtime plane answers instantly regardless.
    assert_eq!(
        handle.status(),
        ActorStatus::Running,
        "status() must answer even though the handler is parked mid-callback"
    );

    // Release the hang: the actor must resume ordinary service afterward.
    drop(tx);
    handle.send(ProbeMsg::Ping).await.unwrap();
    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_stopped_resolves_on_graceful_stop() {
    let handle = Idle.spawn().await.unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle.wait_stopped())
        .await
        .expect("wait_stopped must resolve after a graceful stop");

    assert_eq!(handle.status(), ActorStatus::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_stopped_resolves_instantly_when_already_stopped() {
    let handle = Idle.spawn().await.unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle.wait_stopped())
        .await
        .expect("wait_stopped must resolve after a graceful stop");

    // The actor is already Stopped. A second call - on a fresh clone of the
    // handle, not just the original - must observe the terminal state
    // instantly: a borrow() of the watch cell's current value, no changed()
    // wait required. A buggy changed()-wait would never resolve here (no
    // further transitions exist), so any finite deadline discriminates; the
    // 500ms one absorbs CI scheduling stalls.
    let cloned = handle.clone();
    tokio::time::timeout(Duration::from_millis(500), cloned.wait_stopped())
        .await
        .expect("wait_stopped must resolve immediately when already Stopped");

    assert_eq!(handle.status(), ActorStatus::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_stopped_resolves_on_kill() {
    let handle = Idle.spawn().await.unwrap();

    handle.stop(StopReason::Kill).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle.wait_stopped())
        .await
        .expect("wait_stopped must resolve after a kill");

    assert_eq!(handle.status(), ActorStatus::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_stopped_resolves_when_aborted_mid_hang() {
    let name = "status-plane-abort-target";

    // Race the timed-out spawn against a poll of the registry: registration
    // happens synchronously before pre_start ever runs, so the name resolves
    // almost immediately, well before the abort fires.
    let (spawn_result, handle) = tokio::join!(
        async {
            HangingPreStart
                .spawn()
                .named(name)
                .start_timeout(Duration::from_millis(100))
                .await
        },
        async {
            loop {
                if let Some(h) = ActorSystem::default().get::<HangingPreStart>(name) {
                    return h;
                }
                tokio::task::yield_now().await;
            }
        }
    );

    match spawn_result {
        Err(SpawnError::StartTimeout) => {}
        Ok(_) => panic!("spawn must time out: pre_start never returns"),
        Err(other) => panic!("expected SpawnError::StartTimeout, got: {other}"),
    }

    // The task was aborted mid pre_start: it never wrote Stopped itself
    // (nobody reached that line), but wait_stopped must still resolve
    // instead of hanging forever on a sender that will never write again.
    tokio::time::timeout(Duration::from_secs(2), handle.wait_stopped())
        .await
        .expect("wait_stopped must resolve once the aborted task's sender drops");
}
