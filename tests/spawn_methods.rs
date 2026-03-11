use std::sync::Arc;

use tokio_actors::actor::context::ActorContext;
use tokio_actors::actor::{Actor, ActorExt};
use tokio_actors::error::SpawnError;
use tokio_actors::system::ActorSystem;
use tokio_actors::{ActorConfig, ActorResult};

#[derive(Debug, Default)]
struct Ping;

impl Actor for Ping {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// spawn() -- anonymous, auto UUID id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_anonymous() {
    let handle = Ping.spawn().await.unwrap();
    assert!(handle.is_alive());
    let id = handle.id().as_str();
    assert_eq!(id.len(), 36, "UUID should be 36 chars: {id}");
}

/// spawn().named("x") -- name == id, registered in default system.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_named_registers_in_default() {
    let name = format!("named-{}", uuid::Uuid::new_v4());
    let handle = Ping.spawn().named(&name).await.unwrap();

    assert!(handle.is_alive());
    assert_eq!(handle.id().as_str(), name);

    let sys = ActorSystem::default();
    let found = sys.get::<Ping>(&name);
    assert!(found.is_some(), "actor should be in default system");
}

/// spawn().named("x").with_config(config) -- name + custom config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_named_with_custom_config() {
    let name = format!("named-with-{}", uuid::Uuid::new_v4());
    let config = ActorConfig::default().with_mailbox_capacity(128);
    let handle = Ping.spawn().named(&name).with_config(config).await.unwrap();

    assert!(handle.is_alive());
    assert_eq!(handle.mailbox_capacity(), 128);
    assert_eq!(handle.id().as_str(), name);
}

/// spawn().on_system(&system) -- anonymous in specific system.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_on_specific_system() {
    let sys_name = format!("spawn-on-{}", uuid::Uuid::new_v4());
    let sys = ActorSystem::create(&sys_name).unwrap();

    let handle = Ping.spawn().on_system(&sys).await.unwrap();
    assert!(handle.is_alive());
    let id_str = handle.id().as_str();
    assert_eq!(id_str.len(), 36); // UUID
}

/// spawn().named("x").on_system(&system) -- named in specific system.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_on_named_in_system() {
    let sys_name = format!("on-named-{}", uuid::Uuid::new_v4());
    let sys = ActorSystem::create(&sys_name).unwrap();

    let handle = Ping.spawn().named("target").on_system(&sys).await.unwrap();
    assert!(handle.is_alive());
    assert_eq!(handle.id().as_str(), "target");

    let found = sys.get::<Ping>("target");
    assert!(found.is_some());
}

/// spawn().named("x").on_system(&system).with_config(config) -- full params.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_on_named_with_full_params() {
    let sys_name = format!("on-named-with-{}", uuid::Uuid::new_v4());
    let sys = ActorSystem::create(&sys_name).unwrap();
    let config = ActorConfig::default().with_mailbox_capacity(256);

    let handle = Ping
        .spawn()
        .named("full")
        .on_system(&sys)
        .with_config(config)
        .await
        .unwrap();
    assert!(handle.is_alive());
    assert_eq!(handle.mailbox_capacity(), 256);

    let found = sys.get::<Ping>("full");
    assert!(found.is_some());
}

/// Name collision returns NameTaken error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_named_collision() {
    let name = format!("collision-{}", uuid::Uuid::new_v4());
    let _h1 = Ping.spawn().named(&name).await.unwrap();
    let result = Ping.spawn().named(&name).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        SpawnError::NameTaken { name: n, .. } => assert_eq!(n, name),
        other => panic!("expected NameTaken, got: {other}"),
    }
}

/// stop/kill via system on named actors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_stop_named_actor() {
    let sys_name = format!("stop-sys-{}", uuid::Uuid::new_v4());
    let sys = ActorSystem::create(&sys_name).unwrap();

    let handle = Ping.spawn().named("stopper").on_system(&sys).await.unwrap();
    assert!(sys.get::<Ping>("stopper").is_some());

    sys.stop("stopper").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!handle.is_alive(), "actor should be stopped");
}

/// kill via system on named actors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_kill_named_actor() {
    let sys_name = format!("kill-sys-{}", uuid::Uuid::new_v4());
    let sys = ActorSystem::create(&sys_name).unwrap();

    let handle = Ping
        .spawn()
        .named("killable")
        .on_system(&sys)
        .await
        .unwrap();
    sys.kill("killable").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!handle.is_alive());
}

/// Concurrent spawn_named with same name -- one succeeds, one fails (TOCTOU test).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_spawn_named_no_race() {
    let sys_name = format!("toctou-{}", uuid::Uuid::new_v4());
    let sys = ActorSystem::create(&sys_name).unwrap();

    let sys1 = Arc::clone(&sys);
    let sys2 = Arc::clone(&sys);

    let (r1, r2) = tokio::join!(
        async move { Ping.spawn().named("contested").on_system(&sys1).await },
        async move { Ping.spawn().named("contested").on_system(&sys2).await },
    );

    let (success_count, failure_count) = match (&r1, &r2) {
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => (1, 1),
        _ => panic!("expected exactly one success and one failure, got: {r1:?} / {r2:?}"),
    };

    assert_eq!(success_count, 1);
    assert_eq!(failure_count, 1);
}
