//! The refusals and bounds of an event manager's client calls, read off a
//! manager whose thread a handler holds: a full mailbox, a bound that passes, a
//! call from the manager's own task, a manager that is stopping and one that
//! has ended, and an owner that is stopping.

use std::future::Future;
use std::pin::pin;
use std::sync::mpsc as std_mpsc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_actors::event::{
    EventError, EventHandler, EventManager, EventManagerRef, Handled, HandlerExit, Removed,
};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason,
};

mod common;

type Events = EventManagerRef<u8, (), ()>;

/// Event 7 polls a call to its own manager once and reports the poll; event 1
/// holds the manager's thread until the gate gives; a stop of the manager
/// holds it in `terminate` the same way.
struct Held {
    gate: std_mpsc::Receiver<()>,
    report: mpsc::UnboundedSender<String>,
    own: Events,
}

impl Held {
    fn hold(&self, what: &str) {
        let _ = self.report.send(what.into());
        let _ = self.gate.recv_timeout(Duration::from_secs(30));
    }
}

impl EventHandler for Held {
    type Event = u8;
    type Call = ();
    type Reply = ();

    fn handle_event(&mut self, event: &u8) -> Handled {
        match event {
            7 => {
                let mut cx = Context::from_waker(Waker::noop());
                let polled = pin!(self.own.which_handlers()).poll(&mut cx);
                let _ = self.report.send(format!("{polled:?}"));
            }
            1 => self.hold("event"),
            _ => {}
        }
        Handled::Keep
    }

    fn terminate(&mut self, why: &Removed) -> Option<()> {
        if matches!(why, Removed::Stop(_)) {
            self.hold("terminate");
        }
        None
    }
}

struct Quiet;

impl EventHandler for Quiet {
    type Event = u8;
    type Call = ();
    type Reply = ();

    fn handle_event(&mut self, _: &u8) -> Handled {
        Handled::Keep
    }
}

/// Asks to supervise a handler from `on_stopped` and reports the answer.
struct Late {
    manager: Events,
    report: mpsc::UnboundedSender<Result<(), EventError>>,
}

impl Actor for Late {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let added = ctx
            .add_sup_handler(&self.manager, Quiet, |_: HandlerExit| ())
            .await;
        let _ = self.report.send(added.map(|_| ()));
        Ok(())
    }
}

async fn got<T>(seen: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), seen.recv())
        .await
        .expect("a hang")
        .expect("the sender is alive")
}

#[test]
#[rustfmt::skip]
fn event_calls() {
    common::run_multi_thread_only(2, async {
        // An owner that is stopping adds no supervised handler.
        let spare = EventManager::<u8>::new().spawn().await.unwrap();
        let (told, mut answers) = mpsc::unbounded_channel();
        let late = Late { manager: EventManagerRef::of(&spare), report: told }.spawn().await.unwrap();
        late.stop(StopReason::Graceful).await.unwrap();
        assert_eq!(got(&mut answers).await, Err(EventError::Stopping));

        let (open, gate) = std_mpsc::channel();
        let (report, mut seen) = mpsc::unbounded_channel();
        let manager = EventManager::<u8>::new().spawn().with_config(ActorConfig::default().with_mailbox_capacity(2)).await.unwrap();
        let events = EventManagerRef::of(&manager);
        let held = events.add_handler(Held { gate, report, own: events.clone() }).await.unwrap();

        // A call made from the manager's own task is refused.
        events.notify(7).await.unwrap();
        assert_eq!(got(&mut seen).await, "Ready(Err(CallingSelf))");

        // A bound that passes ends the caller's wait, and the request it queued is still served.
        events.notify(1).await.unwrap();
        assert_eq!(got(&mut seen).await, "event");
        let late_add = events.clone().timeout(Duration::ZERO).add_handler(Quiet).await;
        assert_eq!(late_add, Err(EventError::Timeout));

        // `try_notify` answers a full mailbox at once, and `notify` waits for room.
        assert_eq!(events.try_notify(2), Ok(()));
        assert_eq!(events.try_notify(2), Err(EventError::Full));
        let mut waiting = Box::pin(events.notify(3));
        std::future::poll_fn(|cx| { assert!(waiting.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
        open.send(()).unwrap();
        assert_eq!(waiting.await, Ok(()));
        assert_eq!(events.which_handlers().await.unwrap().len(), 2, "the add that timed out was served");
        assert!(events.which_handlers().await.unwrap().contains(&held));

        // A manager that has ended answers its reason, to a cast and to a call.
        open.send(()).unwrap();
        manager.stop(StopReason::Graceful).await.unwrap();
        assert_eq!(manager.wait_stopped().await, StopReason::Graceful);
        assert_eq!(got(&mut seen).await, "terminate");
        assert_eq!(events.try_notify(5), Err(EventError::Ended(StopReason::Graceful)));
        assert_eq!(events.which_handlers().await, Err(EventError::Ended(StopReason::Graceful)));
    });
}
