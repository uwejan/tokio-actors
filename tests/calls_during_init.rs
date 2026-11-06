//! What a caller gets from an actor that has not finished its initialization.
//!
//! - A call queued while `pre_start` runs is dropped unhandled when the
//!   initialization fails: its `send` returns `AskError::ResponseDropped`, a
//!   later call returns `Closed`, and `wait_stopped()` reports the end.
//! - `get_status()` is answered only after `pre_start` and `on_started`; for a
//!   failed initialization it returns an `AskError`, never a status. An
//!   ordered initial start therefore spawns each child only after the previous
//!   one finished its initialization.

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, AskError, StopReason,
};

mod common;

/// Long enough that a real hang fails the test, never a condition's wait.
const BOUND: Duration = Duration::from_secs(60);

/// How long an answer that must not come is waited for. The actor under test
/// is parked at a gate only the test opens, so an answer inside the window
/// could only come from the crate answering during the initialization.
const QUIET: Duration = Duration::from_millis(150);

/// Waits at a gate `pre_start` and `on_started` each own, and tells the test
/// when it is there.
#[derive(Clone, Default)]
struct Stage {
    entered: Arc<Notify>,
    gate: Arc<Notify>,
}

impl Stage {
    async fn pass(&self) {
        self.entered.notify_one();
        self.gate.notified().await;
    }

    async fn reached(&self) {
        timeout(BOUND, self.entered.notified())
            .await
            .expect("the actor reaches the stage");
    }

    fn open(&self) {
        self.gate.notify_one();
    }
}

/// An actor that holds in `pre_start` and in `on_started` until the test lets
/// it on, and whose `pre_start` can fail.
struct Staged {
    pre_start: Stage,
    on_started: Stage,
    fail: bool,
    handled: Arc<AtomicUsize>,
}

impl Staged {
    fn new(fail: bool) -> Self {
        Self {
            pre_start: Stage::default(),
            on_started: Stage::default(),
            fail,
            handled: Arc::default(),
        }
    }
}

impl Actor for Staged {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.pre_start.pass().await;
        if self.fail {
            return Err(ActorError::user("init failed"));
        }
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.on_started.pass().await;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.handled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Polls `call` for the quiet window and reports whether it stayed pending.
async fn stays_pending<F: Future + Unpin>(call: &mut F) -> bool {
    timeout(QUIET, call).await.is_err()
}

// ---------------------------------------------------------------------------
// A call queued before a failed initialization
// ---------------------------------------------------------------------------

async fn a_send_queued_before_a_failed_init_is_dropped() {
    let actor = Staged::new(true);
    let (pre_start, handled) = (actor.pre_start.clone(), actor.handled.clone());
    let h = actor.spawn().detached().await.expect("detached spawn");
    pre_start.reached().await;

    let call = tokio::spawn({
        let h = h.clone();
        async move { h.send(()).await }
    });
    let deadline = tokio::time::Instant::now() + BOUND;
    while h.mailbox_len() == 0 {
        assert!(tokio::time::Instant::now() < deadline, "the call is queued");
        tokio::task::yield_now().await;
    }

    pre_start.open();
    let answer = timeout(BOUND, call)
        .await
        .expect("the queued call is answered once the initialization failed")
        .expect("the caller ends");
    assert!(
        matches!(
            answer,
            Err(AskError::ResponseDropped {
                reason: StopReason::Failure(_)
            })
        ),
        "{answer:?}"
    );
    let reason = timeout(BOUND, h.wait_stopped())
        .await
        .expect("the end is reported");
    assert!(matches!(reason, StopReason::Failure(_)), "{reason:?}");
    assert_eq!(
        handled.load(Ordering::SeqCst),
        0,
        "the queued call was dropped without running"
    );
    let later = timeout(BOUND, h.send(())).await.expect("answered");
    assert!(matches!(later, Err(AskError::Closed)), "{later:?}");
}

#[test]
fn a_send_queued_before_a_failed_init_is_dropped_current_thread() {
    common::run_paused(a_send_queued_before_a_failed_init_is_dropped());
}

// ---------------------------------------------------------------------------
// A status query during the initialization
// ---------------------------------------------------------------------------

async fn a_status_query_of_a_failed_init_is_an_error() {
    let actor = Staged::new(true);
    let pre_start = actor.pre_start.clone();
    let h = actor.spawn().detached().await.expect("detached spawn");
    pre_start.reached().await;

    let mut query = pin!(h.get_status());
    assert!(
        stays_pending(&mut query).await,
        "answered while pre_start runs"
    );

    pre_start.open();
    let answer = timeout(BOUND, query)
        .await
        .expect("the query is answered once the initialization failed");
    assert!(
        matches!(
            answer,
            Err(AskError::ResponseDropped {
                reason: StopReason::Failure(_)
            })
        ),
        "{answer:?}"
    );
}

#[test]
fn a_status_query_of_a_failed_init_is_an_error_current_thread() {
    common::run_paused(a_status_query_of_a_failed_init_is_an_error());
}

// ---------------------------------------------------------------------------
// An ordered initial start
// ---------------------------------------------------------------------------
