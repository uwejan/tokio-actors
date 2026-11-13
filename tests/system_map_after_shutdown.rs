//! The process map of named systems after a shutdown: a system that has
//! completed a shutdown stays in the map exactly as long as someone holds it
//! (its user, or an actor of it that has not ended), and a system that has not
//! completed one stays until it does, whatever its user does. A handle never
//! mistakes a later system of the same name for its own.

use std::collections::HashSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::time::{sleep, timeout};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, SpawnError, StopReason,
};

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn unique(base: &str) -> String {
    format!("{base}-{}", uuid::Uuid::new_v4())
}

#[tokio::test]
async fn a_system_stays_in_the_map_after_its_shutdown() {
    let name = unique("kept-after-shutdown");
    let sys = ActorSystem::create(name.clone()).unwrap();
    sys.shutdown().await;

    assert!(ActorSystem::get_named(&name).is_some());
    assert!(ActorSystem::all().contains(&name));
    assert!(matches!(
        ActorSystem::create(name),
        Err(SpawnError::SystemNameTaken(_))
    ));
}

/// Waits until every holder of the system has let go. Used where an actor or
/// a stale handle of the system may still be winding down.
async fn wait_gone(weak: &Weak<ActorSystem>) {
    for _ in 0..250 {
        if weak.strong_count() == 0 {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("the system was not freed after its last holder let go");
}

/// What a row does to a system before its user lets go of it.
#[derive(Clone, Copy, Debug)]
enum Row {
    /// Created and dropped; no shutdown.
    ActiveDropped,
    /// Shut down; the user keeps it.
    ShutDownHeld,
    /// Shut down twice; the user keeps it.
    ShutDownTwiceHeld,
    /// Shut down, reactivated and dropped.
    ReactivatedDropped,
    /// Shut down and dropped.
    ShutDownDropped,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_map_keeps_a_system_exactly_while_it_must() {
    // (row, the map still has the system once the user has let go or kept it)
    let rows = [
        (Row::ActiveDropped, true),
        (Row::ShutDownHeld, true),
        (Row::ShutDownTwiceHeld, true),
        (Row::ReactivatedDropped, true),
        (Row::ShutDownDropped, false),
    ];
    for (row, kept) in rows {
        let name = unique("map-row");
        let sys = ActorSystem::create(name.clone()).unwrap();
        let weak = Arc::downgrade(&sys);
        let held = match row {
            Row::ActiveDropped => {
                drop(sys);
                None
            }
            Row::ShutDownHeld => {
                sys.shutdown().await;
                Some(sys)
            }
            Row::ShutDownTwiceHeld => {
                sys.shutdown().await;
                let again = timeout(Duration::from_secs(10), sys.shutdown())
                    .await
                    .expect("a second shutdown of a shut-down system ends");
                assert!(again.outcomes.is_empty());
                Some(sys)
            }
            Row::ReactivatedDropped => {
                sys.shutdown().await;
                assert!(sys.reactivate());
                drop(sys);
                None
            }
            Row::ShutDownDropped => {
                sys.shutdown().await;
                drop(sys);
                None
            }
        };

        if !kept {
            // A shutdown's walk lets go of the system before the call
            // returns, so the user's drop is the last one and nothing waits.
            assert_eq!(weak.strong_count(), 0, "freed at the drop, row {row:?}");
        }
        assert_eq!(
            ActorSystem::get_named(&name).is_some(),
            kept,
            "found, row {row:?}"
        );
        assert_eq!(
            ActorSystem::all().contains(&name),
            kept,
            "listed, row {row:?}"
        );
        let taken = match ActorSystem::create(name.clone()) {
            Ok(made) => {
                made.shutdown().await;
                false
            }
            Err(SpawnError::SystemNameTaken(_)) => true,
            Err(other) => panic!("unexpected refusal: {other}"),
        };
        assert_eq!(taken, kept, "name taken, row {row:?}");
        if matches!(row, Row::ReactivatedDropped) {
            // Active again, so it takes work, found by name.
            let found = ActorSystem::get_named(&name).unwrap();
            let actor = Idle.spawn().named("after").on_system(&found).await.unwrap();
            actor.send(()).await.unwrap();
        }
        drop(held);
    }
}

// A handle's `Hash` and `Eq` read only its id and its system's identity, which
// never change, so the interior mutability of its channels is not part of the key.
#[allow(clippy::mutable_key_type)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handle_never_mistakes_a_later_system_of_its_name_for_its_own() {
    let name = unique("handle-identity");
    let first = ActorSystem::create(name.clone()).unwrap();
    let old = Idle
        .spawn()
        .named("worker")
        .on_system(&first)
        .await
        .unwrap();
    first.shutdown().await;
    let weak = Arc::downgrade(&first);
    drop(first);
    wait_gone(&weak).await;
    assert!(ActorSystem::get_named(&name).is_none());

    let second = ActorSystem::create(name.clone()).unwrap();
    let new = Idle
        .spawn()
        .named("worker")
        .on_system(&second)
        .await
        .unwrap();

    assert_eq!(old, old.clone());
    assert_ne!(old, new, "same id and same system name, different systems");
    let both: HashSet<_> = [old.clone(), new.clone()].into_iter().collect();
    assert_eq!(both.len(), 2);

    // A `Kill` through the stale handle is refused at once and touches nothing
    // of the later system.
    let refused = timeout(Duration::from_secs(5), old.stop(StopReason::Kill))
        .await
        .expect("the stale Kill answers");
    assert!(refused.is_err());
    let bounded = timeout(
        Duration::from_secs(5),
        old.stop_within(StopReason::Kill, Duration::from_millis(50)),
    )
    .await
    .expect("the stale bounded Kill answers");
    assert!(bounded.is_err());
    new.send(())
        .await
        .expect("the later system's actor still runs");
    assert!(ActorSystem::get_named(&name).is_some());
}
