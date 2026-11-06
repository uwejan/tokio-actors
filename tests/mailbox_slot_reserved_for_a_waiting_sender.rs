//! A slot handed to a waiting sender counts as in use from the moment it is
//! handed over, before that sender is polled again and pushes its message:
//! `mailbox_len` and `priority_len` count the slots in use (the messages
//! queued plus the slots reserved and not yet filled), and `mailbox_available`
//! counts only the free ones.

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, SendError,
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

fn block() -> (Msg, oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    (Msg::Block(entered_tx, release_rx), entered_rx, release_tx)
}

type Pending<'a> = Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + 'a>>;

/// Polls `send` once with a waker that does nothing and asserts it parks.
fn park(send: &mut Pending<'_>) {
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(send.as_mut().poll(&mut cx), Poll::Pending),
        "a send into a full queue parks"
    );
}

async fn wait_for_len(log: &Mutex<Vec<&'static str>>, n: usize) {
    timeout(BOUND, async {
        while log.lock().unwrap().len() < n {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the messages are handled");
}

#[test]
fn a_slot_handed_to_a_waiting_sender_counts_in_mailbox_len_before_the_sender_pushes() {
    common::run(2, async {
        let log = Arc::new(Mutex::new(Vec::new()));
        let actor = Recorder { log: log.clone() }
            .spawn()
            .with_config(ActorConfig::default().with_mailbox_capacity(2))
            .await
            .unwrap();

        // The handler is held on the first gate, so the two slots fill.
        let (first, entered, release_first) = block();
        actor.notify(first).await.unwrap();
        timeout(BOUND, entered).await.expect("first gate").unwrap();
        let (second, second_entered, release_second) = block();
        actor.try_notify(second).unwrap();
        actor.try_notify(Msg::Tag("queued")).unwrap();
        assert_eq!(actor.mailbox_len(), 2);
        assert_eq!(actor.mailbox_available(), 0);

        // A third sender parks, then the actor takes the second gate message
        // and the freed slot goes to the parked sender.
        let mut waiting: Pending<'_> = Box::pin(actor.notify(Msg::Tag("waiting")));
        park(&mut waiting);
        release_first.send(()).unwrap();
        timeout(BOUND, second_entered)
            .await
            .expect("second gate")
            .unwrap();

        // The queue holds one message and one slot is reserved for the parked
        // sender, which has not been polled again.
        assert_eq!(actor.mailbox_len(), 2, "the reserved slot is in use");
        assert_eq!(actor.mailbox_available(), 0, "no slot is free");
        assert!(log.lock().unwrap().is_empty(), "nothing else was handled");

        // The sender fills the slot it was given and the message arrives.
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            waiting.as_mut().poll(&mut cx),
            Poll::Ready(Ok(()))
        ));
        release_second.send(()).unwrap();
        wait_for_len(&log, 2).await;
        assert_eq!(*log.lock().unwrap(), vec!["queued", "waiting"]);
    });
}

#[test]
fn a_slot_handed_to_a_waiting_sender_counts_in_priority_len_before_the_sender_pushes() {
    common::run(2, async {
        let log = Arc::new(Mutex::new(Vec::new()));
        let actor = Recorder { log: log.clone() }
            .spawn()
            .with_config(ActorConfig::default().with_priority_lane(1))
            .await
            .unwrap();

        let (gate, entered, release) = block();
        actor.notify(gate).await.unwrap();
        timeout(BOUND, entered).await.expect("gate").unwrap();
        actor.try_notify_priority(Msg::Tag("first")).unwrap();
        assert_eq!(actor.priority_len(), 1);

        let mut waiting: Pending<'_> = Box::pin(actor.notify_priority(Msg::Tag("waiting")));
        park(&mut waiting);

        // The actor takes the queued priority message, the slot goes to the
        // parked sender, and a normal message behind it shows the actor has
        // moved on.
        release.send(()).unwrap();
        actor.notify(Msg::Tag("sync")).await.unwrap();
        wait_for_len(&log, 2).await;
        assert_eq!(*log.lock().unwrap(), vec!["first", "sync"]);
        assert_eq!(
            actor.priority_len(),
            1,
            "the slot reserved for the parked sender is in use"
        );

        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            waiting.as_mut().poll(&mut cx),
            Poll::Ready(Ok(()))
        ));
        wait_for_len(&log, 3).await;
        assert_eq!(*log.lock().unwrap(), vec!["first", "sync", "waiting"]);
        assert_eq!(actor.priority_len(), 0);
    });
}
