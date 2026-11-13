//! An actor id belongs to whichever actor holds it when it is used: a named
//! actor's id is its name, so a later actor that takes the name after the
//! first ended has an equal id, and `kill_by_id` reaches that later actor. An
//! anonymous actor gets a random id of its own.

mod common;

use std::future::IntoFuture;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, StopReason,
};

const BOUND: Duration = Duration::from_secs(30);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("id-reuse-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

async fn within<F: IntoFuture>(what: &str, fut: F) -> F::Output {
    match timeout(BOUND, fut.into_future()).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {BOUND:?} waiting for: {what}"),
    }
}

#[derive(Default)]
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn kill_by_id_reaches_the_actor_that_holds_the_id_when_it_is_called() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("reuse")).unwrap();
        let first = within("first spawn", Idle.spawn().named("w").on_system(&sys))
            .await
            .unwrap();
        let id = first.id().clone();
        first.stop(StopReason::Graceful).await.unwrap();
        assert_eq!(
            within("first stop", first.wait_stopped()).await,
            StopReason::Graceful
        );

        let second = within("second spawn", Idle.spawn().named("w").on_system(&sys))
            .await
            .unwrap();
        assert_eq!(
            &id,
            second.id(),
            "the later holder of the name has an equal id"
        );

        let report = within("kill_by_id", sys.kill_by_id(&id)).await;
        assert!(report.is_some(), "the id reaches the actor holding it now");
        assert_eq!(
            within("second stop", second.wait_stopped()).await,
            StopReason::Kill,
            "the later actor took the Kill"
        );
        assert_eq!(first.wait_stopped().await, StopReason::Graceful);
    });
}
