//! The abort that backs a `Kill` is issued once the kill grace has passed, or
//! once the window of a `stop_within` call has passed in its place: a bounded
//! graceful stop escalates to a `Kill` at its window and aborts after the grace
//! on top of it, and a `Kill` already raised keeps the earlier deadline of the
//! grace when a longer window is asked for afterwards.

mod common;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, StopOutcome, StopReason,
};

const WINDOW: Duration = Duration::from_secs(1);
const GRACE: Duration = Duration::from_secs(2);

/// How far past its exact instant an outcome may land on the paused clock: the
/// timers' rounding, far short of the window and the grace above.
const PAUSED_SLACK: Duration = Duration::from_millis(30);

const HANG_BOUND: Duration = Duration::from_secs(600);

/// Parks in its handler at an await that only a task abort ends.
struct Hangs {
    started: Arc<Notify>,
}

impl Actor for Hangs {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.started.notify_one();
        std::future::pending::<()>().await;
        unreachable!("pending() never resolves")
    }
}

#[test]
fn a_graceful_stop_within_aborts_after_its_window_plus_the_kill_grace() {
    common::run_paused(async {
        let started = Arc::new(Notify::new());
        let actor = Hangs {
            started: started.clone(),
        }
        .spawn()
        .kill_grace(GRACE)
        .await
        .unwrap();
        actor.notify(()).await.unwrap();
        timeout(HANG_BOUND, started.notified())
            .await
            .expect("the handler parks");

        let began = Instant::now();
        let outcome = actor
            .stop_within(StopReason::Graceful, WINDOW)
            .await
            .unwrap();
        let elapsed = began.elapsed();

        assert_eq!(outcome, StopOutcome::Aborted);
        assert!(
            elapsed >= WINDOW + GRACE,
            "the abort waited the window and then the grace: {elapsed:?}"
        );
        assert!(
            elapsed < WINDOW + GRACE + PAUSED_SLACK,
            "the abort followed the grace at once: {elapsed:?}"
        );
    });
}

#[test]
fn a_kill_raised_by_name_keeps_its_grace_when_a_longer_kill_window_is_asked_for_after_it() {
    common::run_paused(async {
        let sys =
            ActorSystem::create(format!("grace-then-window-{}", uuid::Uuid::new_v4())).unwrap();
        let started = Arc::new(Notify::new());
        let actor = Hangs {
            started: started.clone(),
        }
        .spawn()
        .named("hangs")
        .kill_grace(GRACE)
        .on_system(&sys)
        .await
        .unwrap();
        actor.notify(()).await.unwrap();
        timeout(HANG_BOUND, started.notified())
            .await
            .expect("the handler parks");

        let began = Instant::now();
        sys.kill("hangs").await.unwrap();
        let outcome = actor
            .stop_within(StopReason::Kill, Duration::from_secs(3600))
            .await
            .unwrap();
        let elapsed = began.elapsed();

        assert_eq!(outcome, StopOutcome::Aborted);
        assert!(
            elapsed >= GRACE && elapsed < GRACE + PAUSED_SLACK,
            "the abort landed at the kill grace, not at the hour: {elapsed:?}"
        );
    });
}
