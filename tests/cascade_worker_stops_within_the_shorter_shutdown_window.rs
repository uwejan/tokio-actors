//! A trapping worker under a one-child supervisor is stopped in order when the
//! supervisor above them is killed: the one-child supervisor is raised
//! `ParentKilled` and stops the worker within the shorter of its own
//! `Shutdown` window and the worker's.

mod common;

use std::time::Duration;

use tokio::time::{timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, ActorSystem, RestartType, Shutdown, StopReason,
};

/// How far past its exact instant an outcome may land on the paused clock:
/// one poll step and the timers' rounding, far short of the windows below.
const PAUSED_SLACK: Duration = Duration::from_millis(30);

const HANG_BOUND: Duration = Duration::from_secs(600);

/// A node that spawns the next node of `chain` as its child, each with the
/// `Shutdown` the chain gives it. A node with no child below it is the worker:
/// it declines every graceful stop.
struct Node {
    chain: Vec<(&'static str, Shutdown)>,
}

impl Actor for Node {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(((name, shutdown), rest)) = self.chain.split_first() {
            let rest = rest.to_vec();
            let mut config = ActorConfig::default();
            if !rest.is_empty() {
                config = config.supervisor();
            }
            ctx.spawn_child(move || Node {
                chain: rest.clone(),
            })
            .named(*name)
            .shutdown(*shutdown)
            .restart_type(RestartType::Permanent)
            .with_config(config)
            .await?;
        }
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        !self.chain.is_empty()
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Kills the root of "root -> wrapper (wrapper_window) -> worker
/// (worker_window)" and returns how long the worker took to end.
async fn time_to_stop_the_worker(wrapper_window: Duration, worker_window: Duration) -> Duration {
    let sys = ActorSystem::create(format!("shorter-window-{}", uuid::Uuid::new_v4())).unwrap();
    let root = Node {
        chain: vec![
            ("wrapper", Shutdown::Timeout(wrapper_window)),
            ("worker", Shutdown::Timeout(worker_window)),
        ],
    }
    .spawn()
    .named("root")
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();
    let worker = sys
        .get::<Node>("worker")
        .expect("the worker is registered once the tree started");

    let started = Instant::now();
    root.stop(StopReason::Kill).await.unwrap();
    timeout(HANG_BOUND, worker.wait_stopped())
        .await
        .expect("the worker ends");
    started.elapsed()
}

#[test]
fn a_worker_under_a_killed_supervisor_stops_within_the_wrapper_window_when_that_is_shorter() {
    common::run_paused(async {
        let window = Duration::from_millis(200);
        let elapsed = time_to_stop_the_worker(window, Duration::from_secs(5)).await;
        assert!(
            elapsed >= window,
            "the worker declined its stop until the window passed: {elapsed:?}"
        );
        assert!(
            elapsed < window + PAUSED_SLACK,
            "the shorter window of the wrapper bounded the stop: {elapsed:?}"
        );
    });
}

#[test]
fn a_worker_under_a_killed_supervisor_stops_within_its_own_window_when_that_is_shorter() {
    common::run_paused(async {
        let window = Duration::from_millis(200);
        let elapsed = time_to_stop_the_worker(Duration::from_secs(5), window).await;
        assert!(
            elapsed >= window,
            "the worker declined its stop until the window passed: {elapsed:?}"
        );
        assert!(
            elapsed < window + PAUSED_SLACK,
            "the shorter window of the worker bounded the stop: {elapsed:?}"
        );
    });
}
