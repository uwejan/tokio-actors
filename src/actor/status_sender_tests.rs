//! The senders of an actor's status plane right after its spawn: four, one
//! each for the registry guard, the context, the abort guard and the run
//! loop's own parameters. A spawn that keeps a sender for the caller (only the
//! test variant `spawn_actor_parts`) holds one more, and the plane stays open
//! while the caller holds it. The end-order tests spawn through `spawn_actor`,
//! so the shape they check is the shape of every other spawn.
//!
//! The last tests pin where the plane closes for an actor with no timer and no
//! stream: each of those holds a sender of its own until its future drops, and
//! so keeps the plane open past the run loop's parameters, which hides the
//! order of the parameters' senders from the end-order tests. Without them the
//! plane stays open while the actor's value drops, on a return, on an abort
//! while suspended and on an abort before the first poll, and is closed once
//! the task has ended.

use std::time::Duration;

use crate::actor::context::ActorContext;
use crate::actor::runtime::{spawn_actor, ActorConfig};
use crate::actor::supervision::DEFAULT_KILL_GRACE;
use crate::actor::Actor;
use crate::error::ActorResult;
use crate::system::ActorSystem;
use crate::types::{ActorId, StopReason};

/// The longest a wait may take. It only limits how long a real hang takes to
/// report.
const HANG: Duration = Duration::from_secs(60);

/// An actor that only answers.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn unique(what: &str) -> String {
    format!("{what}-{}", uuid::Uuid::new_v4())
}

/// A spawn through `spawn_actor` holds four senders, whether or not its task
/// has run yet, and the same four after an acknowledged start.
async fn a_spawn_holds_four_senders() {
    let system = ActorSystem::create(unique("senders-four")).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (handle, _join, _subtree) = spawn_actor(
        ActorId::from(unique("four")),
        Idle,
        ActorConfig::default(),
        None,
        Some(system),
        Some(tx),
        true,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the actor spawns");
    assert_eq!(
        handle.status_rx().sender_count(),
        4,
        "guard, context, abort guard and run loop"
    );
    tokio::time::timeout(HANG, rx)
        .await
        .expect("the start is acknowledged")
        .expect("the ack is sent")
        .expect("the actor starts");
    assert_eq!(
        handle.status_rx().sender_count(),
        4,
        "a started actor holds the same four"
    );
    handle.stop(StopReason::Graceful).await.unwrap();
    tokio::time::timeout(HANG, handle.wait_stopped())
        .await
        .expect("the actor stops");
}

#[tokio::test(flavor = "current_thread")]
async fn a_spawn_holds_four_senders_current_thread() {
    a_spawn_holds_four_senders().await;
}
