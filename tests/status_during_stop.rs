//! Status requests racing a stop, on many idle actors: every requester is
//! answered, either with the actor's status or with an error that says the
//! actor ended. None waits.
use std::time::Duration;

use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, StopReason,
};

const BOUND: Duration = Duration::from_secs(5);

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();
    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Spawns `actors` idle actors, races two status requests and a stop on each,
/// and returns how many requesters did not resolve within the bound.
async fn round(actors: usize) -> usize {
    let mut tasks = Vec::new();
    for i in 0..actors {
        let handle = Idle.spawn().await.unwrap();
        let (first, second, stopper) = (handle.clone(), handle.clone(), handle.clone());
        let a = tokio::spawn(async move { first.get_status().await.is_ok() });
        let b = tokio::spawn(async move { second.get_status().await.is_ok() });
        let c = tokio::spawn(async move {
            for _ in 0..(i % 4) {
                tokio::task::yield_now().await;
            }
            let _ = stopper.stop(StopReason::Graceful).await;
        });
        tasks.push((a, b, c, handle));
    }
    let mut waiting = 0;
    for (a, b, c, _handle) in tasks {
        waiting += usize::from(timeout(BOUND, a).await.is_err());
        waiting += usize::from(timeout(BOUND, b).await.is_err());
        let _ = timeout(BOUND, c).await;
    }
    waiting
}

async fn run(rounds: usize, actors: usize) {
    let mut waiting = 0;
    for _ in 0..rounds {
        waiting += round(actors).await;
    }
    assert_eq!(waiting, 0, "status requests that never resolved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_requests_racing_a_stop_are_answered_multi_thread() {
    run(10, 1000).await;
}

#[tokio::test(flavor = "current_thread")]
async fn status_requests_racing_a_stop_are_answered_current_thread() {
    run(10, 1000).await;
}
