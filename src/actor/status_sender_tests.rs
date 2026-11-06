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

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::actor::context::ActorContext;
use crate::actor::runtime::{spawn_actor, spawn_actor_parts, ActorConfig};
use crate::actor::supervision::DEFAULT_KILL_GRACE;
use crate::actor::Actor;
use crate::error::ActorResult;
use crate::system::ActorSystem;
use crate::types::{ActorId, StatusRx, StopReason};

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

/// The test variant holds one more sender for its caller, and the plane stays
/// open past the actor's own end while the caller holds it.
async fn the_variant_that_keeps_a_sender_holds_five() {
    let system = ActorSystem::create(unique("senders-five")).unwrap();
    let (handle, join, kept, _subtree) = spawn_actor_parts(
        ActorId::from(unique("five")),
        Idle,
        ActorConfig::default(),
        None,
        Some(system),
        None,
        true,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the actor spawns");
    let plane = handle.status_rx();
    assert_eq!(plane.sender_count(), 5, "the four and the caller's");
    handle.stop(StopReason::Graceful).await.unwrap();
    tokio::time::timeout(HANG, handle.wait_stopped())
        .await
        .expect("the actor stops");
    // The task completes only after every drop of its end has run.
    tokio::time::timeout(HANG, join)
        .await
        .expect("the task ends")
        .expect("the task is not cancelled");
    assert!(
        !plane.has_changed().is_err(),
        "the plane stays open while the caller holds its sender"
    );
    drop(kept);
    assert!(
        plane.has_changed().is_err(),
        "the plane closes when the caller lets go of the last sender"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_spawn_holds_four_senders_current_thread() {
    a_spawn_holds_four_senders().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spawn_holds_four_senders_multi_thread() {
    a_spawn_holds_four_senders().await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_variant_that_keeps_a_sender_holds_five_current_thread() {
    the_variant_that_keeps_a_sender_holds_five().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_variant_that_keeps_a_sender_holds_five_multi_thread() {
    the_variant_that_keeps_a_sender_holds_five().await;
}

/// An actor's value: when it drops it records whether the plane reads as
/// ended.
struct Probe {
    plane: Arc<OnceLock<StatusRx>>,
    ended: Arc<Mutex<Option<bool>>>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        let ended = self
            .plane
            .get()
            .is_some_and(|plane| plane.has_changed().is_err());
        *self.ended.lock().unwrap() = Some(ended);
    }
}

/// An actor with no timer and no stream, whose value is a [`Probe`]. It parks
/// in `on_started` when asked to, so an abort lands while it is suspended.
struct Bare {
    _probe: Probe,
    started: Option<oneshot::Sender<()>>,
    park: bool,
}

impl Actor for Bare {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
        }
        if self.park {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A [`Bare`] actor spawned through `spawn_actor`, and what its value saw.
struct BareRig {
    handle: crate::actor::handle::ActorHandle<Bare>,
    join: tokio::task::JoinHandle<StopReason>,
    plane: Arc<OnceLock<StatusRx>>,
    ended: Arc<Mutex<Option<bool>>>,
    started: oneshot::Receiver<()>,
}

fn bare(park: bool) -> BareRig {
    let system = ActorSystem::create(unique("senders-bare")).unwrap();
    let plane = Arc::new(OnceLock::new());
    let ended = Arc::new(Mutex::new(None));
    let (started_tx, started) = oneshot::channel();
    let (handle, join, _subtree) = spawn_actor(
        ActorId::from(unique("bare")),
        Bare {
            _probe: Probe {
                plane: plane.clone(),
                ended: ended.clone(),
            },
            started: Some(started_tx),
            park,
        },
        ActorConfig::default(),
        None,
        Some(system),
        None,
        true,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the actor spawns");
    let _ = plane.set(handle.status_rx());
    BareRig {
        handle,
        join,
        plane,
        ended,
        started,
    }
}

impl BareRig {
    /// Waits for the task to end, however it ends: it completes only after
    /// every drop of its end has run.
    async fn task_end(self) -> (Option<bool>, bool) {
        let _ = tokio::time::timeout(HANG, self.join)
            .await
            .expect("the task ends");
        let saw = *self.ended.lock().unwrap();
        let closed = self.plane.get().unwrap().has_changed().is_err();
        (saw, closed)
    }
}

/// On a return the value drops while the plane is open, and the plane is
/// closed once the task has ended.
async fn a_returning_actor_s_value_drops_while_the_plane_is_open() {
    let mut rig = bare(false);
    tokio::time::timeout(HANG, &mut rig.started)
        .await
        .expect("the actor starts")
        .expect("the start is reported");
    rig.handle.stop(StopReason::Graceful).await.unwrap();
    let (saw, closed) = rig.task_end().await;
    assert_eq!(saw, Some(false), "the value dropped after the plane closed");
    assert!(closed, "the plane is closed once the task has ended");
}

/// On an abort while suspended the value drops while the plane is open, and
/// the plane is closed once the task has ended.
async fn a_suspended_actor_s_value_drops_while_the_plane_is_open() {
    let mut rig = bare(true);
    tokio::time::timeout(HANG, &mut rig.started)
        .await
        .expect("the actor starts")
        .expect("the start is reported");
    rig.join.abort();
    let (saw, closed) = rig.task_end().await;
    assert_eq!(saw, Some(false), "the value dropped after the plane closed");
    assert!(closed, "the plane is closed once the task has ended");
}

#[tokio::test(flavor = "current_thread")]
async fn a_returning_actor_s_value_drops_while_the_plane_is_open_current_thread() {
    a_returning_actor_s_value_drops_while_the_plane_is_open().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_returning_actor_s_value_drops_while_the_plane_is_open_multi_thread() {
    a_returning_actor_s_value_drops_while_the_plane_is_open().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_suspended_actor_s_value_drops_while_the_plane_is_open_current_thread() {
    a_suspended_actor_s_value_drops_while_the_plane_is_open().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suspended_actor_s_value_drops_while_the_plane_is_open_multi_thread() {
    a_suspended_actor_s_value_drops_while_the_plane_is_open().await;
}

/// On an abort before the first poll the value drops while the plane is open
/// (the registry guard, declared last, still holds it), and the plane is
/// closed once the task has ended. A current-thread runtime does not poll the
/// task before the test awaits.
#[tokio::test(flavor = "current_thread")]
async fn an_actor_aborted_before_its_first_poll_drops_its_value_while_the_plane_is_open() {
    let rig = bare(false);
    rig.join.abort();
    let (saw, closed) = rig.task_end().await;
    assert_eq!(saw, Some(false), "the value dropped after the plane closed");
    assert!(closed, "the plane is closed once the task has ended");
}
