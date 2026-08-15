//! Behavioral suite for the system visibility surface: `actor_ids`,
//! `actor_status`, and `kill_by_id`.
//!
//! Every actor is enumerable, statusable, and killable through its system,
//! independent of any handle the caller may still be holding - parity with
//! `erlang:processes/0` (enumeration) and `erlang:exit(Pid, kill)`
//! (untrappable kill), erlang.org/docs/28.
//!
//! Per the process-global default-system test discipline: assertions
//! against `ActorSystem::default()` are membership-only, never emptiness or
//! exact-set equality, since `cargo test` runs concurrently in one process
//! and the default system is a singleton shared by every test in the crate.

use std::time::{Duration, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, ActorSystem, StopReason,
};

#[derive(Default)]
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Polls a synchronous predicate until it holds or the deadline passes.
async fn wait_until<F>(timeout_ms: u64, mut pred: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// Zombie end-to-end: a bare spawn with its only external handle dropped is
// still enumerable, statusable, and killable through its system, and any
// handle obtained before the drop still observes the kill.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zombie_actor_visible_and_killable() {
    let handle = Idle.spawn().await.expect("spawn succeeds");
    let id = handle.id().clone();

    let sys = ActorSystem::default();
    let observer = sys
        .get_by_id::<Idle>(&id)
        .expect("registered before the drop");

    drop(handle);

    assert!(
        sys.actor_ids().contains(&id),
        "a zombie actor stays enumerable through its system"
    );
    assert_eq!(sys.actor_status(&id), Some(ActorStatus::Running));

    assert!(
        sys.kill_by_id(&id).await,
        "kill_by_id finds the registered zombie"
    );

    tokio::time::timeout(Duration::from_secs(5), observer.wait_stopped())
        .await
        .expect("kill_by_id stops the zombie within the timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actor_status_none_for_unknown_id() {
    let sys = ActorSystem::default();
    let unknown = tokio_actors::ActorId::from(format!("unknown-{}", uuid::Uuid::new_v4()));
    assert_eq!(sys.actor_status(&unknown), None);
    assert!(!sys.kill_by_id(&unknown).await);
}

// A gracefully-stopped actor's id leaves `actor_ids()` once its
// `RegistryGuard` drops - no leftover entry for a clean stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_actor_id_leaves_actor_ids() {
    let handle = Idle.spawn().await.expect("spawn succeeds");
    let id = handle.id().clone();

    let sys = ActorSystem::default();
    assert!(sys.actor_ids().contains(&id));

    handle
        .stop(StopReason::Graceful)
        .await
        .expect("stop request delivered");
    handle.wait_stopped().await;

    let pruned = wait_until(1_000, || !sys.actor_ids().contains(&id)).await;
    assert!(pruned, "stopped actor's id must leave actor_ids()");
}
