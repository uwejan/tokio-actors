//! A system is held until the actor that runs in it has sealed its stop lane.
//!
//! A handle reaches its system by identity and a `Kill` it raises needs that
//! system for its deadline and backstop, so no `Kill` may be accepted once the
//! last holder of the system is gone. The actor's task holds the system
//! through its registry guard, its context, its system plane and its queue
//! slots; the queue slot's hold is the one that lasts past the guard's drop.

use std::sync::Mutex;
use std::time::Duration;

use crate::actor::{context::ActorContext, Actor, ActorExt};
use crate::error::ActorResult;
use crate::system::ActorSystem;
use crate::types::StopReason;

/// What a hook saw: the point, whether the system was reachable and whether
/// the lane took a `Kill`.
type Seen = (&'static str, bool, bool);

struct Blocker {
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    started: std::sync::mpsc::Sender<()>,
}

impl Actor for Blocker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        // Holds its worker thread until the test lets go, so the actor is
        // still registered when its system is shut down and dropped.
        let _ = self.started.send(());
        let _ = self.release.lock().unwrap().recv();
        Ok(())
    }
}

/// An actor that does nothing.
struct Twin;

impl Actor for Twin {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// An actor that sends an exit signal through the handle it is given.
struct Signaller;

impl Actor for Signaller {
    type Message = crate::actor::handle::ActorHandle<Twin>;
    type Response = ();

    async fn handle(
        &mut self,
        target: Self::Message,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        ctx.exit(&target, StopReason::Kill)
            .expect("a Kill is never refused");
        Ok(())
    }
}

/// An exit signal sent through a handle names the handle's own system, found
/// by identity. When that system is gone and a later system holds the same
/// name and an actor of the same id, the signal reaches nothing of the later
/// system: its actor lives, no stop request or kill step names anything, and
/// no event is raised there for an incarnation it never held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exit_through_the_handle_of_a_gone_system_leaves_a_later_system_of_the_same_name_alone()
{
    use crate::telemetry::{EventKind, KindSet, TelemetryEvent};

    let name = format!("exit-by-handle-{}", uuid::Uuid::new_v4());
    let first = ActorSystem::create(name.clone()).unwrap();
    let old = Twin.spawn().named("twin").on_system(&first).await.unwrap();
    first.shutdown().await;
    old.wait_stopped().await;
    drop(first);
    for _ in 0..250 {
        if ActorSystem::get_named(&name).is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        ActorSystem::get_named(&name).is_none(),
        "the first system is gone"
    );

    let second = ActorSystem::create(name.clone()).unwrap();
    let twin = Twin.spawn().named("twin").on_system(&second).await.unwrap();
    let sender = Signaller
        .spawn()
        .named("signaller")
        .on_system(&second)
        .await
        .unwrap();
    let sink = second.telemetry_mode().collect(KindSet::of(&[
        EventKind::StopRequested,
        EventKind::KillStep,
    ]));
    sender.send(old.clone()).await.unwrap();

    // The pump is one queue: the request of a later stop is reported after
    // anything the signal raised, so once it is here, the signal has been
    // reported or never.
    let barrier = Twin
        .spawn()
        .named("barrier")
        .on_system(&second)
        .await
        .unwrap();
    barrier.stop(StopReason::Graceful).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let events = loop {
        let events = sink.events();
        if events.iter().any(|event| {
            matches!(event, TelemetryEvent::StopRequested { actor, .. }
                if actor.id().as_str() == "barrier")
        }) {
            break events;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the barrier's request is never reported"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let others: Vec<_> = events
        .iter()
        .filter(|event| {
            !matches!(event, TelemetryEvent::StopRequested { actor, .. }
                if actor.id().as_str() == "barrier")
        })
        .collect();
    assert!(others.is_empty(), "the signal raised events: {others:#?}");
    assert_eq!(
        second.telemetry_mode().pump.dropped_for_test(),
        0,
        "no event was raised for an incarnation the later system never held"
    );
    assert!(
        twin.is_alive(),
        "the later system's actor of the same id is untouched"
    );
}
