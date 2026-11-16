//! Calls to a state machine actor, each wait on a handshake the test observes.

use std::future::Future;
use std::task::Poll;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_actors::actor::{context::ActorContext, ActorExt};
use tokio_actors::statem::{
    Harness, ReplyTo, StateMachine, StatemActor, StatemContext, StatemEvent, Transition,
};
use tokio_actors::{ActorError, ActorHandle, ActorResult, AskError, StopReason};

mod common;

#[rustfmt::skip]
enum Ev { Open, Stop, Gate }

/// A state whose comparison panics when either side is 9.
#[derive(Clone)]
struct St(u8);

impl PartialEq for St {
    fn eq(&self, other: &Self) -> bool {
        assert!(self.0 != 9 && other.0 != 9, "the state compare panics");
        self.0 == other.0
    }
}

/// A second machine whose call carries the first machine's return address and
/// whose event function panics holding it.
struct Relay;

#[rustfmt::skip]
impl StateMachine for Relay {
    type State = ();
    type Event = ();
    type Call = ReplyTo<u16>;
    type Reply = ();
    type TimerName = ();

    fn handle_event(&mut self, _: &(), _: StatemEvent<Self>, _: &mut StatemContext<'_, Self>) -> Transition<()> {
        panic!("the inner machine panics holding the outer address")
    }
}

/// A third machine that postpones a call carrying another machine's address
/// and stops in the same step.
struct Holder;

#[rustfmt::skip]
impl StateMachine for Holder {
    type State = ();
    type Event = ReplyTo<u16>;
    type Call = u16;
    type Reply = u16;
    type TimerName = ();

    fn handle_event(&mut self, _: &(), event: StatemEvent<Self>, cx: &mut StatemContext<'_, Self>) -> Transition<()> {
        if let StatemEvent::Cast(to) = event { let _ = cx.postpone(StatemEvent::Call(0, to)); }
        Transition::Stop(StopReason::Graceful)
    }
}

/// The handshakes a row observes: `postponed` fires when a call is postponed,
/// `park` receives a return address the machine gives away, `own` reports the
/// outcome of the machine's call to itself, `gate` holds a gated turn on a
/// multi-thread runtime until the row releases it.
struct Door {
    postponed: mpsc::UnboundedSender<()>,
    park: mpsc::UnboundedSender<ReplyTo<u16>>,
    own: Option<oneshot::Sender<bool>>,
    gate: std::sync::mpsc::Receiver<()>,
}

#[rustfmt::skip]
impl StateMachine for Door {
    type State = St;
    type Event = Ev;
    type Call = u16;
    type Reply = u16;
    type TimerName = ();

    async fn init(&mut self, ctx: &mut ActorContext<StatemActor<Self>>) -> ActorResult<()> {
        if let Some(report) = self.own.take() {
            let outcome = ctx.self_handle().call(1).await;
            let _ = report.send(matches!(outcome, Err(AskError::CallingSelf)));
        }
        Ok(())
    }

    fn handle_event(&mut self, state: &St, event: StatemEvent<Self>, cx: &mut StatemContext<'_, Self>) -> Transition<St> {
        match (state.0, event) {
            (0, StatemEvent::Call(254, to)) => {
                let _ = self.postponed.send(());
                let _ = cx.postpone(StatemEvent::Call(254, to));
            }
            (_, StatemEvent::Cast(Ev::Open)) => return Transition::Next(St(1)),
            (_, StatemEvent::Cast(Ev::Gate)) => {
                let _ = self.postponed.send(());
                if tokio::runtime::Handle::current().runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
                    let _ = self.gate.recv_timeout(Duration::from_secs(30));
                }
            }
            (_, StatemEvent::Call(248, to)) => { let _ = cx.postpone(StatemEvent::Call(248, to)); return Transition::Next(St(9)); }
            (_, StatemEvent::Call(249, to)) => { Harness::new(Relay, ()).0.call(to); }
            (_, StatemEvent::Cast(Ev::Stop)) => return Transition::Stop(StopReason::Graceful),
            (_, StatemEvent::Call(255, to)) => drop(to),
            (_, StatemEvent::Call(253, to)) => { let _ = self.park.send(to); }
            (_, StatemEvent::Call(251, to)) => { cx.reply(to, 7); return Transition::Stop(StopReason::Graceful); }
            (_, StatemEvent::Call(250, _to)) => panic!("callback panics holding the address"),
            (_, StatemEvent::Call(c, to)) => cx.reply(to, c * 2),
            _ => {}
        }
        Transition::Keep
    }
}

struct Rig {
    door: ActorHandle<StatemActor<Door>>,
    postponed: mpsc::UnboundedReceiver<()>,
    park: mpsc::UnboundedReceiver<ReplyTo<u16>>,
    gate: std::sync::mpsc::Sender<()>,
}

#[rustfmt::skip]
async fn rig(own: Option<oneshot::Sender<bool>>) -> Rig {
    let ((postponed_tx, postponed), (park_tx, park)) = (mpsc::unbounded_channel(), mpsc::unbounded_channel());
    let (gate, gate_rx) = std::sync::mpsc::channel();
    let door = StatemActor::new(Door { postponed: postponed_tx, park: park_tx, own, gate: gate_rx }, St(0)).spawn().await.unwrap();
    Rig { door, postponed, park, gate }
}

type Called = tokio::task::JoinHandle<Result<u16, AskError>>;

/// A call made from a task of its own, so the row can act while it waits.
fn calling(door: &ActorHandle<StatemActor<Door>>, call: u16) -> Called {
    let door = door.clone();
    tokio::spawn(async move { door.call(call).await })
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), fut)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for: {what}"))
}

#[test]
#[rustfmt::skip]
fn statem_calls() {
    common::run(2, async {
        // A call is answered, with and without a bound.
        let mut r = rig(None).await;
        assert_eq!(within("reply", r.door.call(3)).await.unwrap(), 6);
        assert_eq!(within("bounded reply", r.door.call_timeout(4, Duration::from_secs(30))).await.unwrap(), 8);

        // A call postponed in state 0 is answered after the change to state 1.
        let call = calling(&r.door, 254);
        within("postponed", r.postponed.recv()).await.unwrap();
        r.door.cast(Ev::Open).await.unwrap();
        assert_eq!(within("answer after the change", call).await.unwrap().unwrap(), 508);

        // A return address dropped while the machine runs answers NoReply.
        let outcome = within("dropped address", r.door.call(255)).await;
        assert!(matches!(outcome, Err(AskError::Actor(ActorError::NoReply))), "{outcome:?}");

        // A return address held by another task that panics answers NoReply while the machine runs.
        let call = calling(&r.door, 253);
        let kept = within("parked address", r.park.recv()).await.unwrap();
        let holder = tokio::spawn(async move { let _kept = kept; panic!("task panics holding the address") });
        assert!(within("holder's panic", holder).await.unwrap_err().is_panic());
        let outcome = tokio::time::timeout(Duration::from_secs(30), call).await.expect("call hangs").unwrap();
        assert!(matches!(outcome, Err(AskError::Actor(ActorError::NoReply))), "{outcome:?}");

        // A return address given away when the machine ends: the call reads the end, not the address.
        let call = calling(&r.door, 253);
        let kept = within("parked address", r.park.recv()).await.unwrap();
        r.door.cast(Ev::Stop).await.unwrap();
        let outcome = within("end of the machine", call).await.unwrap();
        assert!(matches!(outcome, Err(AskError::ResponseDropped { reason: StopReason::Graceful })), "{outcome:?}");
        drop(kept);

        // A reply sent together with a stop is read.
        let r = rig(None).await;
        assert_eq!(within("reply with a stop", r.door.call(251)).await.unwrap(), 7);

        // A return address dropped before the caller's wait starts: the failed turn's answer
        // is held until the machine has ended, so the address is closed when the wait begins.
        let r = rig(None).await;
        let outcome = within("address dropped by a panic", r.door.call(250)).await;
        assert!(matches!(outcome, Err(AskError::ResponseDropped { reason: StopReason::Failure(_) })), "{outcome:?}");

        // A call postponed at a graceful stop reads the stop.
        let mut r = rig(None).await;
        let call = calling(&r.door, 254);
        within("postponed", r.postponed.recv()).await.unwrap();
        r.door.cast(Ev::Stop).await.unwrap();
        let outcome = within("end of the machine", call).await.unwrap();
        assert!(matches!(outcome, Err(AskError::ResponseDropped { reason: StopReason::Graceful })), "{outcome:?}");

        // A call postponed by a step whose state compare panics reads the failure.
        let r = rig(None).await;
        let outcome = within("compare panics", r.door.call(248)).await;
        assert!(matches!(outcome, Err(AskError::ResponseDropped { reason: StopReason::Failure(_) })), "{outcome:?}");

        // A call queued behind a turn when a Kill lands reads the Kill.
        let mut r = rig(None).await;
        r.door.cast(Ev::Gate).await.unwrap();
        within("gated turn", r.postponed.recv()).await.unwrap();
        let mut queued = Box::pin(r.door.call(3));
        std::future::poll_fn(|cx| { assert!(queued.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
        r.door.stop(StopReason::Kill).await.unwrap();
        let _ = r.gate.send(());
        let outcome = within("killed", queued).await;
        assert!(matches!(outcome, Err(AskError::ResponseDropped { reason: StopReason::Kill })), "{outcome:?}");

        // An address handed to another machine whose callback panics through this one reads this one's failure.
        let r = rig(None).await;
        let outcome = within("inner panic", r.door.call(249)).await;
        assert!(matches!(outcome, Err(AskError::ResponseDropped { reason: StopReason::Failure(_) })), "{outcome:?}");

        // An address handed to another machine that postpones it and stops answers NoReply, and this machine goes on.
        let mut r = rig(None).await;
        let call = calling(&r.door, 253);
        let kept = within("parked address", r.park.recv()).await.unwrap();
        StatemActor::new(Holder, ()).spawn().await.unwrap().cast(kept).await.unwrap();
        let outcome = within("address closed by the other machine", call).await.unwrap();
        assert!(matches!(outcome, Err(AskError::Actor(ActorError::NoReply))), "{outcome:?}");
        assert_eq!(within("next call", r.door.call(3)).await.unwrap(), 6);

        // A call from the machine's own task is refused.
        let (own, refused) = oneshot::channel();
        let _r = rig(Some(own)).await;
        assert!(within("call to oneself", refused).await.unwrap());

        // A call whose address is kept past its bound reports a timeout with the call enqueued.
        let mut r = rig(None).await;
        let late = r.door.call_timeout(253, Duration::from_millis(20));
        let outcome = within("bounded call", late).await;
        assert!(matches!(outcome, Err(AskError::Timeout { enqueued: true })), "{outcome:?}");
        drop(within("parked address", r.park.recv()).await);
    });
}
