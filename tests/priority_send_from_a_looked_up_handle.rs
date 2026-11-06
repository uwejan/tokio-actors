//! Any holder of an actor's handle can send on its priority lane. A handle the
//! spawner never passed on, fetched from the system by name, overtakes the
//! normal backlog without the receiver having made anything for it.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, ActorSystem,
};

const BOUND: Duration = Duration::from_secs(30);

enum Msg {
    /// Signals `.0` when the handler begins, then waits for `.1`.
    Block(oneshot::Sender<()>, oneshot::Receiver<()>),
    Tag(&'static str),
}

struct Recorder {
    log: Arc<Mutex<Vec<&'static str>>>,
}

impl Actor for Recorder {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Block(entered, release) => {
                let _ = entered.send(());
                let _ = release.await;
            }
            Msg::Tag(tag) => self.log.lock().unwrap().push(tag),
        }
        Ok(())
    }
}

#[test]
fn a_handle_looked_up_by_name_can_send_on_the_priority_lane_and_overtakes_the_backlog() {
    common::run(2, async {
        let sys = ActorSystem::create(format!("priority-lookup-{}", uuid::Uuid::new_v4())).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let spawned = Recorder { log: log.clone() }
            .spawn()
            .named("recorder")
            .with_config(ActorConfig::default().with_priority_lane(4))
            .on_system(&sys)
            .await
            .unwrap();

        // Park the handler so the backlog is built while it sits between two
        // messages.
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        spawned
            .notify(Msg::Block(entered_tx, release_rx))
            .await
            .unwrap();
        timeout(BOUND, entered_rx)
            .await
            .expect("the handler is entered")
            .unwrap();
        spawned.notify(Msg::Tag("normal")).await.unwrap();

        // A handle the spawner never handed to the sender.
        let looked_up = sys
            .get::<Recorder>("recorder")
            .expect("the system holds the actor under its name");
        looked_up
            .notify_priority(Msg::Tag("priority"))
            .await
            .unwrap();

        release_tx.send(()).unwrap();
        timeout(BOUND, async {
            while log.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both messages are handled");
        assert_eq!(*log.lock().unwrap(), vec!["priority", "normal"]);
    });
}
