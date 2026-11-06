#![allow(dead_code)]
//! A stop request of the same severity that lands while `pre_stop` runs is
//! guaranteed a fresh `pre_stop` invocation unless the one in flight accepts
//! or panics: a veto is asked again, a panic ends the stop as a failure with
//! no further invocation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
enum First {
    Panic,
    Veto,
}

/// Its first `pre_stop` waits at a gate and then panics or vetoes; every later
/// one accepts.
struct Gated {
    first: First,
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    stopped_with: Arc<Mutex<Option<StopReason>>>,
}

impl Actor for Gated {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
            return true;
        }
        self.entered.notify_one();
        self.release.notified().await;
        match self.first {
            First::Panic => panic!("pre_stop panicked"),
            First::Veto => false,
        }
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.stopped_with.lock().unwrap() = Some(reason.clone());
        Ok(())
    }
}

/// Raises `Graceful`, raises `Graceful` again while the first `pre_stop` is at
/// its gate, lets it end as `first` says, and returns the number of
/// invocations, the exit reason and the reason `on_stopped` received.
async fn same_severity_retry(first: First) -> (usize, StopReason, Option<StopReason>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let stopped_with = Arc::new(Mutex::new(None));
    let h = Gated {
        first,
        calls: calls.clone(),
        entered: entered.clone(),
        release: release.clone(),
        stopped_with: stopped_with.clone(),
    }
    .spawn()
    .await
    .unwrap();
    h.stop(StopReason::Graceful).await.unwrap();
    timeout(BOUND, entered.notified())
        .await
        .expect("pre_stop starts");
    h.stop(StopReason::Graceful)
        .await
        .expect("a request of the same severity lands");
    release.notify_one();
    let reason = timeout(BOUND, h.wait_stopped())
        .await
        .expect("the actor ends");
    let seen = stopped_with.lock().unwrap().clone();
    (calls.load(Ordering::SeqCst), reason, seen)
}

#[test]
fn a_same_severity_retry_while_pre_stop_panics_is_not_asked_again() {
    common::run(2, async {
        let (calls, reason, seen) = same_severity_retry(First::Panic).await;
        assert_eq!(calls, 1, "pre_stop was asked again after it panicked");
        match &reason {
            StopReason::Failure(ActorError::Panic(text)) => {
                assert!(text.contains("pre_stop panicked"), "{text}")
            }
            other => panic!("the panic is the reason, got {other:?}"),
        }
        assert_eq!(seen, Some(reason));
    });
}
