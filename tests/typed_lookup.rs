//! A lookup by name or by id answers `None` when the registered actor is of
//! a different type than the one asked for, and a handle when the type
//! matches.

mod common;

use std::future::IntoFuture;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem,
};

const BOUND: Duration = Duration::from_secs(30);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "typed-lookup-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
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

#[derive(Default)]
struct Other;

impl Actor for Other {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, msg: u32, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        Ok(msg)
    }
}

#[test]
fn a_lookup_with_another_actor_type_answers_none() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("named")).unwrap();
        let named = within("spawn", Idle.spawn().named("typed").on_system(&sys))
            .await
            .unwrap();
        let anonymous = within("spawn", Idle.spawn().on_system(&sys)).await.unwrap();

        assert!(sys.get::<Idle>("typed").is_some());
        assert!(sys.get::<Other>("typed").is_none());
        assert!(sys.get_by_id::<Idle>(named.id()).is_some());
        assert!(sys.get_by_id::<Other>(named.id()).is_none());
        assert!(sys.get_by_id::<Idle>(anonymous.id()).is_some());
        assert!(sys.get_by_id::<Other>(anonymous.id()).is_none());

        // A lookup that found nothing of its type leaves the actor alone.
        assert!(sys.actor_ids().contains(named.id()));
        assert!(sys.registered().contains(&"typed".to_string()));
    });
}
