//! A stop request only strengthens: a request of higher rank within the same
//! strength that lands while `pre_stop` runs is the reason the actor stops
//! with when `pre_stop` accepts, and `on_stopped` receives it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, StopReason,
};

struct Gate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    asked: Arc<Mutex<Vec<StopReason>>>,
    on_stopped_saw: Arc<Mutex<Option<StopReason>>>,
    /// Vetoes a `Graceful` request, accepts anything else.
    veto_graceful: bool,
}

impl Actor for Gate {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.asked.lock().unwrap().push(reason.clone());
        self.entered.notify_one();
        self.release.notified().await;
        !(self.veto_graceful && *reason == StopReason::Graceful)
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.on_stopped_saw.lock().unwrap() = Some(reason.clone());
        Ok(())
    }
}

struct Rig {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    asked: Arc<Mutex<Vec<StopReason>>>,
    saw: Arc<Mutex<Option<StopReason>>>,
    handle: tokio_actors::ActorHandle<Gate>,
}

async fn rig(veto_graceful: bool) -> Rig {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let asked = Arc::new(Mutex::new(Vec::new()));
    let saw = Arc::new(Mutex::new(None));
    let handle = Gate {
        entered: entered.clone(),
        release: release.clone(),
        asked: asked.clone(),
        on_stopped_saw: saw.clone(),
        veto_graceful,
    }
    .spawn()
    .await
    .unwrap();
    Rig {
        entered,
        release,
        asked,
        saw,
        handle,
    }
}

async fn ended(rig: &Rig) -> StopReason {
    tokio::time::timeout(Duration::from_secs(10), rig.handle.wait_stopped())
        .await
        .expect("the actor ends")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_request_that_lands_during_an_accepting_pre_stop_is_the_reason() {
    let rig = rig(false).await;
    rig.handle.stop(StopReason::Graceful).await.unwrap();
    rig.entered.notified().await;
    rig.handle.stop(StopReason::ParentRequest).await.unwrap();
    rig.release.notify_one();
    assert_eq!(ended(&rig).await, StopReason::ParentRequest);
    assert_eq!(*rig.saw.lock().unwrap(), Some(StopReason::ParentRequest));
    assert_eq!(
        *rig.asked.lock().unwrap(),
        vec![StopReason::Graceful],
        "an accepting pre_stop is not asked again"
    );
}
