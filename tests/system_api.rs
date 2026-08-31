use std::time::Duration;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    system::{ActorSystem, ShutdownPolicy, SystemConfig},
    ActorResult, SendError, Shutdown, StopReason,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_with_custom_config() {
    let sys = ActorSystem::create_with(
        format!("cw-{}", uuid::Uuid::new_v4()),
        SystemConfig {
            shutdown_policy: ShutdownPolicy {
                timeout: Duration::from_secs(1),
                per_actor_timeout: Duration::from_millis(500),
            },
        },
    )
    .unwrap();
    assert!(sys.name().starts_with("cw-"));
}

#[test]
fn shutdown_policy_has_per_actor_timeout() {
    let policy = ShutdownPolicy {
        timeout: Duration::from_secs(30),
        per_actor_timeout: Duration::from_secs(5),
    };
    assert_eq!(policy.per_actor_timeout, Duration::from_secs(5));
}

#[test]
fn system_config_default() {
    let config = SystemConfig::default();
    assert_eq!(config.shutdown_policy.timeout, Duration::from_secs(30));
    assert_eq!(
        config.shutdown_policy.per_actor_timeout,
        Duration::from_secs(5)
    );
}

// ---------------------------------------------------------------------------
// ActorSystem::stop/kill: NotFound vs Closed
// ---------------------------------------------------------------------------

/// Vetoes every stoppable stop forever. Paired with a parent that shuts it
/// down under `Shutdown::Infinity`, this keeps the parent's own child-stop
/// wait open indefinitely - the deterministic window the suite below needs:
/// a parent whose OWN stop lane has already closed (closing is the very
/// first step of its teardown, before it ever signals a child) but whose
/// `RegistryGuard` has not dropped yet, because it is still waiting on this
/// never-dying child.
#[derive(Default)]
struct Vetoer;

impl Actor for Vetoer {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }
}

/// A supervisor that spawns one `Vetoer` child under `Shutdown::Infinity`.
#[derive(Default)]
struct Sup;

impl Actor for Sup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(Vetoer::default)
            .shutdown(Shutdown::Infinity)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_and_kill_unknown_name_return_not_found() {
    let sys = ActorSystem::create(format!("api-nf-{}", uuid::Uuid::new_v4())).unwrap();

    let err = sys.stop("does-not-exist").await.unwrap_err();
    assert!(matches!(err, SendError::NotFound), "got {err:?}");

    let err = sys.kill("does-not-exist").await.unwrap_err();
    assert!(matches!(err, SendError::NotFound), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_known_but_already_dead_actor_returns_closed() {
    let sys = ActorSystem::create(format!("api-closed-{}", uuid::Uuid::new_v4())).unwrap();

    Sup.spawn()
        .named("api-closed-sup")
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();

    // See the `Vetoer`/`Sup` docs above: a graceful stop closes the
    // supervisor's own stop lane almost immediately (before it ever signals
    // the child), while its registry entry (by_name) stays present
    // indefinitely because the `Vetoer` never dies.
    sys.stop("api-closed-sup").await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match sys.stop("api-closed-sup").await {
            Err(SendError::Closed) => return,
            Err(SendError::NotFound) => {
                panic!("the supervisor's registry entry must still be present")
            }
            Ok(()) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the supervisor's system channel must close soon after Kill"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}
