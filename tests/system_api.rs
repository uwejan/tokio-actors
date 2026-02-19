use std::time::Duration;

use tokio_actors::system::{ActorSystem, ShutdownPolicy, SystemConfig};

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
    assert_eq!(config.shutdown_policy.per_actor_timeout, Duration::from_secs(5));
}
