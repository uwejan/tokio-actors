use tokio_actors::error::{ActorError, SpawnError};

#[test]
fn name_taken_error_includes_system() {
    let err = SpawnError::NameTaken {
        name: "counter".into(),
        system: "default".into(),
    };
    let msg = err.to_string();
    assert!(msg.contains("counter"), "error should mention the name");
    assert!(msg.contains("default"), "error should mention the system");
}

#[test]
fn actor_error_spawn_preserves_type() {
    let spawn_err = SpawnError::NameTaken {
        name: "x".into(),
        system: "test".into(),
    };
    let actor_err: ActorError = spawn_err.into();
    match actor_err {
        ActorError::Spawn(_) => {}
        other => panic!("expected ActorError::Spawn, got: {other}"),
    }
}

#[test]
fn system_name_taken_is_visible() {
    let err = SpawnError::SystemNameTaken("prod".into());
    assert!(err.to_string().contains("prod"));
}
