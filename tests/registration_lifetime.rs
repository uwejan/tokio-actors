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
    ActorId, ActorResult, ActorSystem, StopReason,
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
