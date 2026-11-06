//! The process map of named systems keeps a system after its shutdown: it is
//! still found by name and listed, and its name stays taken.

use tokio_actors::{ActorSystem, SpawnError};

#[tokio::test]
async fn a_system_stays_in_the_map_after_its_shutdown() {
    let name = format!("kept-after-shutdown-{}", uuid::Uuid::new_v4());
    let sys = ActorSystem::create(name.clone()).unwrap();
    sys.shutdown().await;

    assert!(ActorSystem::get_named(&name).is_some());
    assert!(ActorSystem::all().contains(&name));
    assert!(matches!(
        ActorSystem::create(name),
        Err(SpawnError::SystemNameTaken(_))
    ));
}
