//! Behavioral suite for the stop lane: the `watch`-based signal that
//! delivers stop/kill requests, replacing the escalation ladder's former
//! awaited-send transport.
//!
//! Covers: turn-boundary delivery under a flooded mailbox (never racing an
//! in-flight handler), monotone severity (a `Kill` in flight is never
//! displaced, and never displaces nothing weaker either - order-independent),
//! coalescing of multiple raises landing between two turn boundaries, the
//! veto-then-resend re-fire guarantee, and the already-terminated-actor error
//! parity with the pre-lane closed-mpsc behavior.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::sleep;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, SendError, StopReason,
};

// ---------------------------------------------------------------------------
// Helper actor
// ---------------------------------------------------------------------------

/// An actor whose handler can be parked on a test-controlled `Notify` pair:
/// while `Hang` is in flight, the actor's single task is fully consumed, so
/// its stop lane, system channel, and mailbox are all left unserviced until
/// the test releases it. `pre_stop` counts its own invocations and vetoes
/// every call whose 1-based index is below `accept_from`.
struct Prober {
    pre_stop_calls: Arc<AtomicUsize>,
    on_stopped_calls: Arc<AtomicUsize>,
    accept_from: usize,
}

impl Default for Prober {
    fn default() -> Self {
        Self {
            pre_stop_calls: Arc::new(AtomicUsize::new(0)),
            on_stopped_calls: Arc::new(AtomicUsize::new(0)),
            accept_from: usize::MAX, // veto forever unless overridden
        }
    }
}

enum ProbeMsg {
    /// Notifies `started` as soon as the handler begins, then blocks until
    /// `release` fires - giving the test a deterministic window in which the
    /// actor's task is fully occupied and cannot service its `select!`.
    Hang {
        started: Arc<Notify>,
        release: Arc<Notify>,
    },
    /// A no-op, used only to fill a mailbox slot for the flooding test.
    Noop,
}

impl Actor for Prober {
    type Message = ProbeMsg;
    type Response = ();

    async fn handle(&mut self, msg: ProbeMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ProbeMsg::Hang { started, release } => {
                started.notify_one();
                release.notified().await;
                Ok(())
            }
            ProbeMsg::Noop => Ok(()),
        }
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        let n = self.pre_stop_calls.fetch_add(1, Ordering::SeqCst) + 1;
        n >= self.accept_from
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.on_stopped_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Item 4: stop to an already-terminated actor
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_after_already_stopped_matches_closed_mpsc_error() {
    let handle = Prober::default()
        .spawn()
        .named("lane-already-dead")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    // accept_from defaults to MAX (always veto), so escalate straight to
    // Kill to guarantee termination without depending on pre_stop.
    handle.stop(StopReason::Kill).await.unwrap();
    handle.wait_stopped().await;

    let stop_err = handle.stop(StopReason::Graceful).await.unwrap_err();
    assert!(
        matches!(stop_err, SendError::Closed),
        "got {stop_err:?}, expected the same Closed error a full mailbox send used to return"
    );

    // Cross-check against the pre-existing closed-mailbox error for the same
    // dead actor: both planes must agree on "already stopped".
    let notify_err = handle.notify(ProbeMsg::Noop).await.unwrap_err();
    assert!(matches!(notify_err, SendError::Closed));
}

// ---------------------------------------------------------------------------
// Item 5: veto-then-resend re-fires pre_stop
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn veto_then_resend_same_severity_re_fires_pre_stop() {
    let actor = Prober {
        accept_from: 2, // first Graceful is vetoed, second is accepted
        ..Prober::default()
    };
    let pre_stop_calls = actor.pre_stop_calls.clone();
    let on_stopped_calls = actor.on_stopped_calls.clone();

    let handle = actor
        .spawn()
        .named("lane-veto-then-resend")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    sleep(Duration::from_millis(50)).await;
    assert_eq!(pre_stop_calls.load(Ordering::SeqCst), 1);
    assert!(
        handle.is_alive(),
        "the first, vetoed raise must not stop the actor"
    );

    // Same severity, a fresh generation: must land and re-fire pre_stop.
    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;

    assert_eq!(pre_stop_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        on_stopped_calls.load(Ordering::SeqCst),
        1,
        "the accepted Graceful stop runs on_stopped"
    );
}

// ---------------------------------------------------------------------------
// Item 1: coalescing - N raises between two turn boundaries collapse to one
// observation; N raises each separated by an observed turn boundary produce
// N observations. Together these pin both ends of ">=1 and <=N".
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coalesced_raises_during_a_busy_handler_fire_pre_stop_exactly_once() {
    let actor = Prober::default(); // vetoes forever: isolates the count
    let pre_stop_calls = actor.pre_stop_calls.clone();

    let handle = actor
        .spawn()
        .named("lane-coalesce")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    handle
        .notify(ProbeMsg::Hang {
            started: started.clone(),
            release: release.clone(),
        })
        .await
        .unwrap();
    started.notified().await; // the actor's task is now fully occupied

    // Five raises land while the loop cannot possibly be polling the lane.
    for _ in 0..5 {
        handle.stop(StopReason::Graceful).await.unwrap();
    }

    release.notify_one();
    sleep(Duration::from_millis(50)).await;

    assert_eq!(
        pre_stop_calls.load(Ordering::SeqCst),
        1,
        "N raises landing between two turn boundaries must coalesce into a \
         single observation, not N"
    );
    assert!(handle.is_alive());

    handle.stop(StopReason::Kill).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_observed_raises_each_produce_their_own_pre_stop_call() {
    let actor = Prober::default(); // vetoes forever
    let pre_stop_calls = actor.pre_stop_calls.clone();

    let handle = actor
        .spawn()
        .named("lane-sequential")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    const N: usize = 4;
    for i in 1..=N {
        handle.stop(StopReason::Graceful).await.unwrap();
        // Wait for this raise's own observation before issuing the next one,
        // so none of them can coalesce with each other.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while pre_stop_calls.load(Ordering::SeqCst) < i {
            assert!(
                tokio::time::Instant::now() < deadline,
                "raise {i} was never observed"
            );
            sleep(Duration::from_millis(5)).await;
        }
    }

    assert_eq!(pre_stop_calls.load(Ordering::SeqCst), N);
    assert!(handle.is_alive());

    handle.stop(StopReason::Kill).await.unwrap();
}

// ---------------------------------------------------------------------------
// Severity monotonicity: Kill always wins, regardless of raise order, and a
// superseded lower-severity raise is never separately delivered (not even a
// vetoed one) - it disappears entirely into the single coalesced cell.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_before_pending_graceful_observation_suppresses_it_entirely() {
    let actor = Prober::default();
    let pre_stop_calls = actor.pre_stop_calls.clone();
    let on_stopped_calls = actor.on_stopped_calls.clone();

    let handle = actor
        .spawn()
        .named("lane-kill-supersedes")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    handle
        .notify(ProbeMsg::Hang {
            started: started.clone(),
            release: release.clone(),
        })
        .await
        .unwrap();
    started.notified().await;

    // Graceful, then Kill, both while the loop cannot observe either yet.
    handle.stop(StopReason::Graceful).await.unwrap();
    handle.stop(StopReason::Kill).await.unwrap();

    release.notify_one();
    handle.wait_stopped().await;

    assert_eq!(
        pre_stop_calls.load(Ordering::SeqCst),
        0,
        "the superseded Graceful raise must never be separately delivered - \
         Kill bypasses pre_stop, and coalescing means there is only ever one \
         cell, not a queue of both"
    );
    assert_eq!(
        on_stopped_calls.load(Ordering::SeqCst),
        0,
        "Kill bypasses on_stopped too"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_after_kill_has_no_effect_once_kill_already_landed() {
    let actor = Prober::default();
    let pre_stop_calls = actor.pre_stop_calls.clone();
    let on_stopped_calls = actor.on_stopped_calls.clone();

    let handle = actor
        .spawn()
        .named("lane-kill-first")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    handle
        .notify(ProbeMsg::Hang {
            started: started.clone(),
            release: release.clone(),
        })
        .await
        .unwrap();
    started.notified().await;

    // Kill first this time, then a weaker Graceful: order must not matter.
    handle.stop(StopReason::Kill).await.unwrap();
    handle.stop(StopReason::Graceful).await.unwrap();

    release.notify_one();
    handle.wait_stopped().await;

    assert_eq!(pre_stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(on_stopped_calls.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Item 2: turn-boundary delivery under a flooded mailbox - Kill is observed
// only once the in-flight handler completes, never mid-handler, even though
// the mailbox is simultaneously full and the lane outranks it.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_under_a_flooded_mailbox_waits_for_the_in_flight_handler() {
    let config = ActorConfig::default().with_mailbox_capacity(1);
    let handle = Prober::default()
        .spawn()
        .named("lane-flood-turn-boundary")
        .with_config(config)
        .await
        .unwrap();

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    handle
        .notify(ProbeMsg::Hang {
            started: started.clone(),
            release: release.clone(),
        })
        .await
        .unwrap();
    started.notified().await; // handler is now in flight

    // Flood the mailbox behind the in-flight handler.
    let _ = handle.try_notify(ProbeMsg::Noop);

    // Kill outranks both the system channel and the (full) mailbox, but must
    // still not race the in-flight handler.
    handle.stop(StopReason::Kill).await.unwrap();
    sleep(Duration::from_millis(30)).await;
    assert!(
        handle.is_alive(),
        "Kill must not cancel or race an in-flight handler invocation (ITP)"
    );

    release.notify_one();
    handle.wait_stopped().await;
}

// ---------------------------------------------------------------------------
// Item 3: the escalation ladder signals through the lane everywhere, never
// through an awaited channel send.
// ---------------------------------------------------------------------------

#[test]
fn escalation_ladder_contains_no_awaited_stop_sends() {
    // Every module that participates in stop/kill delivery - spawning,
    // supervision's escalation ladder, and system-level shutdown alike -
    // must signal through the stop lane (a synchronous, infallible watch
    // write), never through an awaited send of a `Stop` message.
    let sources: &[(&str, &str)] = &[
        (
            "src/actor/runtime.rs",
            include_str!("../src/actor/runtime.rs"),
        ),
        (
            "src/actor/context.rs",
            include_str!("../src/actor/context.rs"),
        ),
        (
            "src/actor/supervision.rs",
            include_str!("../src/actor/supervision.rs"),
        ),
        (
            "src/actor/handle.rs",
            include_str!("../src/actor/handle.rs"),
        ),
        ("src/system.rs", include_str!("../src/system.rs")),
        ("src/types/mod.rs", include_str!("../src/types/mod.rs")),
    ];
    for (path, src) in sources {
        assert!(
            !src.contains("SystemMessage::Stop"),
            "{path} still constructs SystemMessage::Stop - stop/kill delivery \
             must go through the stop lane instead"
        );
    }
}
