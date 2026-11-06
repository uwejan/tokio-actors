//! One condition, one outcome: when a stop meets a second failure (an `Err`
//! or a panic from `on_stopped`, or from a final delivery, or a panic from
//! `pre_stop` when a `Failure` stop request landed while it ran), the first
//! failure stays the actor's exit reason, and a second failure becomes
//! the reason only when none stands. A `Kill` raised while `pre_stop` runs is
//! the exit reason when `pre_stop` then panics, whatever it panics with.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, ChildEvent, RestartType, StopReason, SupervisionAction,
};

mod common;

#[derive(Clone, Copy)]
enum Crash {
    HandleErr,
    HandlePanic,
}

#[derive(Clone, Copy)]
enum Cleanup {
    Err,
    Panic,
}

struct Subject {
    cleanup: Cleanup,
}

impl Actor for Subject {
    type Message = Crash;
    type Response = ();

    async fn handle(&mut self, msg: Crash, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Crash::HandleErr => Err(ActorError::user("db down")),
            Crash::HandlePanic => panic!("handler panic"),
        }
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        match self.cleanup {
            Cleanup::Err => Err(ActorError::user("close failed")),
            Cleanup::Panic => panic!("close panicked"),
        }
    }
}

async fn end_reason(cleanup: Cleanup, crash: Option<Crash>) -> StopReason {
    let h = Subject { cleanup }.spawn().await.unwrap();
    match crash {
        Some(c) => {
            let _ = h.send(c).await;
        }
        None => {
            h.stop(StopReason::Graceful).await.unwrap();
        }
    }
    tokio::time::timeout(Duration::from_secs(10), h.wait_stopped())
        .await
        .expect("the actor ends")
}

#[test]
fn the_first_failure_stays_the_reason_whichever_way_on_stopped_fails() {
    common::run(2, async {
        let db_down = StopReason::Failure(ActorError::user("db down"));
        // An earlier failure stands: an Err from on_stopped, as a panic.
        assert_eq!(
            end_reason(Cleanup::Err, Some(Crash::HandleErr)).await,
            db_down
        );
        assert_eq!(
            end_reason(Cleanup::Panic, Some(Crash::HandleErr)).await,
            db_down
        );
        match end_reason(Cleanup::Err, Some(Crash::HandlePanic)).await {
            StopReason::Failure(ActorError::Panic(text)) => assert!(text.contains("handler panic")),
            other => panic!("the handler's panic stands, got {other:?}"),
        }
        match end_reason(Cleanup::Panic, Some(Crash::HandlePanic)).await {
            StopReason::Failure(ActorError::Panic(text)) => assert!(text.contains("handler panic")),
            other => panic!("the handler's panic stands, got {other:?}"),
        }
    });
}

/// A stop that is no failure (a parent's request, a cancellation, an
/// inherited stop) is not a failure that stands: an `Err` or a panic from
/// `on_stopped` is then the reason the actor ends with.
#[test]
fn a_failure_of_on_stopped_is_the_reason_after_a_stop_that_is_no_failure() {
    common::run(2, async {
        for forced in [
            StopReason::ParentRequest,
            StopReason::Cancelled,
            StopReason::ParentKilled,
        ] {
            let h = Subject {
                cleanup: Cleanup::Err,
            }
            .spawn()
            .await
            .unwrap();
            h.stop(forced.clone()).await.unwrap();
            let reason = tokio::time::timeout(Duration::from_secs(10), h.wait_stopped())
                .await
                .expect("the actor ends");
            assert_eq!(
                reason,
                StopReason::Failure(ActorError::user("close failed")),
                "{forced:?}"
            );

            let h = Subject {
                cleanup: Cleanup::Panic,
            }
            .spawn()
            .await
            .unwrap();
            h.stop(forced.clone()).await.unwrap();
            let reason = tokio::time::timeout(Duration::from_secs(10), h.wait_stopped())
                .await
                .expect("the actor ends");
            match reason {
                StopReason::Failure(ActorError::Panic(text)) => {
                    assert!(text.contains("close panicked"), "{forced:?}: {text}")
                }
                other => panic!("{forced:?}: the cleanup panic is the reason, got {other:?}"),
            }
        }
    });
}

// ---------------------------------------------------------------------------
// The final deliveries of a supervisor
// ---------------------------------------------------------------------------

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Fails its own loop after a child's death is recorded but not yet
/// delivered, then fails the final delivery of that death.
struct Failing {
    delivery: Cleanup,
}

impl Actor for Failing {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let child = ctx
            .spawn_child(|| Idle)
            .restart_type(RestartType::Temporary)
            .await?;
        child.stop(StopReason::Cancelled).await.unwrap();
        child.wait_stopped().await;
        // The loop is inside this handler: the child's death waits for the
        // final delivery, which the failure below makes.
        Err(ActorError::user("supervisor failed"))
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if event.action != SupervisionAction::SupervisorStopping {
            return Ok(());
        }
        match self.delivery {
            Cleanup::Err => Err(ActorError::user("delivery failed")),
            Cleanup::Panic => panic!("delivery panicked"),
        }
    }
}

async fn supervisor_end(delivery: Cleanup) -> StopReason {
    let sup = Failing { delivery }.spawn().supervisor().await.unwrap();
    sup.notify(()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), sup.wait_stopped())
        .await
        .expect("the supervisor ends")
}

#[test]
fn a_failed_final_delivery_does_not_replace_the_loops_failure() {
    common::run(2, async {
        let failed = StopReason::Failure(ActorError::user("supervisor failed"));
        assert_eq!(supervisor_end(Cleanup::Err).await, failed);
        assert_eq!(supervisor_end(Cleanup::Panic).await, failed);
    });
}

// ---------------------------------------------------------------------------
// A panic of `pre_stop`
// ---------------------------------------------------------------------------

/// A panic payload whose own drop panics.
struct PanicsWhenDropped;

impl Drop for PanicsWhenDropped {
    fn drop(&mut self) {
        panic!("the payload panicked when dropped");
    }
}

/// A panic payload whose own drop panics with a payload whose drop panics too.
struct PanicsTwiceWhenDropped;

impl Drop for PanicsTwiceWhenDropped {
    fn drop(&mut self) {
        std::panic::panic_any(PanicsWhenDropped);
    }
}

/// What `pre_stop` panics with.
#[derive(Clone, Copy, Debug)]
enum Payload {
    /// A string, as `panic!("...")` raises.
    Text,
    /// A value whose own drop panics.
    DropPanics,
    /// A value whose own drop panics with a value whose drop panics too.
    DropPanicsTwice,
}

/// What `on_stopped` saw.
#[derive(Clone)]
struct Seen {
    /// The reason `on_stopped` received.
    reason: StopReason,
    /// The last error the context held when `on_stopped` ran.
    last_error: Option<ActorError>,
}

/// Holds `pre_stop` at a gate the test opens, then panics with `payload`.
struct Gated {
    payload: Payload,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    stopped_with: Arc<Mutex<Option<Seen>>>,
}

impl Actor for Gated {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.entered.notify_one();
        self.release.notified().await;
        match self.payload {
            Payload::Text => panic!("pre_stop panicked"),
            Payload::DropPanics => std::panic::panic_any(PanicsWhenDropped),
            Payload::DropPanicsTwice => std::panic::panic_any(PanicsTwiceWhenDropped),
        }
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.stopped_with.lock().unwrap() = Some(Seen {
            reason: reason.clone(),
            last_error: ctx.last_error().cloned(),
        });
        Ok(())
    }
}

/// How a stop with a panicking `pre_stop` ended.
struct PreStopEnd {
    /// The reason `wait_stopped` returned.
    reason: StopReason,
    /// What `on_stopped` saw, if it ran.
    seen: Option<Seen>,
}

/// Stops the actor gracefully, raises `meanwhile` (if any) once `pre_stop` is
/// running, lets `pre_stop` panic with `payload`, and returns how the actor
/// ended. The kill grace outlasts the test, so a `Kill` raised meanwhile ends
/// the actor only through the run loop's own read of the stop lane, never
/// through the abort that backs it.
async fn pre_stop_panic_end(payload: Payload, meanwhile: Option<StopReason>) -> PreStopEnd {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let stopped_with = Arc::new(Mutex::new(None));
    let h = Gated {
        payload,
        entered: entered.clone(),
        release: release.clone(),
        stopped_with: stopped_with.clone(),
    }
    .spawn()
    .kill_grace(Duration::from_secs(3600))
    .await
    .unwrap();
    h.stop(StopReason::Graceful).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("pre_stop starts");
    if let Some(reason) = meanwhile {
        h.stop(reason).await.unwrap();
    }
    release.notify_one();
    let reason = tokio::time::timeout(Duration::from_secs(10), h.wait_stopped())
        .await
        .expect("the actor ends");
    let seen = stopped_with.lock().unwrap().clone();
    PreStopEnd { reason, seen }
}

#[test]
fn a_failure_raised_while_pre_stop_runs_stays_the_reason_when_it_panics() {
    common::run(2, async {
        let standing = StopReason::Failure(ActorError::user("db down"));
        let end = pre_stop_panic_end(Payload::Text, Some(standing.clone())).await;
        assert_eq!(end.reason, standing);
        let seen = end.seen.expect("on_stopped runs");
        assert_eq!(seen.reason, standing);
        // The standing failure is the reason, and the panic is still the
        // last error the actor recorded.
        match seen.last_error {
            Some(ActorError::Panic(text)) => assert!(text.contains("pre_stop panicked"), "{text}"),
            other => panic!("the pre_stop panic is the last error, got {other:?}"),
        }
    });
}

#[test]
fn a_panic_of_pre_stop_is_the_reason_when_no_failure_stands() {
    common::run(2, async {
        for meanwhile in [
            None,
            Some(StopReason::Cancelled),
            Some(StopReason::ParentKilled),
        ] {
            let end = pre_stop_panic_end(Payload::Text, meanwhile.clone()).await;
            let error = match &end.reason {
                StopReason::Failure(error @ ActorError::Panic(text)) => {
                    assert!(text.contains("pre_stop panicked"), "{meanwhile:?}: {text}");
                    error.clone()
                }
                other => panic!("{meanwhile:?}: the panic is the reason, got {other:?}"),
            };
            let seen = end.seen.expect("on_stopped runs");
            assert_eq!(seen.reason, end.reason, "{meanwhile:?}");
            // The last error is the reason's own error.
            assert_eq!(seen.last_error, Some(error), "{meanwhile:?}");
        }
    });
}

#[test]
fn a_kill_raised_while_pre_stop_runs_is_the_reason_when_it_panics_with_any_payload() {
    common::run(2, async {
        let mut wrong = Vec::new();
        for payload in [Payload::Text, Payload::DropPanics, Payload::DropPanicsTwice] {
            let end = pre_stop_panic_end(payload, Some(StopReason::Kill)).await;
            if end.reason != StopReason::Kill || end.seen.is_some() {
                wrong.push(format!(
                    "{payload:?}: ended as {:?}, on_stopped ran: {}",
                    end.reason,
                    end.seen.is_some()
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "a Kill is the reason and on_stopped never runs: {wrong:#?}"
        );
    });
}
