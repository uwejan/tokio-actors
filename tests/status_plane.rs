//! Behavioral suite for the runtime status plane:
//! `ActorHandle::status()` and `ActorHandle::wait_stopped()`.
//!
//! - `status()` reads a per-actor cell maintained directly by the runtime, so it
//!   answers instantly even while the actor is stuck inside a callback and not
//!   servicing its mailbox or system channel (contrast with the queue-plane
//!   `get_status()`, which hangs in that same scenario).
//! - `wait_stopped()` resolves for every way an actor can die: a graceful
//!   stop, a kill, and even a task abort (where the status sender is dropped
//!   without ever writing `Stopped`).

use std::time::Duration;

use tokio::sync::oneshot;

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
    /// Fires the first sender once the handler is running, then awaits a
    /// receiver the test holds the other sender for; never resolves until the
    /// test drops or fires that sender.
    Hang(oneshot::Sender<()>, oneshot::Receiver<()>),
    /// A normal message, used to prove the actor is still alive and serving
    /// once a hang is released.
    Ping,
}

impl Actor for Prober {
    type Message = ProbeMsg;
    type Response = ();

    async fn handle(&mut self, msg: ProbeMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ProbeMsg::Hang(entered, rx) => {
                let _ = entered.send(());
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
async fn status_answers_running_for_hung_actor() {
    let handle = Prober.spawn().await.unwrap();

    let (tx, rx) = oneshot::channel::<()>();
    let (entered_tx, entered_rx) = oneshot::channel::<()>();
    handle.notify(ProbeMsg::Hang(entered_tx, rx)).await.unwrap();
    // The handler reports that it is inside the callback.
    tokio::time::timeout(Duration::from_secs(10), entered_rx)
        .await
        .expect("the handler must start the hang")
        .expect("the handler dropped its report");

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
