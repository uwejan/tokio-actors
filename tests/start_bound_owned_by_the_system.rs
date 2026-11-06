//! The bound of a spawn with a `start_timeout` is enforced by the system, not
//! by the caller's future: a caller that stops waiting, for any reason, leaves
//! the bound in force. The actor is aborted at its deadline, no lifecycle hook
//! runs afterward, and its name is free.

use std::future::IntoFuture;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::time::{advance, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, ActorSystem, SpawnError, StopReason,
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

/// An actor whose initialization fails.
struct FailsToStart;

impl Actor for FailsToStart {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("no start"))
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
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

#[test]
fn a_spawn_future_never_polled_again_still_meets_its_start_timeout() {
    common::run(2, async {
        let sys = ActorSystem::create(unique("stored-spawn")).unwrap();
        let name = unique("never-starts");
        let spawned_at = Instant::now();
        let mut stored = NeverStarts
            .spawn()
            .named(name.as_str())
            .on_system(&sys)
            .start_timeout(START_TIMEOUT)
            .into_future();
        // One poll starts the actor; nothing polls the future again, and it
        // stays alive until the end of the test.
        let first = stored
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(first, Poll::Pending));

        assert_aborted_by_the_system(&sys, &name, spawned_at).await;
        drop(stored);
    });
}

#[test]
fn a_spawn_that_starts_in_time_keeps_running_past_its_deadline() {
    common::run_paused(async {
        let sys = ActorSystem::create(unique("started-in-time")).unwrap();
        let actor = Quick
            .spawn()
            .named(unique("quick"))
            .on_system(&sys)
            .start_timeout(START_TIMEOUT)
            .await
            .unwrap();
        advance(START_TIMEOUT * 10).await;
        assert!(actor.is_alive(), "the bound ends with the start");
        assert_eq!(actor.send(()).await.unwrap(), 7);
    });
}

#[test]
fn a_failed_start_under_a_start_timeout_reads_as_an_init_error() {
    common::run(2, async {
        let sys = ActorSystem::create(unique("failed-start")).unwrap();
        let name = unique("fails");
        let answer = FailsToStart
            .spawn()
            .named(name.as_str())
            .on_system(&sys)
            .start_timeout(Duration::from_secs(30))
            .await;
        assert!(matches!(answer, Err(SpawnError::Init(_))));
        assert!(sys.get::<FailsToStart>(&name).is_none());
    });
}
