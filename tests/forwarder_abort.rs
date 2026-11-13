#![allow(unused_imports)]
//! The panics of a timer's or stream's task at its actor's end. A
//! forwarder parked on a full mailbox owns two user values: a recurring
//! timer's tick and its factory, or a stream's item and the stream. When
//! both drops panic, the process does not abort, the actor's reason is the
//! first panic, and with the `tracing` feature the second is logged once,
//! naming the actor. A factory or a stream whose drop panics alone is the
//! task's panic, also while the thread is unwinding from an unrelated panic,
//! and a panic of the runtime itself in the task, followed by the factory's
//! drop panic, does not abort either. Each scenario runs in a child process
//! of this test binary, so an abort fails one test instead of ending the
//! binary.

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(feature = "tracing")]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, SpawnError, StopReason, StreamEvent,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);

/// Set right before the actor stops itself: from then on the parked value
/// and its owner panic when dropped. Each scenario runs alone in its own
/// process, so one flag serves it.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The text of the parked value's drop panic.
const PARKED: &str = "the parked value panicked";

/// The text of the owner's drop panic: the recurring factory's capture or
/// the stream.
const OWNER: &str = "the owner panicked";

/// Panics with [`OWNER`] when dropped once armed.
struct Owner;

impl Drop for Owner {
    fn drop(&mut self) {
        if ARMED.load(Ordering::SeqCst) {
            panic!("{OWNER}");
        }
    }
}

/// The `n`th message a forwarder built; from the third on it panics with
/// [`PARKED`] when dropped once armed. The first is handled, the second
/// fills the one slot of the mailbox, the third is parked in the send.
struct Item(usize);

impl Drop for Item {
    fn drop(&mut self) {
        if self.0 >= 3 && ARMED.load(Ordering::SeqCst) {
            panic!("{PARKED}");
        }
    }
}

/// How many items the forwarder built.
static BUILT: AtomicUsize = AtomicUsize::new(0);

fn next_item() -> Item {
    Item(BUILT.fetch_add(1, Ordering::SeqCst) + 1)
}

impl From<StreamEvent<()>> for Item {
    fn from(_: StreamEvent<()>) -> Self {
        next_item()
    }
}

/// A stream that always has an item and owns an [`Owner`].
struct Flooding(#[allow(dead_code)] Owner);

impl Stream for Flooding {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        Poll::Ready(Some(()))
    }
}

/// Starts a recurring timer or a stream whose items fill its one-slot
/// mailbox. Its first turn waits until the third item is built, parked in
/// the forwarder's send, then arms the drops and stops the actor.
struct Parked {
    stream: bool,
}

impl Actor for Parked {
    type Message = Item;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.stream {
            ctx.add_stream(Flooding(Owner));
        } else {
            let owner = Owner;
            ctx.every_with(Duration::from_millis(5), move || {
                let _ = &owner;
                next_item()
            })
            .await
            .expect("the timer starts");
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: Item, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        // The third item is built right before its send, with no await
        // between, and the second fills the mailbox while this turn runs.
        while BUILT.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        ARMED.store(true, Ordering::SeqCst);
        ctx.self_handle()
            .stop(StopReason::Graceful)
            .await
            .expect("accepted");
        Ok(())
    }
}

fn on(flavor: &str, body: impl Future<Output = ()>) {
    let runtime = match flavor {
        "current_thread" => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
        _ => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build(),
    }
    .expect("a runtime");
    runtime.block_on(body);
}

fn a_parked_forwarder_with_two_panicking_drops_ends_in_one_reason(stream: bool, flavor: &str) {
    #[cfg(feature = "tracing")]
    let logged = warnings::install();
    on(flavor, async {
        let actor = Parked { stream }
            .spawn()
            .with_config(ActorConfig::default().with_mailbox_capacity(1))
            .await
            .expect("the actor starts");
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends");
        assert_eq!(
            reason,
            StopReason::Failure(ActorError::Panic(PARKED.to_string())),
            "the parked value's panic is the end's"
        );
        #[cfg(feature = "tracing")]
        {
            let token = format!("actor_id={} ", actor.id());
            let lines: Vec<String> = logged
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains(&token) && line.contains(OWNER))
                .cloned()
                .collect();
            assert_eq!(
                lines.len(),
                1,
                "the owner's panic is logged once: {lines:?}"
            );
            assert!(lines[0].contains("discarded"), "{lines:?}");
        }
    });
}

#[test]
fn a_parked_stream_item_and_its_stream_both_panicking_end_in_one_reason_current_thread() {
    if common::in_a_child_process(
        "a_parked_stream_item_and_its_stream_both_panicking_end_in_one_reason_current_thread",
    ) {
        a_parked_forwarder_with_two_panicking_drops_ends_in_one_reason(true, "current_thread");
    }
}

/// A stream that never yields and owns an [`Owner`].
struct Silent(#[allow(dead_code)] Owner);

impl Stream for Silent {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        Poll::Pending
    }
}

/// Holds a recurring timer that never fires or a stream that never yields,
/// either owning an [`Owner`]. Answers each message with the last error its
/// timers and streams left.
struct Holder {
    stream: bool,
}

impl Actor for Holder {
    type Message = Item;
    type Response = Option<String>;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.stream {
            ctx.add_stream(Silent(Owner));
        } else {
            let owner = Owner;
            ctx.every_with(Duration::from_secs(3600), move || {
                let _ = &owner;
                next_item()
            })
            .await
            .expect("the timer starts");
        }
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: Item,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Option<String>> {
        Ok(ctx.last_forwarder_error().map(ToString::to_string))
    }
}

/// Set by a test whose log subscriber panics on every line that records an
/// ended stream's panic its forwarder did not report.
#[cfg(feature = "tracing")]
static PANIC_ON_ENDED: AtomicBool = AtomicBool::new(false);

/// Set by a test whose log subscriber panics once, on the first line that
/// says a panic was discarded.
#[cfg(feature = "tracing")]
static PANIC_ON_DISCARDED: AtomicBool = AtomicBool::new(false);

/// The text of that subscriber's panic.
#[cfg(feature = "tracing")]
const SUBSCRIBER: &str = "the log subscriber panicked";

/// Registers a recurring timer on a runtime without the time driver: the call
/// panics at its entry while the factory is held, and the factory's drop
/// panics during that unwind. Returns the actor's id and the text of the
/// start's failure.
fn a_timer_without_a_time_driver() -> (String, String) {
    ARMED.store(true, Ordering::SeqCst);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime without the time driver");
    let name = format!("entry-hold-{}", uuid::Uuid::new_v4());
    let error = runtime.block_on(async {
        match (Holder { stream: false }).spawn().named(name.clone()).await {
            Err(SpawnError::Init(error)) => error.to_string(),
            other => panic!("expected the start to fail, got {:?}", other.map(|_| ())),
        }
    });
    ARMED.store(false, Ordering::SeqCst);
    std::mem::forget(runtime);
    (name, error)
}

/// The entry check's panic and the factory's drop panic in one timer call do
/// not abort: the check's panic stands as the start's failure, and with the
/// `tracing` feature the factory's is logged once, naming the actor.
#[test]
fn an_entry_panic_and_a_factory_drop_panic_in_one_timer_call_do_not_abort() {
    if !common::in_a_child_process(
        "an_entry_panic_and_a_factory_drop_panic_in_one_timer_call_do_not_abort",
    ) {
        return;
    }
    #[cfg(feature = "tracing")]
    let logged = warnings::install();
    let (name, error) = a_timer_without_a_time_driver();
    assert!(
        error.contains("needs the Tokio time driver"),
        "the entry check's panic: {error}"
    );
    #[cfg(feature = "tracing")]
    {
        let token = format!("actor_id={name} ");
        let lines: Vec<String> = logged
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(&token) && line.contains(OWNER))
            .cloned()
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "the factory's panic is logged once: {lines:?}"
        );
    }
    #[cfg(not(feature = "tracing"))]
    let _ = name;
}

#[cfg(feature = "tracing")]
mod warnings {
    use std::fmt::Write as _;

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    use super::{Arc, Mutex, OnceLock};

    /// Collects every WARN event as one line of `name=value` pairs.
    #[derive(Clone, Default)]
    struct Warnings {
        lines: Arc<Mutex<Vec<String>>>,
    }

    /// Installs the one global subscriber of this process on first use and
    /// returns its lines.
    pub fn install() -> Arc<Mutex<Vec<String>>> {
        static GLOBAL: OnceLock<Warnings> = OnceLock::new();
        GLOBAL
            .get_or_init(|| {
                let warnings = Warnings::default();
                tracing::subscriber::set_global_default(warnings.clone())
                    .expect("no other global subscriber is installed in this process");
                warnings
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

    impl Subscriber for Warnings {
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
            let discarded = text.0.contains("discarded");
            let ended = text.0.contains("ended before reporting");
            self.lines.lock().unwrap().push(text.0);
            if discarded && super::PANIC_ON_DISCARDED.swap(false, super::Ordering::SeqCst) {
                panic!("{}", super::SUBSCRIBER);
            }
            if ended && super::PANIC_ON_ENDED.load(super::Ordering::SeqCst) {
                panic!("{}", super::SUBSCRIBER);
            }
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }
}
