//! What a caller reads when the actor ends with its message still queued: a
//! request that was queued reads `ResponseDropped` with the final reason, and
//! a reply channel the caller put inside a message itself, which the crate
//! never answers, closes when the message is dropped.

mod common;

use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, AskError, StopReason,
};

const BOUND: Duration = Duration::from_secs(30);

enum Msg {
    /// Signals `.0` when the handler begins, then waits for `.1`.
    Block(oneshot::Sender<()>, oneshot::Receiver<()>),
    /// Answers `.0` from the handler.
    Reply(oneshot::Sender<u32>),
    Echo,
}

struct Server;

impl Actor for Server {
    type Message = Msg;
    type Response = u32;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        match msg {
            Msg::Block(entered, release) => {
                let _ = entered.send(());
                let _ = release.await;
            }
            Msg::Reply(to) => {
                let _ = to.send(7);
            }
            Msg::Echo => {}
        }
        Ok(1)
    }
}

#[test]
fn a_reply_channel_inside_a_queued_message_closes_when_the_actor_ends_without_a_value() {
    common::run(2, async {
        let actor = Server.spawn().await.unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        actor
            .notify(Msg::Block(entered_tx, release_rx))
            .await
            .unwrap();
        timeout(BOUND, entered_rx).await.expect("gate").unwrap();

        let (reply_tx, reply_rx) = oneshot::channel();
        actor.notify(Msg::Reply(reply_tx)).await.unwrap();
        actor.stop(StopReason::Graceful).await.unwrap();
        release_tx.send(()).unwrap();

        let answer = timeout(BOUND, reply_rx)
            .await
            .expect("the channel closes when the message is dropped");
        assert!(
            answer.is_err(),
            "the crate sent nothing on a channel the caller made: {answer:?}"
        );
        timeout(BOUND, actor.wait_stopped()).await.expect("ends");
    });
}

#[test]
fn a_request_still_queued_when_the_actor_ends_reads_response_dropped_with_the_final_reason() {
    common::run(2, async {
        let actor = Server
            .spawn()
            .with_config(ActorConfig::default().with_mailbox_capacity(1))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        actor
            .notify(Msg::Block(entered_tx, release_rx))
            .await
            .unwrap();
        timeout(BOUND, entered_rx).await.expect("gate").unwrap();

        // The one slot is taken by a request that waits for its reply.
        let queued = {
            let actor = actor.clone();
            tokio::spawn(async move { actor.send(Msg::Echo).await })
        };
        timeout(BOUND, async {
            while actor.mailbox_len() < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the request is queued");

        actor.stop(StopReason::Graceful).await.unwrap();
        release_tx.send(()).unwrap();

        let answer = timeout(BOUND, queued)
            .await
            .expect("the request is answered")
            .unwrap();
        match answer {
            Err(AskError::ResponseDropped { reason }) => assert_eq!(reason, StopReason::Graceful),
            other => panic!("expected ResponseDropped with the final reason, got {other:?}"),
        }
    });
}
