//! `stop_within(StopReason::Kill, d)` aborts a task that does not react after
//! `d`, whatever the actor's kill grace: for that call the window replaces the
//! grace.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, StopOutcome, StopReason,
};

const WINDOW: Duration = Duration::from_millis(50);

/// How far past its exact instant an outcome may land on the paused clock:
/// one poll step of a waiting loop and the timer granularity, far short of the
/// kill grace the test configures.
const PAUSED_SLACK: Duration = Duration::from_millis(30);

/// Parks in its handler at an await no cancellation point of the run loop
/// reaches, so only a task abort ends it.
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

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stop_within_kill_aborts_after_its_window_not_after_a_long_grace() {
    let started = Arc::new(Notify::new());
    let h = Hangs {
        started: started.clone(),
    }
    .spawn()
    .kill_grace(Duration::from_secs(3600))
    .await
    .unwrap();
    h.notify(()).await.unwrap();
    timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("the handler parks");

    let t = Instant::now();
    let outcome = h.stop_within(StopReason::Kill, WINDOW).await.unwrap();
    let elapsed = t.elapsed();

    assert_eq!(outcome, StopOutcome::Aborted);
    assert!(
        elapsed < WINDOW + PAUSED_SLACK,
        "the window set the abort, not the one-hour grace: {elapsed:?}"
    );
}
