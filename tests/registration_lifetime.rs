//! An actor's registration in its system lasts through the stop of its
//! children and ends before the actor reads `Stopped`.
//!
//! A supervisor that is stopping stays listed in `actor_ids()` with the status
//! `Stopping` while its children are being stopped, and leaves the list once
//! they are. Whatever way an actor ends (a graceful stop, a `Kill` at a turn
//! boundary, an abort of a callback parked at an await), its id has already
//! left the list when its own value is dropped, and that drop comes before the
//! actor reads `Stopped`.

mod common;

use std::future::IntoFuture;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorId, ActorResult, ActorStatus, ActorSystem, Shutdown, StopReason,
};

const BOUND: Duration = Duration::from_secs(30);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "registration-lifetime-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

async fn within<F: IntoFuture>(what: &str, fut: F) -> F::Output {
    match timeout(BOUND, fut.into_future()).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {BOUND:?} waiting for: {what}"),
    }
}

// ---------------------------------------------------------------------------
// A stopping supervisor stays listed while its children are stopped
// ---------------------------------------------------------------------------

/// What the child's `on_stopped` read about its parent.
type ParentSeen = Arc<Mutex<Option<(bool, Option<ActorStatus>)>>>;

/// A child that, when it is told to stop, reads its parent's registration and
/// then holds its stop until the test releases it.
struct Gated {
    sys: Arc<ActorSystem>,
    parent: Arc<OnceLock<ActorId>>,
    seen: ParentSeen,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for Gated {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let parent = self.parent.get().expect("the parent published its id");
        *self.seen.lock().unwrap() = Some((
            self.sys.actor_ids().contains(parent),
            self.sys.actor_status(parent),
        ));
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

/// Spawns one `Gated` child when it starts and publishes its own id first.
struct Parent {
    sys: Arc<ActorSystem>,
    own_id: Arc<OnceLock<ActorId>>,
    seen: ParentSeen,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for Parent {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _ = self.own_id.set(ctx.actor_id().clone());
        let (sys, parent, seen, entered, release) = (
            self.sys.clone(),
            self.own_id.clone(),
            self.seen.clone(),
            self.entered.clone(),
            self.release.clone(),
        );
        ctx.spawn_child(move || Gated {
            sys: sys.clone(),
            parent: parent.clone(),
            seen: seen.clone(),
            entered: entered.clone(),
            release: release.clone(),
        })
        .shutdown(Shutdown::Infinity)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn a_supervisor_stays_listed_as_stopping_while_its_children_are_stopped() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("listed")).unwrap();
        let seen: ParentSeen = Arc::default();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let sup = within(
            "the supervisor to start",
            Parent {
                sys: sys.clone(),
                own_id: Arc::default(),
                seen: seen.clone(),
                entered: entered.clone(),
                release: release.clone(),
            }
            .spawn()
            .on_system(&sys)
            .supervisor(),
        )
        .await
        .unwrap();
        let id = sup.id().clone();

        sup.stop(StopReason::Graceful).await.unwrap();
        within("the child to begin its stop", entered.notified()).await;

        // The child is mid-stop: the parent's registration is still there.
        assert_eq!(
            *seen.lock().unwrap(),
            Some((true, Some(ActorStatus::Stopping))),
            "the child read its parent as listed and Stopping"
        );
        assert!(
            sys.actor_ids().contains(&id),
            "the supervisor stays listed while its child is being stopped"
        );
        assert_eq!(sys.actor_status(&id), Some(ActorStatus::Stopping));

        release.notify_one();
        let reason = within("the supervisor to stop", sup.wait_stopped()).await;
        assert_eq!(reason, StopReason::Graceful);
        assert!(
            !sys.actor_ids().contains(&id),
            "the supervisor left the list once its children were stopped"
        );
    });
}

// ---------------------------------------------------------------------------
// The id has left the list before the actor value is dropped, before Stopped
// ---------------------------------------------------------------------------

/// The actor's value: when it is dropped it records whether its id is still
/// listed in its system.
struct Listed {
    sys: Arc<ActorSystem>,
    own_id: Arc<OnceLock<ActorId>>,
    listed_at_drop: Arc<Mutex<Option<bool>>>,
    parked: Option<Arc<Notify>>,
    began: Arc<Notify>,
}

impl Actor for Listed {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _ = self.own_id.set(ctx.actor_id().clone());
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(parked) = &self.parked {
            self.began.notify_one();
            // Parked at an await until the task is aborted.
            parked.notified().await;
        }
        Ok(())
    }
}

impl Drop for Listed {
    fn drop(&mut self) {
        if let Some(id) = self.own_id.get() {
            *self.listed_at_drop.lock().unwrap() = Some(self.sys.actor_ids().contains(id));
        }
    }
}

struct EndRig {
    sys: Arc<ActorSystem>,
    listed_at_drop: Arc<Mutex<Option<bool>>>,
    began: Arc<Notify>,
}

impl EndRig {
    fn new(base: &str) -> Self {
        Self {
            sys: ActorSystem::create(uname(base)).unwrap(),
            listed_at_drop: Arc::default(),
            began: Arc::new(Notify::new()),
        }
    }

    fn actor(&self, parked: bool) -> Listed {
        Listed {
            sys: self.sys.clone(),
            own_id: Arc::default(),
            listed_at_drop: self.listed_at_drop.clone(),
            parked: parked.then(|| Arc::new(Notify::new())),
            began: self.began.clone(),
        }
    }

    /// The drop of the value ran, and found the id already unlisted.
    fn assert_unlisted_at_drop(&self, how: &str) {
        assert_eq!(
            *self.listed_at_drop.lock().unwrap(),
            Some(false),
            "the id was still listed when the value of an actor ended by {how} was dropped"
        );
    }
}

#[test]
fn a_gracefully_stopped_actor_is_unlisted_before_its_value_drops() {
    common::run(2, async {
        let rig = EndRig::new("graceful");
        let h = within(
            "the actor to start",
            rig.actor(false).spawn().on_system(&rig.sys),
        )
        .await
        .unwrap();
        let id = h.id().clone();
        assert!(rig.sys.actor_ids().contains(&id));

        h.stop(StopReason::Graceful).await.unwrap();
        assert_eq!(
            within("the actor to stop", h.wait_stopped()).await,
            StopReason::Graceful
        );
        rig.assert_unlisted_at_drop("a graceful stop");
        assert!(!rig.sys.actor_ids().contains(&id));
    });
}

#[test]
fn an_aborted_actor_is_unlisted_before_its_value_drops() {
    common::run(2, async {
        let rig = EndRig::new("abort");
        let h = within(
            "the actor to start",
            rig.actor(true)
                .spawn()
                .on_system(&rig.sys)
                .kill_grace(Duration::ZERO),
        )
        .await
        .unwrap();
        let id = h.id().clone();

        h.notify(()).await.unwrap();
        within("the handler to park", rig.began.notified()).await;
        assert!(within("the kill", rig.sys.kill_by_id(&id)).await.is_some());
        assert_eq!(
            within("the actor to stop", h.wait_stopped()).await,
            StopReason::Kill
        );
        rig.assert_unlisted_at_drop("an abort");
        assert!(!rig.sys.actor_ids().contains(&id));
    });
}
