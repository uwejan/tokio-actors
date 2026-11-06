//! The shutdown's global deadline is counted from the call's first poll, not
//! from the moment the future is created: a call that is created, left
//! unpolled for longer than its whole window and then polled still gives its
//! roots the full window before anything is forced.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, handle::ActorHandle, Actor, ActorExt},
    ActorResult, ActorStatus, ActorSystem, ShutdownPolicy, ShutdownReport, StopOutcome, StopReason,
    SystemConfig,
};

mod common;

/// The longest a wait may take on the paused clock. It only limits how long a
/// real hang takes to report.
const BOUND: Duration = Duration::from_secs(600);

/// How many times the test yields to let every task and timer that is ready
/// run, with the paused clock where it stands.
const SETTLE_YIELDS: usize = 200;

fn unique(what: &str) -> String {
    format!("{what}-{}", uuid::Uuid::new_v4())
}

/// Declines every graceful stop. A `Kill` is not declinable and it ends the
/// actor at its next turn boundary, so the actor ends exactly when the
/// shutdown forces it.
struct Declines;

impl Actor for Declines {
    type Message = ();
    type Response = ();

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A system whose stored policy gives each root `per_actor` and the whole
/// shutdown `total`.
fn system_with(what: &str, total: Duration, per_actor: Duration) -> Arc<ActorSystem> {
    ActorSystem::create_with(
        unique(what),
        SystemConfig {
            shutdown_policy: ShutdownPolicy {
                timeout: total,
                per_actor_timeout: per_actor,
            },
            ..Default::default()
        },
    )
    .unwrap()
}

/// Spawns a `Declines` root.
async fn declining_root(sys: &Arc<ActorSystem>, name: &str) -> ActorHandle<Declines> {
    Declines.spawn().named(name).on_system(sys).await.unwrap()
}

async fn settle() {
    for _ in 0..SETTLE_YIELDS {
        tokio::task::yield_now().await;
    }
}

/// Polls `call` once with a waker that does nothing.
fn poll_once(call: &mut Pin<Box<dyn Future<Output = ShutdownReport> + '_>>) {
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(call.as_mut().poll(&mut cx), Poll::Pending),
        "the call waits for its walk"
    );
}

/// Runs the scenario for a call made with `make`: created, left unpolled for
/// 30 s of the paused clock (the window is 5 s), polled once, and then watched
/// as the clock moves.
async fn the_window_counts_from_the_first_poll<'a>(
    sys: &'a Arc<ActorSystem>,
    make: impl FnOnce(&'a ActorSystem) -> Pin<Box<dyn Future<Output = ShutdownReport> + 'a>>,
) {
    let root = declining_root(sys, "declining").await;
    let mut call = make(sys);
    tokio::time::advance(Duration::from_secs(30)).await;
    poll_once(&mut call);

    // Inside the 5 s window of the first poll nothing is forced, so the root
    // that declines its stop is still there.
    tokio::time::advance(Duration::from_secs(4)).await;
    settle().await;
    assert_ne!(
        root.status(),
        ActorStatus::Stopped,
        "nothing is forced before the window of the first poll has passed"
    );

    // Past the window of the first poll the root is forced and ends.
    tokio::time::advance(Duration::from_secs(2)).await;
    timeout(BOUND, root.wait_stopped())
        .await
        .expect("the root is forced once the window of the first poll has passed");
    let report = timeout(BOUND, call)
        .await
        .expect("the call returns once its walk is done");
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(report.outcomes[0].1.outcome, StopOutcome::Killed);
}

#[test]
fn the_global_deadline_of_shutdown_with_counts_from_the_first_poll() {
    common::run_paused(async {
        let sys = system_with(
            "first-poll-deadline-with",
            Duration::from_secs(600),
            Duration::from_secs(600),
        );
        the_window_counts_from_the_first_poll(&sys, |sys| {
            Box::pin(sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(5),
                per_actor_timeout: Duration::from_secs(60),
            }))
        })
        .await;
    });
}
