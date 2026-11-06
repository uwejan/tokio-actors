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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, ActorStatus, ActorSystem, AskError, StopReason,
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
fn a_send_queued_before_a_failed_init_is_dropped_multi_thread() {
    common::run_multi_thread_only(2, a_send_queued_before_a_failed_init_is_dropped());
}

#[test]
fn a_send_queued_before_a_failed_init_is_dropped_current_thread() {
    common::run_paused(a_send_queued_before_a_failed_init_is_dropped());
}

// ---------------------------------------------------------------------------
// A status query during the initialization
// ---------------------------------------------------------------------------

async fn a_status_query_waits_for_pre_start_and_on_started() {
    let actor = Staged::new(false);
    let (pre_start, on_started) = (actor.pre_start.clone(), actor.on_started.clone());
    let h = actor.spawn().detached().await.expect("detached spawn");
    pre_start.reached().await;
    assert_eq!(h.status(), ActorStatus::Initializing);

    let mut query = pin!(h.get_status());
    assert!(
        stays_pending(&mut query).await,
        "answered while pre_start runs"
    );

    pre_start.open();
    on_started.reached().await;
    assert_eq!(h.status(), ActorStatus::Initializing);
    assert!(
        stays_pending(&mut query).await,
        "answered while on_started runs"
    );

    on_started.open();
    let info = timeout(BOUND, query)
        .await
        .expect("answered once the initialization finished")
        .expect("a started actor answers");
    assert_eq!(info.status, ActorStatus::Running);
}

#[test]
fn a_status_query_waits_for_pre_start_and_on_started_multi_thread() {
    common::run_multi_thread_only(2, a_status_query_waits_for_pre_start_and_on_started());
}

#[test]
fn a_status_query_waits_for_pre_start_and_on_started_current_thread() {
    common::run_paused(a_status_query_waits_for_pre_start_and_on_started());
}

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
fn a_status_query_of_a_failed_init_is_an_error_multi_thread() {
    common::run_multi_thread_only(2, a_status_query_of_a_failed_init_is_an_error());
}

#[test]
fn a_status_query_of_a_failed_init_is_an_error_current_thread() {
    common::run_paused(a_status_query_of_a_failed_init_is_an_error());
}

// ---------------------------------------------------------------------------
// An ordered initial start
// ---------------------------------------------------------------------------

/// Spawns two children in `on_started`, awaiting the first child's
/// `get_status()` before it spawns the second.
struct Starter {
    first: Stage,
    second_started: Arc<AtomicBool>,
    second: Stage,
}

impl Actor for Starter {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let stage = self.first.clone();
        let first = ctx
            .spawn_child(move || Child {
                stage: stage.clone(),
                started: None,
            })
            .named("first")
            .await?;
        first
            .get_status()
            .await
            .map_err(|e| ActorError::user(e.to_string()))?;

        let (stage, started) = (self.second.clone(), self.second_started.clone());
        let second = ctx
            .spawn_child(move || Child {
                stage: stage.clone(),
                started: Some(started.clone()),
            })
            .named("second")
            .await?;
        second
            .get_status()
            .await
            .map_err(|e| ActorError::user(e.to_string()))?;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A child that holds in `pre_start` until the test lets it on.
struct Child {
    stage: Stage,
    started: Option<Arc<AtomicBool>>,
}

impl Actor for Child {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(started) = &self.started {
            started.store(true, Ordering::SeqCst);
        }
        self.stage.pass().await;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

async fn an_ordered_start_spawns_the_next_child_after_the_previous_one_started() {
    let sys = ActorSystem::create(format!("ordered-start-{}", uuid::Uuid::new_v4())).unwrap();
    let starter = Starter {
        first: Stage::default(),
        second_started: Arc::default(),
        second: Stage::default(),
    };
    let (first, second, second_started) = (
        starter.first.clone(),
        starter.second.clone(),
        starter.second_started.clone(),
    );
    let sup = starter
        .spawn()
        .on_system(&sys)
        .supervisor()
        .detached()
        .await
        .expect("detached spawn");

    first.reached().await;
    // The first child is still in `pre_start`: the supervisor waits on its
    // `get_status()` and has not spawned the second child.
    let mut second_up = pin!(second.reached());
    assert!(
        stays_pending(&mut second_up).await,
        "the second child started before the first finished its start"
    );
    assert!(!second_started.load(Ordering::SeqCst));
    assert_eq!(sup.status(), ActorStatus::Initializing);

    first.open();
    timeout(BOUND, second_up)
        .await
        .expect("the second child starts once the first finished");
    assert_eq!(
        sup.status(),
        ActorStatus::Initializing,
        "the supervisor waits on the second child as well"
    );
    second.open();
    let info = timeout(BOUND, sup.get_status())
        .await
        .expect("answered once the supervisor started")
        .expect("a started supervisor answers");
    assert_eq!(info.status, ActorStatus::Running);
    assert_eq!(info.child_count, 2);

    sup.stop(StopReason::Graceful).await.unwrap();
    timeout(BOUND, sup.wait_stopped()).await.unwrap();
}

#[test]
fn an_ordered_start_spawns_the_next_child_after_the_previous_one_started_multi_thread() {
    common::run_multi_thread_only(
        2,
        an_ordered_start_spawns_the_next_child_after_the_previous_one_started(),
    );
}

#[test]
fn an_ordered_start_spawns_the_next_child_after_the_previous_one_started_current_thread() {
    common::run_paused(an_ordered_start_spawns_the_next_child_after_the_previous_one_started());
}
