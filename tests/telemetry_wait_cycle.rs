//! Two real actors that send to each other from their handlers with no
//! timeout wait on each other for good. With wait-cycle detection on, the
//! second send to wait closes the cycle and one event reports it; neither
//! send is touched.

use tokio_actors::{ActorSystem, SystemConfig};

/// A system starts with wait-cycle detection off, and on when its
/// configuration says so.
#[test]
fn the_configuration_switch_starts_detection() {
    let rows = [
        (SystemConfig::default(), false),
        (SystemConfig::default().with_wait_cycle_detection(), true),
    ];
    for (config, want) in rows {
        let name = format!("wait-cycle-config-{}", uuid::Uuid::new_v4());
        let system = ActorSystem::create_with(name, config).unwrap();
        assert_eq!(system.wait_cycle_detection(), want);
    }
}
