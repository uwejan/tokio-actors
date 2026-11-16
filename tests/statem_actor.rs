//! A state machine under `StatemActor`: the start gate and the first enter call,
//! the verbs of the handle, notices mapped to events, a continued message and
//! the stops, each row observed through the machine's own log channel.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio_actors::actor::{context::ActorContext, ActorExt};
use tokio_actors::statem::{
    EnterContext, StateMachine, StatemActor, StatemContext, StatemEvent, StatemMessage, Transition,
};
use tokio_actors::{
    ActorConfig, ActorError, ActorHandle, ActorResult, AskError, SpawnError, StopReason,
    StreamEvent, TrySendError,
};

mod common;

enum Ev {
    Tag(u8),
    Chain,
    Kill,
    Down,
    Data(u8),
    End,
}

impl From<StreamEvent<u8>> for Ev {
    fn from(item: StreamEvent<u8>) -> Self {
        match item {
            StreamEvent::Data(n) => Ev::Data(n),
            StreamEvent::Finished => Ev::End,
        }
    }
}

type Handle = ActorHandle<StatemActor<Probe>>;

/// Reports every hook it runs through `log`; the channel closes when the
/// machine is dropped.
struct Probe {
    log: mpsc::UnboundedSender<String>,
    fail_init: bool,
    watch: Option<Handle>,
    stream: bool,
}

impl Probe {
    fn say(&self, what: impl Into<String>) {
        let _ = self.log.send(what.into());
    }
}

#[rustfmt::skip]
impl StateMachine for Probe {
    type State = u8;
    type Event = Ev;
    type Call = u16;
    type Reply = u16;
    type TimerName = ();

    async fn init(&mut self, ctx: &mut ActorContext<StatemActor<Self>>) -> ActorResult<()> {
        self.say("init");
        if self.fail_init { return Err(ActorError::user("no start")); }
        if let Some(watched) = &self.watch { ctx.monitor(watched, |_| StatemMessage::cast(Ev::Down)).unwrap(); }
        if self.stream { ctx.add_stream(tokio_stream::iter([5u8])); }
        Ok(())
    }

    fn handle_event(&mut self, _: &u8, event: StatemEvent<Self>, cx: &mut StatemContext<'_, Self>) -> Transition<u8> {
        match event {
            StatemEvent::Cast(Ev::Tag(n)) | StatemEvent::Internal(Ev::Tag(n)) => self.say(format!("t{n}")),
            StatemEvent::Cast(Ev::Chain) => {
                let _ = cx.next_event(Ev::Tag(1));
                cx.actor().unwrap().continue_with(StatemMessage::cast(Ev::Tag(2)));
            }
            StatemEvent::Cast(Ev::Kill) => return Transition::Stop(StopReason::Kill),
            StatemEvent::Cast(Ev::Down) => self.say("down"),
            StatemEvent::Cast(Ev::Data(n)) => self.say(format!("d{n}")),
            StatemEvent::Cast(Ev::End) => self.say("fin"),
            _ => {}
        }
        Transition::Keep
    }

    fn on_enter(&mut self, _: &u8, _: &u8, _: &mut EnterContext<'_, Self>) { self.say("enter"); }

    fn terminate(&mut self, reason: &StopReason, _: &u8) {
        self.say(if matches!(reason, StopReason::Failure(_)) { "term failure".to_string() } else { format!("term {reason:?}") });
    }
}

type Log = mpsc::UnboundedReceiver<String>;

fn probe(fail_init: bool, watch: Option<Handle>, stream: bool) -> (Probe, Log) {
    let (log, seen) = mpsc::unbounded_channel();
    (
        Probe {
            log,
            fail_init,
            watch,
            stream,
        },
        seen,
    )
}

/// The next line of the log, or `None` once the machine is gone.
async fn next(log: &mut Log) -> Option<String> {
    tokio::time::timeout(Duration::from_secs(60), log.recv())
        .await
        .expect("a hang")
}

async fn lines(log: &mut Log, want: &[&str]) {
    for line in want {
        assert_eq!(next(log).await.as_deref(), Some(*line));
    }
}

async fn ended(handle: &Handle) -> StopReason {
    tokio::time::timeout(Duration::from_secs(60), handle.wait_stopped())
        .await
        .expect("the machine ends")
}

#[test]
#[rustfmt::skip]
fn statem_actor_rows() {
    common::run_current_thread_only(async {
        // An init that fails: the machine never starts, runs no enter call and no terminate.
        let (m, mut log) = probe(true, None, false);
        let started = StatemActor::new(m, 0).spawn().await;
        assert!(matches!(started, Err(SpawnError::Init(_))), "{started:?}");
        lines(&mut log, &["init"]).await;
        assert_eq!(next(&mut log).await, None);

        // The first enter call runs after init; a continued message follows the inserted events of its call.
        let (m, mut log) = probe(false, None, false);
        let h = StatemActor::new(m, 0).spawn().await.unwrap();
        lines(&mut log, &["init", "enter"]).await;
        h.cast(Ev::Chain).await.unwrap();
        lines(&mut log, &["t1", "t2"]).await;

        // `send` answers once the event is handled.
        h.send(StatemMessage::cast(Ev::Tag(3))).await.unwrap();
        assert_eq!(log.try_recv().as_deref(), Ok("t3"));

        // A notice mapped through `StatemMessage::cast` and a stream item arrive as casts.
        let (m, mut watcher_log) = probe(false, Some(h.clone()), true);
        let watcher = StatemActor::new(m, 0).spawn().await.unwrap();
        lines(&mut watcher_log, &["init", "enter", "d5", "fin"]).await;
        h.stop(StopReason::Graceful).await.unwrap();
        lines(&mut watcher_log, &["down"]).await;
        lines(&mut log, &["term Graceful"]).await;

        // A call made after the machine ended is answered Closed, with nothing queued.
        assert_eq!(ended(&h).await, StopReason::Graceful);
        assert!(matches!(h.call(1).await, Err(AskError::Closed)));
        drop(watcher);

        // `try_cast` never waits, and `notify_priority` rides the priority lane ahead of the mailbox.
        let (m, mut log) = probe(false, None, false);
        let h = StatemActor::new(m, 0).spawn().with_config(ActorConfig::default().with_priority_lane(8)).await.unwrap();
        lines(&mut log, &["init", "enter"]).await;
        h.try_cast(Ev::Tag(4)).unwrap();
        h.try_notify_priority(StatemMessage::cast(Ev::Tag(5))).unwrap();
        let full = (0..80).filter(|_| matches!(h.try_cast(Ev::Tag(9)), Err(TrySendError::Full))).count();
        assert!(full > 0, "a full mailbox answers at once");
        lines(&mut log, &["t5", "t4"]).await;

        // Stop(Kill) is refused and fails the machine, terminate included; a Kill from outside runs no terminate.
        let (m, mut log) = probe(false, None, false);
        let h = StatemActor::new(m, 0).spawn().await.unwrap();
        h.cast(Ev::Kill).await.unwrap();
        assert!(matches!(ended(&h).await, StopReason::Failure(_)));
        lines(&mut log, &["init", "enter", "term failure"]).await;
        let (m, mut log) = probe(false, None, false);
        let h = StatemActor::new(m, 0).spawn().await.unwrap();
        h.stop(StopReason::Kill).await.unwrap();
        assert_eq!(ended(&h).await, StopReason::Kill);
        lines(&mut log, &["init", "enter"]).await;
        assert_eq!(next(&mut log).await, None, "no terminate after a kill");
    });
}
