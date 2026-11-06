//! The bound of a spawn with a `start_timeout` is enforced by the system, not
//! by the caller's future: a caller that stops waiting, for any reason, leaves
//! the bound in force. The actor is aborted at its deadline, no lifecycle hook
//! runs afterward, and its name is free.

use std::future::IntoFuture;
use std::time::Duration;

use tokio::time::{timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_millis(200);

/// An actor whose initialization never ends.
#[derive(Default)]
struct NeverStarts;

impl Actor for NeverStarts {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        std::future::pending::<()>().await;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// An actor that starts at once and answers.
struct Quick;

impl Actor for Quick {
    type Message = ();
    type Response = u8;

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<u8> {
        Ok(7)
    }
}

fn unique(base: &str) -> String {
    format!("{base}-{}", uuid::Uuid::new_v4())
}

/// Waits, by observation, until the system aborted the stuck actor `name`,
/// and checks that it did so no earlier than the start timeout.
async fn assert_aborted_by_the_system(
    sys: &std::sync::Arc<ActorSystem>,
    name: &str,
    spawned_at: Instant,
) {
    let actor = sys
        .get::<NeverStarts>(name)
        .expect("the actor is registered while it initializes");
    let reason = timeout(BOUND, actor.wait_stopped())
        .await
        .expect("the actor is still initializing: the bound died with its caller");
    assert_eq!(reason, StopReason::Kill, "the system aborts the start");
    assert!(
        spawned_at.elapsed() >= START_TIMEOUT,
        "the actor was aborted before its start timeout"
    );
    assert!(sys.get::<NeverStarts>(name).is_none(), "the name is free");
    // The name is free for a new actor.
    let again = Quick.spawn().named(name).on_system(sys).await.unwrap();
    assert_eq!(again.send(()).await.unwrap(), 7);
}

#[test]
fn an_abandoned_awaited_spawn_still_meets_its_start_timeout() {
    common::run(2, async {
        let sys = ActorSystem::create(unique("abandoned-spawn")).unwrap();
        let name = unique("never-starts");
        let spawned_at = Instant::now();
        let caller = tokio::spawn(
            NeverStarts
                .spawn()
                .named(name.as_str())
                .on_system(&sys)
                .start_timeout(START_TIMEOUT)
                .into_future(),
        );
        // The caller gives up as soon as the actor is registered, long before
        // the start timeout.
        while sys.get::<NeverStarts>(&name).is_none() {
            tokio::task::yield_now().await;
        }
        caller.abort();
        match caller.await {
            Err(ended) => assert!(ended.is_cancelled()),
            Ok(_) => panic!("the abandoned caller must not finish"),
        }

        assert_aborted_by_the_system(&sys, &name, spawned_at).await;
    });
}
