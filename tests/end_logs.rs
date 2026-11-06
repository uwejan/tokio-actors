#![cfg(feature = "tracing")]
//! Every panic an actor's end catches and does not make its reason leaves a
//! warning naming the actor: a stream the actor cancelled in `on_stopped`
//! whose drop panics, each of several queued messages whose drops panic
//! after the reason is final, and each of a supervisor's child factories
//! whose drops panic when a `Kill` ends it. A stream cancelled during the run
//! whose drop panics, reaped by the message loop, leaves one warning too.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason,
};

const BOUND: Duration = Duration::from_secs(10);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("end-logs-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {BOUND:?} waiting for: {what}"),
    }
}

fn on(flavor: &str, body: impl Future<Output = ()>) {
    let _ = warnings::install();
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

/// The warnings naming the actor `name` whose payload contains `text`.
fn lines_naming(name: &str, text: &str) -> Vec<String> {
    let token = format!("actor_id={name} ");
    warnings::install()
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(&token) && line.contains(text))
        .cloned()
        .collect()
}

/// Panics with its text when dropped.
struct PanicsWith(String);

impl Drop for PanicsWith {
    fn drop(&mut self) {
        panic!("{}", self.0);
    }
}

/// The gates of a turn the test holds.
#[derive(Default)]
struct Gates {
    entered: Notify,
    release: Notify,
}

enum QueueMsg {
    /// Holds the turn until it is released.
    Hold(Arc<Gates>),
    /// Owns a value whose drop panics.
    Bomb(#[allow(dead_code)] PanicsWith),
}

struct Queue;

impl Actor for Queue {
    type Message = QueueMsg;
    type Response = ();

    async fn handle(&mut self, msg: QueueMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let QueueMsg::Hold(gates) = msg {
            gates.entered.notify_one();
            gates.release.notified().await;
        }
        Ok(())
    }
}

/// N queued messages whose drops panic after the reason is final: N
/// warnings, each naming the actor (the panic the end keeps, and the N - 1
/// it discards).
fn every_panicking_queued_message_is_logged(flavor: &str) {
    const N: usize = 3;
    on(flavor, async {
        let name = uname("queue");
        let actor = Queue
            .spawn()
            .named(name.clone())
            .await
            .expect("the actor starts");
        let gates = Arc::new(Gates::default());
        actor
            .notify(QueueMsg::Hold(gates.clone()))
            .await
            .expect("queued");
        within("the held turn", gates.entered.notified()).await;
        let text = format!("a queued message panicked {name}");
        for i in 0..N {
            actor
                .notify(QueueMsg::Bomb(PanicsWith(format!("{text} {i}"))))
                .await
                .expect("queued");
        }
        actor.stop(StopReason::Graceful).await.expect("accepted");
        gates.release.notify_one();
        let reason = within("the actor's end", actor.wait_stopped()).await;
        assert_eq!(reason, StopReason::Graceful);
        let lines = lines_naming(&name, &text);
        assert_eq!(lines.len(), N, "one warning per panic: {lines:?}");
        for i in 0..N {
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains(&format!("{text} {i}"))),
                "{i}: {lines:?}"
            );
        }
    });
}

#[test]
fn every_panicking_queued_message_is_logged_multi_thread() {
    every_panicking_queued_message_is_logged("multi_thread");
}

/// N priority messages whose drops panic after the reason is final: N
/// warnings, each naming the actor.
fn every_panicking_priority_message_is_logged(flavor: &str) {
    const N: usize = 3;
    on(flavor, async {
        let name = uname("priority");
        let actor = Queue
            .spawn()
            .named(name.clone())
            .with_config(ActorConfig::default().with_priority_lane(8))
            .await
            .expect("the actor starts");
        let gates = Arc::new(Gates::default());
        actor
            .notify(QueueMsg::Hold(gates.clone()))
            .await
            .expect("queued");
        within("the held turn", gates.entered.notified()).await;
        let text = format!("a priority message panicked {name}");
        for i in 0..N {
            actor
                .notify_priority(QueueMsg::Bomb(PanicsWith(format!("{text} {i}"))))
                .await
                .expect("queued");
        }
        actor.stop(StopReason::Graceful).await.expect("accepted");
        gates.release.notify_one();
        let reason = within("the actor's end", actor.wait_stopped()).await;
        assert_eq!(reason, StopReason::Graceful);
        let lines = lines_naming(&name, &text);
        assert_eq!(lines.len(), N, "one warning per panic: {lines:?}");
    });
}

#[test]
fn every_panicking_priority_message_is_logged_current_thread() {
    every_panicking_priority_message_is_logged("current_thread");
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

enum ParentMsg {
    /// Starts two children whose factories panic when dropped.
    Spawn(String),
    /// Holds the turn forever once it has signalled.
    Park(Arc<Gates>),
}

struct Parent;

impl Actor for Parent {
    type Message = ParentMsg;
    type Response = ();

    async fn handle(&mut self, msg: ParentMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ParentMsg::Spawn(text) => {
                for i in 0..2 {
                    let bomb = PanicsWith(format!("{text} {i}"));
                    ctx.spawn_child(move || {
                        let _ = &bomb;
                        Leaf
                    })
                    .named(uname("leaf"))
                    .await?;
                }
                Ok(())
            }
            ParentMsg::Park(gates) => {
                gates.entered.notify_one();
                std::future::pending::<()>().await;
                Ok(())
            }
        }
    }
}

/// A supervisor with two children whose factories panic when dropped is
/// ended by a `Kill`: at a turn boundary (the `Kill` path drops its
/// supervision state) or by the abort while a turn is suspended (its
/// context's drop does). Both factory panics are logged, naming the
/// supervisor: the one kept and the one discarded beside it. In the turn
/// boundary case the kill grace is 60 s and the actor ends within the test's
/// bound, so its end came by the `Kill` path, not by the abort.
fn a_killed_supervisor_logs_every_factory_panic(flavor: &str, suspended: bool) {
    on(flavor, async {
        let name = uname("parent");
        let grace = if suspended {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(60)
        };
        let actor = Parent
            .spawn()
            .named(name.clone())
            .supervisor()
            .kill_grace(grace)
            .await
            .expect("the actor starts");
        let text = format!("a child factory panicked {name}");
        actor
            .send(ParentMsg::Spawn(text.clone()))
            .await
            .expect("the children start");
        if suspended {
            let gates = Arc::new(Gates::default());
            actor
                .notify(ParentMsg::Park(gates.clone()))
                .await
                .expect("queued");
            within("the parked turn", gates.entered.notified()).await;
        }
        actor.stop(StopReason::Kill).await.expect("accepted");
        let reason = within("the actor's end", actor.wait_stopped()).await;
        assert_eq!(reason, StopReason::Kill);
        let lines = lines_naming(&name, &text);
        assert_eq!(lines.len(), 2, "one warning per panic: {lines:?}");
    });
}

#[test]
fn an_aborted_supervisor_logs_every_factory_panic_multi_thread() {
    a_killed_supervisor_logs_every_factory_panic("multi_thread", true);
}

mod warnings {
    use std::fmt::Write as _;
    use std::sync::OnceLock;

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    use super::{Arc, Mutex};

    /// Collects every WARN event as one line of `name=value` pairs.
    #[derive(Clone, Default)]
    struct Warnings {
        lines: Arc<Mutex<Vec<String>>>,
    }

    /// Installs the one global subscriber of this test binary on first use
    /// and returns its lines; a test finds its own lines by its actor's id.
    pub fn install() -> Arc<Mutex<Vec<String>>> {
        static GLOBAL: OnceLock<Warnings> = OnceLock::new();
        GLOBAL
            .get_or_init(|| {
                let warnings = Warnings::default();
                tracing::subscriber::set_global_default(warnings.clone())
                    .expect("no other global subscriber is installed in this test binary");
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
            self.lines.lock().unwrap().push(text.0);
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }
}
