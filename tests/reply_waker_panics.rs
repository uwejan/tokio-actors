//! The caller of a request, of a status query or of a spawn waits on a
//! one-shot channel with a waker of its own, and the actor's task wakes that
//! waker when it answers. A panic of the wake stays in the answer: the actor
//! goes on or ends with its own reason, and the caller reads the answer on its
//! next poll. (The answer to a spawn whose initialization failed is pinned in
//! the runtime's own tests, where the task's join shows how it ended.)

mod common;

use std::future::{Future, IntoFuture};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorSystem, AskError, StopReason,
};

const BOUND: Duration = Duration::from_secs(30);

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: AtomicUsize,
}

impl LoudWake {
    fn arm(&self, armed: bool) {
        self.armed.store(armed, Ordering::SeqCst);
    }

    /// Waits until the waker has been woken once.
    async fn woken(&self) {
        timeout(BOUND, async {
            while self.woken.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the actor answered the waiting caller");
    }
}

impl Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a waker's wake panics");
        }
    }
}

enum Msg {
    /// Signals `.0` when the handler begins, then waits for `.1`.
    Hold(oneshot::Sender<()>, oneshot::Receiver<()>),
    Echo(u64),
    Fail,
    Panic,
}

struct Probe;

impl Actor for Probe {
    type Message = Msg;
    type Response = u64;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
        match msg {
            Msg::Hold(entered, release) => {
                let _ = entered.send(());
                let _ = release.await;
                Ok(0)
            }
            Msg::Echo(n) => Ok(n),
            Msg::Fail => Err(ActorError::user("boom")),
            Msg::Panic => panic!("handler boom"),
        }
    }
}

/// An actor whose handler is held, and the gate that releases it.
async fn held() -> (ActorHandle<Probe>, oneshot::Sender<()>) {
    let actor = Probe.spawn().await.unwrap();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    actor
        .notify(Msg::Hold(entered_tx, release_rx))
        .await
        .unwrap();
    timeout(BOUND, entered_rx)
        .await
        .expect("the handler is entered")
        .unwrap();
    (actor, release_tx)
}

fn quiet() -> Context<'static> {
    Context::from_waker(Waker::noop())
}

/// A request queued behind a held handler: its caller polls with a waker that
/// panics when woken, and the actor answers it.
#[test]
fn an_ask_whose_callers_waker_panics_when_the_actor_replies_leaves_the_actor_running() {
    common::run(2, async {
        let (actor, release) = held().await;
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut ask = Box::pin(actor.send(Msg::Echo(7)));
        assert!(ask
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        loud.arm(true);
        release.send(()).unwrap();
        loud.woken().await;
        loud.arm(false);

        assert_eq!(
            timeout(BOUND, actor.send(Msg::Echo(9)))
                .await
                .expect("the actor answers")
                .expect("a caller's waker that panicked ended the actor"),
            9
        );
        assert!(matches!(
            ask.as_mut().poll(&mut quiet()),
            Poll::Ready(Ok(7))
        ));
    });
}

/// The handler fails on a request whose caller's waker panics when it is woken
/// with the error: the actor ends with the handler's own error.
#[test]
fn an_ask_whose_callers_waker_panics_on_the_error_reply_leaves_the_actor_its_own_reason() {
    common::run(2, async {
        let (actor, release) = held().await;
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut ask = Box::pin(actor.send(Msg::Fail));
        assert!(ask
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        loud.arm(true);
        release.send(()).unwrap();
        loud.woken().await;
        loud.arm(false);

        assert_eq!(
            timeout(BOUND, actor.wait_stopped())
                .await
                .expect("the actor ends"),
            StopReason::Failure(ActorError::user("boom")),
            "a caller's waker that panicked changed how the actor ended"
        );
        assert!(matches!(
            ask.as_mut().poll(&mut quiet()),
            Poll::Ready(Err(AskError::Actor(ActorError::User(message)))) if message == "boom"
        ));
    });
}

/// The handler panics on a request whose caller's waker panics when it is
/// woken with the error: the actor ends with the handler's own panic.
#[test]
fn an_ask_whose_callers_waker_panics_on_the_panic_reply_leaves_the_actor_its_own_reason() {
    common::run(2, async {
        let (actor, release) = held().await;
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut ask = Box::pin(actor.send(Msg::Panic));
        assert!(ask
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        loud.arm(true);
        release.send(()).unwrap();
        loud.woken().await;
        loud.arm(false);

        assert_eq!(
            timeout(BOUND, actor.wait_stopped())
                .await
                .expect("the actor ends"),
            StopReason::Failure(ActorError::Panic("handler boom".to_string())),
            "a caller's waker that panicked changed how the actor ended"
        );
        assert!(matches!(
            ask.as_mut().poll(&mut quiet()),
            Poll::Ready(Err(AskError::Actor(ActorError::Panic(message)))) if message == "handler boom"
        ));
    });
}

/// A status query queued behind a held handler: its caller polls with a waker
/// that panics when woken, and the actor answers the query between two turns.
#[test]
fn a_status_query_whose_callers_waker_panics_when_the_actor_replies_leaves_the_actor_running() {
    common::run(2, async {
        let (actor, release) = held().await;
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut status = Box::pin(actor.get_status());
        assert!(status
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        loud.arm(true);
        release.send(()).unwrap();
        loud.woken().await;
        loud.arm(false);

        assert_eq!(
            timeout(BOUND, actor.send(Msg::Echo(9)))
                .await
                .expect("the actor answers")
                .expect("a caller's waker that panicked ended the actor"),
            9
        );
        assert!(matches!(
            status.as_mut().poll(&mut quiet()),
            Poll::Ready(Ok(_))
        ));
    });
}

/// An actor whose `pre_start` waits for a gate.
struct Gated {
    gate: Option<oneshot::Receiver<()>>,
}

impl Actor for Gated {
    type Message = u64;
    type Response = u64;

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(gate) = self.gate.take() {
            let _ = gate.await;
        }
        Ok(())
    }

    async fn handle(&mut self, msg: u64, _ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
        Ok(msg)
    }
}

fn unique(base: &str) -> String {
    format!("{base}-{}", uuid::Uuid::new_v4())
}

/// A spawn whose caller polls with a waker that panics when woken: the actor
/// acknowledges its start, and keeps running.
#[test]
fn a_spawn_whose_callers_waker_panics_on_the_ack_leaves_the_actor_running() {
    common::run(2, async {
        let sys = ActorSystem::create(unique("loud-ack")).unwrap();
        let (gate_tx, gate_rx) = oneshot::channel();
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut spawn = Box::pin(
            Gated {
                gate: Some(gate_rx),
            }
            .spawn()
            .on_system(&sys)
            .into_future(),
        );
        assert!(spawn
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        loud.arm(true);
        gate_tx.send(()).unwrap();
        loud.woken().await;
        loud.arm(false);

        let Poll::Ready(Ok(actor)) = spawn.as_mut().poll(&mut quiet()) else {
            panic!("the spawn reads its ack on the next poll");
        };
        assert_eq!(
            timeout(BOUND, actor.send(5))
                .await
                .expect("the actor answers")
                .expect("a caller's waker that panicked ended the actor"),
            5
        );
    });
}
