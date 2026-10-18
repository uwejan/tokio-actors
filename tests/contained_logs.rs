#![cfg(feature = "tracing")]
//! A `tracing` subscriber that panics is contained at every log line of the
//! crate: at the drops of an aborted actor's task, at the line that records
//! a drop panic leaving a decided reason as it stands, and at the line that
//! records a panic the end discards. The process does not abort, and the
//! actor's reason is what it would be with a quiet subscriber, also when
//! the subscriber's panic payload itself panics when dropped. Each scenario
//! runs in a child process of this test binary, so an abort fails one test
//! instead of ending the binary.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);

/// Set right before the actor ends: from then on the values below panic
/// when dropped. Each scenario runs alone in its own process.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The text of every drop panic below, followed by what dropped.
const DROPPED: &str = "a value panicked when dropped";

/// Panics with [`DROPPED`] and its name when dropped once armed.
struct Loud(&'static str);

impl Drop for Loud {
    fn drop(&mut self) {
        if ARMED.load(Ordering::SeqCst) {
            panic!("{DROPPED}: {}", self.0);
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

// ---------------------------------------------------------------------------
// The drops of an aborted actor's task
// ---------------------------------------------------------------------------

enum ParkMsg {
    /// Holds the turn forever once it has signalled.
    Park(Arc<Notify>),
    /// Owns a value whose drop panics.
    Queued(#[allow(dead_code)] Loud),
}

/// Its value panics when dropped.
struct Parked(#[allow(dead_code)] Loud);

impl Actor for Parked {
    type Message = ParkMsg;
    type Response = ();

    async fn handle(&mut self, msg: ParkMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let ParkMsg::Park(entered) = msg {
            entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

/// The line that records a drop at the abort.
const ABORT_DROP: &str = "a drop at the abort or the unwind";

/// An actor whose turn never yields is killed: the abort drops its queued
/// message and its value, both panicking, and the subscriber panics on each
/// line that records them. The process does not abort, and the actor reads
/// `Kill`.
#[test]
fn a_panicking_subscriber_at_an_aborted_task_s_drops_does_not_abort() {
    if !common::in_a_child_process(
        "a_panicking_subscriber_at_an_aborted_task_s_drops_does_not_abort",
    ) {
        return;
    }
    let lines = subscriber::install(Some(ABORT_DROP));
    kill_a_parked_actor(&lines);
}

/// [`a_panicking_subscriber_at_an_aborted_task_s_drops_does_not_abort`],
/// with a subscriber whose panic payload holds two values that each panic
/// when dropped. A drop of that payload would panic again while unwinding,
/// which aborts the process; the crate never drops it.
#[test]
fn a_subscriber_panic_whose_payload_panics_when_dropped_does_not_abort() {
    if !common::in_a_child_process(
        "a_subscriber_panic_whose_payload_panics_when_dropped_does_not_abort",
    ) {
        return;
    }
    let lines = subscriber::install_with(Some(ABORT_DROP), subscriber::Payload::TwoBombs);
    kill_a_parked_actor(&lines);
}

/// Kills a [`Parked`] actor whose value and queued message panic when
/// dropped: it reads `Kill`, and both lines that record the drops are
/// written.
fn kill_a_parked_actor(lines: &Mutex<Vec<String>>) {
    let reason = runtime().block_on(async {
        let actor = Parked(Loud("the value"))
            .spawn()
            .kill_grace(Duration::from_millis(50))
            .await
            .expect("the actor starts");
        let entered = Arc::new(Notify::new());
        actor
            .notify(ParkMsg::Park(entered.clone()))
            .await
            .expect("queued");
        timeout(BOUND, entered.notified())
            .await
            .expect("the turn parks");
        actor
            .notify(ParkMsg::Queued(Loud("the queued message")))
            .await
            .expect("queued");
        ARMED.store(true, Ordering::SeqCst);
        actor.stop(StopReason::Kill).await.expect("accepted");
        timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends")
    });
    ARMED.store(false, Ordering::SeqCst);
    assert_eq!(reason, StopReason::Kill);
    let written = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(ABORT_DROP))
        .count();
    assert_eq!(written, 2, "both lines were written");
}

// ---------------------------------------------------------------------------
// A drop panic that leaves a decided reason as it stands
// ---------------------------------------------------------------------------

/// A stream that never yields and owns a [`Loud`].
struct Silent(#[allow(dead_code)] Loud);

impl Stream for Silent {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        Poll::Pending
    }
}

struct Never;

impl From<tokio_actors::StreamEvent<()>> for Never {
    fn from(_: tokio_actors::StreamEvent<()>) -> Self {
        Never
    }
}

/// Holds a stream whose drop panics, still registered when it ends.
struct Streaming;

impl Actor for Streaming {
    type Message = Never;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.add_stream(Silent(Loud("the stream")));
        Ok(())
    }

    async fn handle(&mut self, _msg: Never, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A `Kill` read at a turn boundary decides the reason; the stream's drop
/// at the end then panics, and the subscriber panics on the line that
/// records it. The reason stays `Kill`.
#[test]
fn a_panicking_subscriber_on_a_drop_panic_after_a_kill_leaves_the_kill() {
    if !common::in_a_child_process(
        "a_panicking_subscriber_on_a_drop_panic_after_a_kill_leaves_the_kill",
    ) {
        return;
    }
    let lines = subscriber::install(Some("before the actor's reason was final"));
    let reason = runtime().block_on(async {
        let actor = Streaming.spawn().await.expect("the actor starts");
        ARMED.store(true, Ordering::SeqCst);
        actor.stop(StopReason::Kill).await.expect("accepted");
        timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends")
    });
    ARMED.store(false, Ordering::SeqCst);
    assert_eq!(reason, StopReason::Kill);
    let written = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("before the actor's reason was final"))
        .count();
    assert_eq!(written, 1, "the line was written");
}

// ---------------------------------------------------------------------------
// A panic the end discards
// ---------------------------------------------------------------------------

/// A child that does nothing.
struct Leaf;

impl Actor for Leaf {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A supervisor whose two children's factories panic when dropped.
struct Parent;

impl Actor for Parent {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for name in ["the first factory", "the second factory"] {
            let capture = Loud(name);
            ctx.spawn_child(move || {
                let _ = &capture;
                Leaf
            })
            .await?;
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// The teardown drops both factories: it keeps the first panic and
/// discards the second, and the subscriber panics on the line that records
/// the discarded one. The process does not abort, and the supervisor's
/// reason is the kept panic, as with a quiet subscriber.
#[test]
fn a_panicking_subscriber_on_a_discarded_panic_at_the_end_does_not_abort() {
    if !common::in_a_child_process(
        "a_panicking_subscriber_on_a_discarded_panic_at_the_end_does_not_abort",
    ) {
        return;
    }
    let lines = subscriber::install(Some("this panic is discarded"));
    let reason = runtime().block_on(async {
        let actor = Parent.spawn().supervisor().await.expect("the actor starts");
        ARMED.store(true, Ordering::SeqCst);
        actor.stop(StopReason::Graceful).await.expect("accepted");
        timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends")
    });
    ARMED.store(false, Ordering::SeqCst);
    match &reason {
        StopReason::Failure(ActorError::Panic(text)) if text.starts_with(DROPPED) => {}
        other => panic!("the kept factory panic is the reason: {other:?}"),
    }
    let written = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("this panic is discarded"))
        .count();
    assert_eq!(written, 1, "the line was written");
}

mod subscriber {
    use std::fmt::Write as _;

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    use super::{Arc, Mutex, OnceLock};

    /// What the subscriber panics with.
    #[derive(Clone, Copy)]
    pub enum Payload {
        /// A `String`.
        Text,
        /// A value of two fields that each panic when dropped.
        TwoBombs,
    }

    /// Panics when dropped.
    struct Bomb;

    impl Drop for Bomb {
        fn drop(&mut self) {
            panic!("a field of the subscriber's panic payload panicked when dropped");
        }
    }

    /// A panic payload whose drop panics once per field.
    struct TwoBombs(#[allow(dead_code)] Bomb, #[allow(dead_code)] Bomb);

    /// Collects every WARN event as one line of `name=value` pairs, and
    /// panics with `payload` after recording a line that contains
    /// `panic_on`.
    #[derive(Clone)]
    struct Panicking {
        lines: Arc<Mutex<Vec<String>>>,
        panic_on: Option<&'static str>,
        payload: Payload,
    }

    /// Installs the one global subscriber of this process and returns its
    /// lines.
    pub fn install(panic_on: Option<&'static str>) -> Arc<Mutex<Vec<String>>> {
        install_with(panic_on, Payload::Text)
    }

    /// [`install`], panicking with `payload`.
    pub fn install_with(
        panic_on: Option<&'static str>,
        payload: Payload,
    ) -> Arc<Mutex<Vec<String>>> {
        static GLOBAL: OnceLock<Panicking> = OnceLock::new();
        GLOBAL
            .get_or_init(|| {
                let subscriber = Panicking {
                    lines: Arc::default(),
                    panic_on,
                    payload,
                };
                tracing::subscriber::set_global_default(subscriber.clone())
                    .expect("no other global subscriber is installed in this process");
                subscriber
            })
            .lines
            .clone()
    }

    struct Text(String);

    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    impl Subscriber for Panicking {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == Level::WARN
        }

        fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut text = Text(String::new());
            event.record(&mut text);
            let hit = self.panic_on.is_some_and(|needle| text.0.contains(needle));
            self.lines.lock().unwrap().push(text.0);
            if hit {
                match self.payload {
                    Payload::Text => panic!("the log subscriber panicked"),
                    Payload::TwoBombs => std::panic::panic_any(TwoBombs(Bomb, Bomb)),
                }
            }
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }
}
