#![cfg(feature = "tracing")]
//! An actor whose own value (a field of the actor, not queued mail) panics
//! when dropped is aborted by a `Kill`, on both runtime flavors. The drop is
//! caught: the actor reads `Kill`, the process does not abort, and exactly one
//! warning that names the abort records the panic, also when the `tracing`
//! subscriber itself panics on that warning, with a payload whose own drop
//! panics too. No message is queued, so the warning comes from the drop of the
//! actor's value and nothing else. Each scenario runs in a child process of
//! this test binary, because it installs the one global subscriber and a
//! failure may end the process.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);

/// The text of the warning that records a drop at the abort.
const ABORT_DROP: &str = "a drop at the abort or the unwind";

/// The text the value's drop panics with.
const DROPPED: &str = "the actor's value panicked when dropped";

/// Set right before the kill: from then on the value panics when dropped.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Panics when dropped once armed.
struct Loud;

impl Drop for Loud {
    fn drop(&mut self) {
        if ARMED.load(Ordering::SeqCst) {
            panic!("{DROPPED}");
        }
    }
}

/// Its turn never yields once parked, and its field panics when dropped.
struct Parked(#[allow(dead_code)] Loud);

impl Actor for Parked {
    type Message = Arc<Notify>;
    type Response = ();

    async fn handle(
        &mut self,
        entered: Arc<Notify>,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        entered.notify_one();
        std::future::pending::<()>().await;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Flavor {
    CurrentThread,
    MultiThread,
}

fn runtime(flavor: Flavor) -> tokio::runtime::Runtime {
    match flavor {
        Flavor::CurrentThread => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
        Flavor::MultiThread => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build(),
    }
    .expect("a runtime")
}

/// Kills a parked [`Parked`] actor whose value panics when dropped, and
/// checks its end once the runtime has dropped the task: it read `Kill`, and
/// exactly one warning that names the abort carries the panic text.
fn kill_a_parked_actor(flavor: Flavor, lines: &Mutex<Vec<String>>) {
    let rt = runtime(flavor);
    let reason = rt.block_on(async {
        let actor = Parked(Loud)
            .spawn()
            .kill_grace(Duration::from_millis(50))
            .await
            .expect("the actor starts");
        let entered = Arc::new(Notify::new());
        actor.notify(entered.clone()).await.expect("queued");
        timeout(BOUND, entered.notified())
            .await
            .expect("the turn parks");
        ARMED.store(true, Ordering::SeqCst);
        actor.stop(StopReason::Kill).await.expect("accepted");
        timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends")
    });
    // The task's drops finish when the runtime has dropped it.
    drop(rt);
    ARMED.store(false, Ordering::SeqCst);
    assert_eq!(reason, StopReason::Kill);
    let written: Vec<String> = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(ABORT_DROP))
        .cloned()
        .collect();
    assert_eq!(written.len(), 1, "one warning names the abort: {written:?}");
    assert!(
        written[0].contains(DROPPED),
        "the warning carries the panic text: {}",
        written[0]
    );
}

#[test]
fn a_drop_panic_of_the_actor_s_value_at_an_abort_is_logged_once_current_thread() {
    if !common::in_a_child_process(
        "a_drop_panic_of_the_actor_s_value_at_an_abort_is_logged_once_current_thread",
    ) {
        return;
    }
    let lines = subscriber::install(None, subscriber::Payload::Text);
    kill_a_parked_actor(Flavor::CurrentThread, &lines);
}

#[test]
fn a_drop_panic_of_the_actor_s_value_at_an_abort_is_logged_once_multi_thread() {
    if !common::in_a_child_process(
        "a_drop_panic_of_the_actor_s_value_at_an_abort_is_logged_once_multi_thread",
    ) {
        return;
    }
    let lines = subscriber::install(None, subscriber::Payload::Text);
    kill_a_parked_actor(Flavor::MultiThread, &lines);
}

#[test]
fn a_panicking_subscriber_at_the_drop_of_the_actor_s_value_does_not_abort_current_thread() {
    if !common::in_a_child_process(
        "a_panicking_subscriber_at_the_drop_of_the_actor_s_value_does_not_abort_current_thread",
    ) {
        return;
    }
    let lines = subscriber::install(Some(ABORT_DROP), subscriber::Payload::TwoBombs);
    kill_a_parked_actor(Flavor::CurrentThread, &lines);
}

#[test]
fn a_panicking_subscriber_at_the_drop_of_the_actor_s_value_does_not_abort_multi_thread() {
    if !common::in_a_child_process(
        "a_panicking_subscriber_at_the_drop_of_the_actor_s_value_does_not_abort_multi_thread",
    ) {
        return;
    }
    let lines = subscriber::install(Some(ABORT_DROP), subscriber::Payload::TwoBombs);
    kill_a_parked_actor(Flavor::MultiThread, &lines);
}

/// The same, with a subscriber whose panic payload is a plain text.
#[test]
fn a_text_panic_of_the_subscriber_at_the_drop_of_the_actor_s_value_is_contained() {
    if !common::in_a_child_process(
        "a_text_panic_of_the_subscriber_at_the_drop_of_the_actor_s_value_is_contained",
    ) {
        return;
    }
    let lines = subscriber::install(Some(ABORT_DROP), subscriber::Payload::Text);
    kill_a_parked_actor(Flavor::CurrentThread, &lines);
}

/// A child that does nothing.
struct Leaf;

impl Actor for Leaf {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A supervisor whose two child factories panic when dropped, and whose turn
/// never yields once parked.
struct ParkedParent;

impl Actor for ParkedParent {
    type Message = Arc<Notify>;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for _ in 0..2 {
            let capture = Loud;
            ctx.spawn_child(move || {
                let _ = &capture;
                Leaf
            })
            .await?;
        }
        Ok(())
    }

    async fn handle(
        &mut self,
        entered: Arc<Notify>,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        entered.notify_one();
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// The line that records a panic a step discards.
const DISCARDED: &str = "this panic is discarded";

/// Kills a parked [`ParkedParent`]: the abort drops its context, whose two
/// factories both panic. The first panic is kept and the second discarded, and
/// the warning that records the discarded one names the actor that was
/// aborted.
fn kill_a_parked_supervisor(flavor: Flavor, lines: &Mutex<Vec<String>>) {
    let rt = runtime(flavor);
    let (reason, id) = rt.block_on(async {
        let actor = ParkedParent
            .spawn()
            .kill_grace(Duration::from_millis(50))
            .supervisor()
            .await
            .expect("the actor starts");
        let entered = Arc::new(Notify::new());
        actor.notify(entered.clone()).await.expect("queued");
        timeout(BOUND, entered.notified())
            .await
            .expect("the turn parks");
        ARMED.store(true, Ordering::SeqCst);
        actor.stop(StopReason::Kill).await.expect("accepted");
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends");
        (reason, actor.id().to_string())
    });
    drop(rt);
    ARMED.store(false, Ordering::SeqCst);
    assert_eq!(reason, StopReason::Kill);
    let lines = lines.lock().unwrap();
    let discarded: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains(DISCARDED))
        .collect();
    assert_eq!(discarded.len(), 1, "one discarded panic: {discarded:?}");
    assert!(
        discarded[0].contains(&format!("actor_id={id} ")),
        "the warning names the aborted actor {id}: {}",
        discarded[0]
    );
}

#[test]
fn the_discarded_drop_panic_of_an_aborted_context_names_the_actor_current_thread() {
    if !common::in_a_child_process(
        "the_discarded_drop_panic_of_an_aborted_context_names_the_actor_current_thread",
    ) {
        return;
    }
    let lines = subscriber::install(None, subscriber::Payload::Text);
    kill_a_parked_supervisor(Flavor::CurrentThread, &lines);
}

#[test]
fn the_discarded_drop_panic_of_an_aborted_context_names_the_actor_multi_thread() {
    if !common::in_a_child_process(
        "the_discarded_drop_panic_of_an_aborted_context_names_the_actor_multi_thread",
    ) {
        return;
    }
    let lines = subscriber::install(None, subscriber::Payload::Text);
    kill_a_parked_supervisor(Flavor::MultiThread, &lines);
}

mod subscriber {
    use std::fmt::Write as _;

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    use super::{Arc, Mutex};

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
    struct Recording {
        lines: Arc<Mutex<Vec<String>>>,
        panic_on: Option<&'static str>,
        payload: Payload,
    }

    struct Text(String);

    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    /// Installs the one global subscriber of this process and returns its
    /// lines.
    pub fn install(panic_on: Option<&'static str>, payload: Payload) -> Arc<Mutex<Vec<String>>> {
        let lines = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::set_global_default(Recording {
            lines: lines.clone(),
            panic_on,
            payload,
        })
        .expect("no other global subscriber is installed in this process");
        lines
    }

    impl Subscriber for Recording {
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
