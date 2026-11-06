//! Every actor is enumerable and killable through its system, however it was
//! spawned: a named actor, an anonymous one, a detached one, a supervisor and
//! a supervised child all appear in `actor_ids()`, report a status through
//! `actor_status`, and take a `Kill` from `kill_by_id`.

mod common;

use std::future::IntoFuture;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorId, ActorResult, ActorSystem, StopReason,
};

const BOUND: Duration = Duration::from_secs(30);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "spawn-paths-listed-{base}-{}",
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

/// Spawns one child when it starts and publishes the child's id.
struct Boss {
    child_id: Arc<OnceLock<ActorId>>,
}

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let child = ctx.spawn_child(Idle::default).await?;
        let _ = self.child_id.set(child.id().clone());
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn every_spawn_path_is_enumerable_and_killable() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("paths")).unwrap();
        let named = within("named", Idle.spawn().named("n").on_system(&sys))
            .await
            .unwrap();
        let anonymous = within("anonymous", Idle.spawn().on_system(&sys))
            .await
            .unwrap();
        let detached = within("detached", Idle.spawn().detached().on_system(&sys))
            .await
            .unwrap();
        let child_id = Arc::new(OnceLock::new());
        let boss = within(
            "supervisor",
            Boss {
                child_id: child_id.clone(),
            }
            .spawn()
            .named("boss")
            .on_system(&sys)
            .supervisor(),
        )
        .await
        .unwrap();
        let child = child_id
            .get()
            .expect("the supervisor published its child")
            .clone();

        let ids = [
            named.id().clone(),
            anonymous.id().clone(),
            detached.id().clone(),
            boss.id().clone(),
            child.clone(),
        ];
        let listed = sys.actor_ids();
        for id in &ids {
            assert!(listed.contains(id), "{id} is enumerable");
            assert!(sys.actor_status(id).is_some(), "{id} reports a status");
        }

        // The children go before the supervisor so that no restart muddies
        // the last check.
        for id in ids.iter().rev() {
            assert!(
                within("kill_by_id", sys.kill_by_id(id)).await.is_some(),
                "{id} is killable by id"
            );
        }
        for handle in [&named, &detached] {
            assert_eq!(
                within("stop", handle.wait_stopped()).await,
                StopReason::Kill
            );
        }
        assert_eq!(
            within("stop", anonymous.wait_stopped()).await,
            StopReason::Kill
        );
        let reason = within("stop", boss.wait_stopped()).await;
        assert!(
            matches!(reason, StopReason::Kill),
            "the supervisor ended by the Kill, got {reason:?}"
        );
    });
}
