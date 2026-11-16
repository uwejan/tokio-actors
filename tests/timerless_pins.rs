//! What a state machine, an event manager and an application controller do on
//! a runtime built without the time driver: each call or setter that needs the
//! driver refuses or panics at its entry, and the unbounded form runs.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;
use tokio_actors::application::{
    AppError, AppRoot, AppState, Application, Applications, StartError,
};
use tokio_actors::event::{EventError, EventHandler, EventManager, EventManagerRef, Handled};
use tokio_actors::statem::{
    EnterContext, StateMachine, StatemActor, StatemContext, StatemEvent, Transition,
};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, ActorSystem, RestartType, StopReason,
};

/// A machine whose events ask for a time-out, in the event function or in the
/// enter call, and say what they did on either side of the setter.
struct Setter(mpsc::UnboundedSender<&'static str>);

#[rustfmt::skip]
impl StateMachine for Setter {
    type State = u8;
    type Event = u8;
    type Call = u16;
    type Reply = u16;
    type TimerName = ();

    fn handle_event(&mut self, _: &u8, event: StatemEvent<Self>, cx: &mut StatemContext<'_, Self>) -> Transition<u8> {
        let after = Duration::from_secs(60);
        let _ = self.0.send("before");
        match event {
            StatemEvent::Cast(0) => cx.event_timeout(after, 9),
            StatemEvent::Cast(1) => cx.state_timeout(after, 9),
            StatemEvent::Cast(2) => cx.timeout((), after, 9),
            StatemEvent::Cast(_) => return Transition::Next(1),
            StatemEvent::Call(c, to) => cx.reply(to, c * 2),
            _ => {}
        }
        let _ = self.0.send("after");
        Transition::Keep
    }

    fn on_enter(&mut self, _: &u8, new: &u8, cx: &mut EnterContext<'_, Self>) {
        if *new == 1 { cx.event_timeout(Duration::from_secs(60), 9); }
    }
}

struct Noop;

impl EventHandler for Noop {
    type Event = u8;
    type Call = ();
    type Reply = ();

    fn handle_event(&mut self, _: &u8) -> Handled {
        Handled::Keep
    }
}

struct App;

impl Application for App {
    fn name(&self) -> &str {
        "timerless"
    }

    fn start(
        &mut self,
        system: &Arc<ActorSystem>,
    ) -> impl Future<Output = Result<AppRoot, StartError>> + Send {
        let system = system.clone();
        async move { Ok(AppRoot::of(&Idle.spawn().on_system(&system).await?)) }
    }
}

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn without_timers() -> Runtime {
    Builder::new_current_thread().build().unwrap()
}

/// The text of the panic a spawned call ended with.
fn panic_text<T>(joined: Result<T, tokio::task::JoinError>) -> String {
    match joined {
        Ok(_) => panic!("the call was expected to panic"),
        Err(err) => *err
            .into_panic()
            .downcast::<String>()
            .expect("a formatted panic"),
    }
}

#[test]
#[rustfmt::skip]
fn timerless_pins() {
    // The three time-out setters, in the event function and in the enter call, panic at the call.
    let actor_runtime = without_timers();
    actor_runtime.block_on(async {
        for event in 0..4u8 {
            let (log, mut seen) = mpsc::unbounded_channel();
            let machine = StatemActor::new(Setter(log), 0).spawn().await.unwrap();
            machine.cast(event).await.unwrap();
            let reason = machine.wait_stopped().await;
            assert!(matches!(&reason, StopReason::Failure(ActorError::Panic(text)) if text.contains("enable_time")), "{event}: {reason:?}");
            let lines: Vec<_> = std::iter::from_fn(|| seen.try_recv().ok()).collect();
            assert_eq!(lines, ["before"], "{event}: the setter panics at the call");
        }
    });

    // A bounded call needs the time driver of the caller's runtime; an unbounded one does not.
    let host = Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let (log, _seen) = mpsc::unbounded_channel();
    let machine = host.block_on(async { StatemActor::new(Setter(log), 0).spawn().await.unwrap() });
    let manager = host.block_on(async { EventManager::<u8>::new().spawn().await.unwrap() });
    let caller = without_timers();
    caller.block_on(async {
        let machine = machine.clone();
        assert_eq!(machine.call(2).await.unwrap(), 4);
        let bounded = tokio::spawn(async move { machine.call_timeout(2, Duration::from_secs(60)).await });
        assert!(panic_text(bounded.await).contains("enable_time"));

        // The event manager: `call` has a default bound, the other calls none, and `unbounded` lifts every bound.
        let events = EventManagerRef::of(&manager);
        let id = events.add_handler(Noop).await.unwrap();
        assert_eq!(events.call(id, ()).await, Err(EventError::NoTimeDriver));
        assert_eq!(events.clone().unbounded().call(id, ()).await, Err(EventError::NoReply));
        assert_eq!(events.clone().timeout(Duration::from_secs(60)).which_handlers().await, Err(EventError::NoTimeDriver));
        assert_eq!(events.which_handlers().await.unwrap(), [id]);
    });
    drop(caller);

    // The controller: `which_applications` has a default bound, and a start needs the driver whatever the bound.
    without_timers().block_on(async {
        let apps = Applications::spawn().await.unwrap();
        apps.load(App).await.unwrap();
        assert!(matches!(apps.which_applications().await, Err(AppError::NoTimeDriver)));
        let apps = apps.unbounded();
        assert!(matches!(apps.start("timerless", RestartType::Temporary).await, Err(AppError::NoTimeDriver)));
        let rows = apps.which_applications().await.unwrap();
        assert_eq!((rows.len(), rows[0].state), (1, AppState::Loaded));
    });
}
