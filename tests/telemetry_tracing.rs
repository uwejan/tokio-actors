#![cfg(feature = "tracing")]
//! The `tracing` output of a system names its actor. Every actor-scoped log
//! statement the crate can trigger carries the actor's id, name, incarnation
//! and system as fields. The report of a child that ended unexpectedly while
//! its supervisor stopped it is an error on the `tokio_actors::supervisor`
//! target. The per-incarnation span is created only when a subscriber is
//! interested in it at the spawn, has no parent, and is entered on every poll
//! of the actor's task, so an event written after an await carries it.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;
use tracing::field::{Field, Visit};
use tracing::subscriber::Interest;
use tracing::{span, Event, Level, Metadata, Subscriber};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorResult, ActorSystem, RestartType, StopReason, StreamEvent,
    SystemConfig,
};

const BOUND: Duration = Duration::from_secs(20);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "telemetry-tracing-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

// ---------------------------------------------------------------------------
// The capturing subscriber
// ---------------------------------------------------------------------------

/// One recorded event.
#[derive(Clone, Debug)]
struct Record {
    level: Level,
    target: String,
    message: String,
    fields: BTreeMap<String, String>,
    /// The span this thread had entered innermost when the event was written.
    span: Option<u64>,
}

#[derive(Default)]
struct Text {
    message: String,
    fields: BTreeMap<String, String>,
}

impl Visit for Text {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let mut text = String::new();
        let _ = write!(text, "{value:?}");
        if field.name() == "message" {
            self.message = text;
        } else {
            self.fields.insert(field.name().to_string(), text);
        }
    }
}

struct Capture;

static EVENTS: Mutex<Vec<Record>> = Mutex::new(Vec::new());
/// The fields of a span and whether it has no parent.
type SpanTable = Mutex<HashMap<u64, (BTreeMap<String, String>, bool)>>;

static SPANS: OnceLock<SpanTable> = OnceLock::new();
static NEXT_SPAN: AtomicU64 = AtomicU64::new(1);
static SPANS_CREATED: AtomicUsize = AtomicUsize::new(0);
static ENTERS: AtomicUsize = AtomicUsize::new(0);
/// Whether the subscriber is interested in spans.
static SPANS_WANTED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static ENTERED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

fn spans() -> &'static SpanTable {
    SPANS.get_or_init(Mutex::default)
}

impl Subscriber for Capture {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        !metadata.is_span() || SPANS_WANTED.load(Ordering::SeqCst)
    }

    fn new_span(&self, attrs: &span::Attributes<'_>) -> span::Id {
        let mut text = Text::default();
        attrs.record(&mut text);
        let id = NEXT_SPAN.fetch_add(1, Ordering::SeqCst);
        SPANS_CREATED.fetch_add(1, Ordering::SeqCst);
        spans()
            .lock()
            .unwrap()
            .insert(id, (text.fields, attrs.is_root()));
        span::Id::from_u64(id)
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}

    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut text = Text::default();
        event.record(&mut text);
        let span = ENTERED.with(|stack| stack.borrow().last().copied());
        EVENTS.lock().unwrap().push(Record {
            level: *event.metadata().level(),
            target: event.metadata().target().to_string(),
            message: text.message,
            fields: text.fields,
            span,
        });
    }

    fn enter(&self, id: &span::Id) {
        ENTERS.fetch_add(1, Ordering::SeqCst);
        ENTERED.with(|stack| stack.borrow_mut().push(id.into_u64()));
    }

    fn exit(&self, _: &span::Id) {
        ENTERED.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

/// Installs the one global subscriber on first use and serializes the tests
/// of this binary, which share it.
fn install() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    static INSTALLED: OnceLock<()> = OnceLock::new();
    let guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    INSTALLED.get_or_init(|| {
        tracing::subscriber::set_global_default(Capture)
            .expect("no other global subscriber in this test binary");
    });
    guard
}

fn events_where(pred: impl Fn(&Record) -> bool) -> Vec<Record> {
    EVENTS
        .lock()
        .unwrap()
        .iter()
        .filter(|r| pred(r))
        .cloned()
        .collect()
}

/// The events whose text contains `text` and whose `field` holds `value`.
fn lines(text: &str, field: &str, value: &str) -> Vec<Record> {
    events_where(|r| {
        r.message.contains(text) && r.fields.get(field).map(String::as_str) == Some(value)
    })
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime")
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {BOUND:?} waiting for: {what}"),
    }
}

async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    within(what, async {
        while !pred() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}

// ---------------------------------------------------------------------------
// Actors that trigger the statements
// ---------------------------------------------------------------------------

/// Parks every turn until the test releases it.
struct Parked {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for Parked {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

/// Ends at its first message.
struct Plain;

impl Actor for Plain {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Panics when dropped once armed.
static ARMED: AtomicBool = AtomicBool::new(false);

struct Loud;

impl Drop for Loud {
    fn drop(&mut self) {
        if ARMED.load(Ordering::SeqCst) {
            panic!("the value panicked when dropped");
        }
    }
}

/// Parks a turn forever, holding a value whose drop panics.
struct Holder {
    _loud: Loud,
    entered: Arc<Notify>,
}

impl Actor for Holder {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// A stream whose poll panics.
struct Boom;

impl tokio_stream::Stream for Boom {
    type Item = u32;

    fn poll_next(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<u32>> {
        panic!("the stream panicked");
    }
}

/// Registers the panicking stream when it starts.
struct Streamer;

enum StreamMsg {
    Event,
}

impl From<StreamEvent<u32>> for StreamMsg {
    fn from(_: StreamEvent<u32>) -> Self {
        StreamMsg::Event
    }
}

impl Actor for Streamer {
    type Message = StreamMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.add_stream(Boom);
        Ok(())
    }

    async fn handle(&mut self, _msg: StreamMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Fails its own stop, so its supervisor's teardown reports its end as
/// unexpected.
struct Leaf;

impl Actor for Leaf {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        Err(ActorError::user("the leaf fails its stop"))
    }
}

/// Starts a [`Leaf`] under itself.
struct Sup;

impl Actor for Sup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Leaf)
            .named(uname("leaf"))
            .restart_type(RestartType::Permanent)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// What a scenario produced: the name its statement is keyed by and the
/// system it ran in.
struct Ran {
    name: String,
    system: String,
}

/// One statement the crate can trigger.
struct Site {
    label: &'static str,
    /// The text of the statement.
    text: &'static str,
    level: Level,
    /// `None` for the default target (the crate's module path).
    target: Option<&'static str>,
    /// The field that holds the actor's name (the id of a named actor).
    key: &'static str,
    /// The fields the statement carries besides its text.
    required: &'static [&'static str],
    run: fn() -> Ran,
}

const ACTOR_FIELDS: &[&str] = &["actor_id", "actor_name", "incarnation", "system"];

fn stopped() -> Ran {
    let rt = runtime();
    let (name, system) = (uname("stopped"), uname("stopped-system"));
    rt.block_on(async {
        let sys = ActorSystem::create_with(system.clone(), SystemConfig::default()).unwrap();
        let actor = Plain
            .spawn()
            .named(name.clone())
            .on_system(&sys)
            .await
            .unwrap();
        actor.stop(StopReason::Graceful).await.unwrap();
        within("the end", actor.wait_stopped()).await;
        until("the end line", || {
            !lines("Actor stopped", "actor_id", &name).is_empty()
        })
        .await;
    });
    Ran { name, system }
}

fn priority_lane_full() -> Ran {
    let rt = runtime();
    let (name, system) = (uname("priority"), uname("priority-system"));
    rt.block_on(async {
        let sys = ActorSystem::create_with(system.clone(), SystemConfig::default()).unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let actor = Parked {
            entered: entered.clone(),
            release: release.clone(),
        }
        .spawn()
        .named(name.clone())
        .on_system(&sys)
        .with_config(ActorConfig::default().with_priority_lane(1))
        .await
        .unwrap();
        actor.notify(()).await.unwrap();
        within("the turn", entered.notified()).await;
        actor.try_notify_priority(()).unwrap();
        let waiting = actor.clone();
        let sender = tokio::spawn(async move {
            let _ = waiting.notify_priority(()).await;
        });
        until("the full-lane line", || {
            !lines("priority lane full", "actor_id", &name).is_empty()
        })
        .await;
        actor.stop(StopReason::Kill).await.unwrap();
        let _ = sender.await;
    });
    Ran { name, system }
}

fn turn_over_threshold() -> Ran {
    let rt = runtime();
    let (name, system) = (uname("sweep"), uname("sweep-system"));
    rt.block_on(async {
        let sys = ActorSystem::create_with(
            system.clone(),
            SystemConfig::default().with_turn_warning_threshold(Duration::from_millis(100)),
        )
        .unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let actor = Parked {
            entered: entered.clone(),
            release: release.clone(),
        }
        .spawn()
        .named(name.clone())
        .on_system(&sys)
        .await
        .unwrap();
        actor.notify(()).await.unwrap();
        within("the turn", entered.notified()).await;
        until("the sweep line", || {
            !lines("actor turn exceeds", "actor_id", &name).is_empty()
        })
        .await;
        actor.stop(StopReason::Kill).await.unwrap();
        within("the end", actor.wait_stopped()).await;
    });
    Ran { name, system }
}

fn stream_panic() -> Ran {
    let rt = runtime();
    let (name, system) = (uname("stream"), uname("stream-system"));
    rt.block_on(async {
        let sys = ActorSystem::create_with(system.clone(), SystemConfig::default()).unwrap();
        let actor = Streamer
            .spawn()
            .named(name.clone())
            .on_system(&sys)
            .await
            .unwrap();
        until("the stream line", || {
            !lines("a timer or stream of the actor panicked", "actor_id", &name).is_empty()
        })
        .await;
        let _ = actor.stop(StopReason::Kill).await;
        within("the end", actor.wait_stopped()).await;
    });
    Ran { name, system }
}

fn drop_panic_at_an_abort() -> Ran {
    let rt = runtime();
    let (name, system) = (uname("abort"), uname("abort-system"));
    rt.block_on(async {
        let sys = ActorSystem::create_with(system.clone(), SystemConfig::default()).unwrap();
        let entered = Arc::new(Notify::new());
        let actor = Holder {
            _loud: Loud,
            entered: entered.clone(),
        }
        .spawn()
        .named(name.clone())
        .on_system(&sys)
        .kill_grace(Duration::from_millis(50))
        .await
        .unwrap();
        actor.notify(()).await.unwrap();
        within("the turn", entered.notified()).await;
        ARMED.store(true, Ordering::SeqCst);
        actor.stop(StopReason::Kill).await.unwrap();
        within("the end", actor.wait_stopped()).await;
    });
    // The task's drops finish when the runtime has dropped it.
    drop(rt);
    ARMED.store(false, Ordering::SeqCst);
    Ran { name, system }
}

fn child_ended_unexpectedly() -> Ran {
    let rt = runtime();
    let (name, system) = (uname("sup"), uname("sup-system"));
    rt.block_on(async {
        let sys = ActorSystem::create_with(system.clone(), SystemConfig::default()).unwrap();
        let sup = Sup
            .spawn()
            .named(name.clone())
            .on_system(&sys)
            .supervisor()
            .await
            .unwrap();
        sup.stop(StopReason::Graceful).await.unwrap();
        within("the end", sup.wait_stopped()).await;
        until("the report", || {
            !lines("ended unexpectedly", "supervisor", &name).is_empty()
        })
        .await;
    });
    Ran { name, system }
}

#[test]
fn every_actor_scoped_statement_carries_the_identity_fields_and_its_level() {
    let _serial = install();
    let table = [
        Site {
            label: "actor stopped",
            text: "Actor stopped",
            level: Level::INFO,
            target: None,
            key: "actor_id",
            required: ACTOR_FIELDS,
            run: stopped,
        },
        Site {
            label: "priority lane full",
            text: "priority lane full",
            level: Level::WARN,
            target: None,
            key: "actor_id",
            required: ACTOR_FIELDS,
            run: priority_lane_full,
        },
        Site {
            label: "turn over the threshold",
            text: "actor turn exceeds",
            level: Level::WARN,
            target: None,
            key: "actor_id",
            required: ACTOR_FIELDS,
            run: turn_over_threshold,
        },
        Site {
            label: "stream panic",
            text: "a timer or stream of the actor panicked",
            level: Level::WARN,
            target: None,
            key: "actor_id",
            required: ACTOR_FIELDS,
            run: stream_panic,
        },
        Site {
            label: "drop panic at an abort",
            text: "a drop at the abort or the unwind",
            level: Level::WARN,
            target: None,
            key: "actor_id",
            // Written inside the actor's own task, where the span carries the
            // identity, from holders that keep nothing but the id.
            required: &["actor_id"],
            run: drop_panic_at_an_abort,
        },
        Site {
            label: "child ended unexpectedly",
            text: "ended unexpectedly",
            level: Level::ERROR,
            target: Some("tokio_actors::supervisor"),
            key: "supervisor",
            required: &["supervisor", "child", "incarnation", "reason"],
            run: child_ended_unexpectedly,
        },
    ];
    for site in table {
        let ran = (site.run)();
        let found = lines(site.text, site.key, &ran.name);
        assert!(!found.is_empty(), "{}: no line", site.label);
        for record in &found {
            assert_eq!(record.level, site.level, "{}: level", site.label);
            if let Some(target) = site.target {
                assert_eq!(record.target, target, "{}: target", site.label);
            }
            for field in site.required {
                assert!(
                    record.fields.contains_key(*field),
                    "{}: no field {field}: {record:?}",
                    site.label
                );
            }
            if !site.required.contains(&"incarnation") && site.key == "actor_id" {
                continue;
            }
            let incarnation = &record.fields["incarnation"];
            assert!(
                incarnation.parse::<u64>().is_ok(),
                "{}: incarnation {incarnation}",
                site.label
            );
            if site.key == "actor_id" {
                assert_eq!(record.fields["actor_name"], ran.name, "{}", site.label);
                assert_eq!(record.fields["system"], ran.system, "{}", site.label);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The span
// ---------------------------------------------------------------------------

#[test]
fn the_span_exists_only_for_an_interested_subscriber_and_is_entered_on_every_poll() {
    let _serial = install();

    // No interest in spans: nothing is created at the spawn, the events
    // still go out, and none is inside a span.
    SPANS_WANTED.store(false, Ordering::SeqCst);
    let created = SPANS_CREATED.load(Ordering::SeqCst);
    let enters = ENTERS.load(Ordering::SeqCst);
    let off = stopped();
    assert_eq!(
        SPANS_CREATED.load(Ordering::SeqCst),
        created,
        "no span was created"
    );
    assert_eq!(ENTERS.load(Ordering::SeqCst), enters, "nothing was entered");
    let line = lines("Actor stopped", "actor_id", &off.name);
    assert_eq!(line.len(), 1, "{line:?}");
    assert_eq!(line[0].span, None, "the event is outside any span");

    // Interest in spans: one span per incarnation, with no parent, and the
    // end line, written after the actor's awaits, is inside it.
    SPANS_WANTED.store(true, Ordering::SeqCst);
    let on = stopped();
    SPANS_WANTED.store(false, Ordering::SeqCst);
    let line = lines("Actor stopped", "actor_id", &on.name);
    assert_eq!(line.len(), 1, "{line:?}");
    let id = line[0].span.expect("the end line is inside the span");
    let table = spans().lock().unwrap();
    let (fields, root) = table.get(&id).expect("the span was recorded");
    assert!(*root, "the span has no parent");
    assert_eq!(fields["actor.id"], on.name);
    assert_eq!(fields["actor.name"], on.name);
    assert_eq!(fields["system"], on.system);
    assert!(fields["actor.incarnation"].parse::<u64>().is_ok());
    assert!(
        ENTERS.load(Ordering::SeqCst) > enters,
        "the span was entered"
    );
}

/// A send that waits on a full priority lane warns once for the spell the lane
/// stays full; the warning is armed again when a later priority send finds
/// room, so a second spell warns once more. On one thread a second waiter
/// joins the first spell (it is polled before the test goes on), and it adds
/// no second line.
#[test]
fn a_full_priority_lane_warns_once_per_full_spell() {
    let _serial = install();
    SPANS_WANTED.store(false, Ordering::SeqCst);
    for single_thread in [true, false] {
        let rt = if single_thread {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime")
        } else {
            runtime()
        };
        let name = uname(if single_thread {
            "spells-one"
        } else {
            "spells-many"
        });
        let row = if single_thread {
            "one thread"
        } else {
            "two threads"
        };
        let waiters = if single_thread { 2 } else { 1 };
        rt.block_on(async {
            let sys =
                ActorSystem::create_with(uname("spells-system"), SystemConfig::default()).unwrap();
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let actor = Parked {
                entered: entered.clone(),
                release: release.clone(),
            }
            .spawn()
            .named(name.clone())
            .on_system(&sys)
            .with_config(ActorConfig::default().with_priority_lane(1))
            .await
            .unwrap();
            let full_lines = || lines("priority lane full", "actor_id", &name).len();

            // First spell: the turn holds, the lane is full and the senders wait.
            actor.notify(()).await.unwrap();
            within("the first turn", entered.notified()).await;
            actor.try_notify_priority(()).unwrap();
            let mut senders = Vec::new();
            for _ in 0..waiters {
                let waiting = actor.clone();
                senders.push(tokio::spawn(async move {
                    let _ = waiting.notify_priority(()).await;
                }));
                for _ in 0..3 {
                    tokio::task::yield_now().await;
                }
            }
            until("the first spell's line", || full_lines() >= 1).await;
            assert_eq!(full_lines(), 1, "{row}: one line for the first spell");

            // Each turn frees the lane's slot for the next waiter; the turn
            // after the last waiter's message begins with the lane empty.
            for _ in 0..=waiters {
                release.notify_one();
                within("the next turn", entered.notified()).await;
            }

            // Second spell: a send finds room, the lane fills again, a sender waits.
            actor.notify_priority(()).await.unwrap();
            let waiting = actor.clone();
            senders.push(tokio::spawn(async move {
                let _ = waiting.notify_priority(()).await;
            }));
            until("the second spell's line", || full_lines() >= 2).await;
            assert_eq!(full_lines(), 2, "{row}: one more line for the second spell");

            actor.stop(StopReason::Kill).await.unwrap();
            for sender in senders {
                let _ = sender.await;
            }
        });
    }
}
