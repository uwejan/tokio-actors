//! A parked sender's waker belongs to the sender's task. When the actor takes a
//! message, the freed slot goes to that sender, and the waker's wake panics,
//! the panic stays out of the actor's receive: the actor goes on with the
//! message it took and every message behind it, and the sender gets its slot
//! on its next poll.

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
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
    Count,
    Total,
}

#[derive(Default)]
struct Counter(u64);

impl Actor for Counter {
    type Message = Msg;
    type Response = u64;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
        match msg {
            Msg::Block(entered, release) => {
                let _ = entered.send(());
                let _ = release.await;
            }
            Msg::Count => self.0 += 1,
            Msg::Total => {}
        }
        Ok(self.0)
    }
}

fn block() -> (Msg, oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    (Msg::Block(entered_tx, release_rx), entered_rx, release_tx)
}

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: AtomicUsize,
}

impl Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a waker's wake panics");
        }
    }
}

type Pending<'a> = Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + 'a>>;

#[test]
fn a_sender_whose_waker_panics_when_it_is_granted_a_slot_does_not_stop_the_actor() {
    common::run(2, async {
        let actor = Counter::default()
            .spawn()
            .with_config(ActorConfig::default().with_mailbox_capacity(1))
            .await
            .unwrap();

        // The handler is held on a gate, so the one slot fills.
        let (gate, entered, release) = block();
        actor.notify(gate).await.unwrap();
        timeout(BOUND, entered).await.expect("the gate").unwrap();
        actor.try_notify(Msg::Count).unwrap();

        // A second sender parks with a waker that panics when it is woken.
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut waiting: Pending<'_> = Box::pin(actor.notify(Msg::Count));
        let mut cx = Context::from_waker(&waker);
        assert!(
            matches!(waiting.as_mut().poll(&mut cx), Poll::Pending),
            "a send into a full queue parks"
        );

        // The actor takes the queued message and the freed slot goes to the
        // parked sender, whose waker panics.
        loud.armed.store(true, Ordering::SeqCst);
        release.send(()).unwrap();
        timeout(BOUND, async {
            while loud.woken.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the actor wakes the sender it granted the slot");
        loud.armed.store(false, Ordering::SeqCst);

        // The sender is polled again and fills its slot; the actor has
        // handled the first message and handles this one.
        timeout(BOUND, waiting)
            .await
            .expect("the sender completes")
            .unwrap();
        let total = timeout(BOUND, actor.send(Msg::Total))
            .await
            .expect("the actor answers")
            .expect("the actor is running");
        assert_eq!(total, 2);
    });
}
