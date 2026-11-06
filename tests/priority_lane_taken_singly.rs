//! The priority lane hands the run loop one message per receive, so the
//! messages still in the lane stay counted by `priority_len` while a handler
//! runs.

mod common;

use std::time::Duration;

use tokio::sync::oneshot;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult,
};

enum Msg {
    Block(oneshot::Sender<()>, oneshot::Receiver<()>),
    Tag,
}

struct Gate;

impl Actor for Gate {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Msg::Block(entered, release) = msg {
            let _ = entered.send(());
            let _ = release.await;
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_priority_lane_is_taken_one_message_at_a_time() {
    let handle = Gate
        .spawn()
        .with_config(ActorConfig::default().with_priority_lane(8))
        .await
        .unwrap();
    // The actor is parked in a normal-lane handler while the lane fills.
    let (entered_a, entered_a_rx) = oneshot::channel();
    let (release_a, release_a_rx) = oneshot::channel();
    handle
        .notify(Msg::Block(entered_a, release_a_rx))
        .await
        .unwrap();
    entered_a_rx.await.unwrap();
    let (entered_b, entered_b_rx) = oneshot::channel();
    let (release_b, release_b_rx) = oneshot::channel();
    handle
        .notify_priority(Msg::Block(entered_b, release_b_rx))
        .await
        .unwrap();
    for _ in 0..3 {
        handle.notify_priority(Msg::Tag).await.unwrap();
    }
    release_a.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered_b_rx)
        .await
        .expect("the first priority message is handled")
        .unwrap();
    assert_eq!(
        handle.priority_len(),
        3,
        "the three messages behind the first stay in the lane"
    );
    release_b.send(()).unwrap();
}
