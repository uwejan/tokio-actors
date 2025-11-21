use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::sleep;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason,
};

struct LifecycleActor {
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl LifecycleActor {
    fn new() -> (Self, Arc<AtomicBool>, Arc<AtomicBool>) {
        let started = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        (
            Self {
                started: started.clone(),
                stopped: stopped.clone(),
            },
            started,
            stopped,
        )
    }
}

#[derive(Clone)]
enum LifecycleMsg {
    Ping,
}

enum LifecycleResp {
    Ack,
}

#[async_trait]
impl Actor for LifecycleActor {
    type Message = LifecycleMsg;
    type Response = LifecycleResp;

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            LifecycleMsg::Ping => Ok(LifecycleResp::Ack),
        }
    }

    async fn on_stopped(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_hooks_fire_without_supervision() {
    let (actor, started_flag, stopped_flag) = LifecycleActor::new();
    let handle = actor
        .spawn_actor("lifecycle", ActorConfig::default())
        .await
        .unwrap();

    sleep(Duration::from_millis(10)).await;
    assert!(started_flag.load(Ordering::SeqCst));
    println!("lifecycle started=true");

    handle.notify(LifecycleMsg::Ping).await.unwrap();
    handle.stop(StopReason::Graceful).await.unwrap();
    sleep(Duration::from_millis(10)).await;

    assert!(stopped_flag.load(Ordering::SeqCst));
    println!("lifecycle stopped=true");
}
