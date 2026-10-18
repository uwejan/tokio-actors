#![cfg(feature = "tracing")]
//! A tracing subscriber that panics on the warning a supervisor's teardown
//! logs for a child that ended unexpectedly, inside that supervisor's poll.
//! The log line is written inside a catch, so the panic is contained there:
//! the supervisor's own poll does not unwind, and its status plane, its
//! supervisor's report of its end, a restart attempt that joins it and an
//! awaited spawn read the reason they read with a quiet subscriber.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::field::{Field, Visit};
use tracing::span;
use tracing::{Event, Metadata, Subscriber};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorResult, ActorSystem, ChildEvent, RestartType, SpawnError,
    StopReason,
};

const BOUND: Duration = Duration::from_secs(10);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("own-unwind-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

/// The process-wide subscriber: it records every event, and panics, once
/// per armed supervisor, on the warning that supervisor's teardown logs for
/// a child that ended unexpectedly.
#[derive(Default)]
struct Trap {
    armed: Mutex<HashSet<String>>,
    log: Mutex<Vec<String>>,
}

/// An event's fields, as `name=value` pairs.
struct Fields {
    line: String,
    supervisor: Option<String>,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "supervisor" {
            self.supervisor = Some(text.clone());
        }
        self.line.push_str(&format!("{}={text} ", field.name()));
    }
}

impl Subscriber for &'static Trap {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields {
            line: String::new(),
            supervisor: None,
        };
        event.record(&mut fields);
        self.log.lock().unwrap().push(fields.line.clone());
        if !fields.line.contains("ended unexpectedly") {
            return;
        }
        let Some(supervisor) = fields.supervisor else {
            return;
        };
        if self.armed.lock().unwrap().remove(&supervisor) {
            panic!("trap in the poll of {supervisor}");
        }
    }
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}

fn trap() -> &'static Trap {
    static TRAP: OnceLock<&'static Trap> = OnceLock::new();
    TRAP.get_or_init(|| {
        let trap: &'static Trap = Box::leak(Box::default());
        tracing::subscriber::set_global_default(trap).expect("the only subscriber");
        trap
    })
}

/// Arms the trap for the supervisor named `name`.
fn arm(name: &str) {
    trap().armed.lock().unwrap().insert(name.to_string());
}

/// The teardown warnings that name the supervisor `name`.
fn warning_lines(name: &str) -> usize {
    let token = format!("supervisor={name} ");
    trap()
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("ended unexpectedly") && line.contains(&token))
        .count()
}

/// No log line records an uncaught panic of the supervisor `name`'s task.
fn no_uncaught_panic(name: &str) -> bool {
    let token = format!("trap in the poll of {name}");
    !trap()
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("panicked outside every catch") && line.contains(&token))
}

fn on(flavor: &str, body: impl Future<Output = ()>) {
    let runtime = match flavor {
        "current_thread" => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
        _ => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build(),
    }
    .expect("a runtime");
    runtime.block_on(body);
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {BOUND:?} waiting for: {what}"),
    }
}

/// Fails its own stop, so its supervisor's teardown logs its end as
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

/// What a [`Mid`] is asked.
enum MidMsg {
    /// Answer at once: the instance has finished its initialization.
    Ping,
    /// Start a leaf.
    Leaf,
    /// Fail the turn.
    Fail,
}

/// Takes one from `left` when it is above zero, and says whether it did.
fn take_one(left: &AtomicUsize) -> bool {
    let mut seen = left.load(Ordering::SeqCst);
    while seen > 0 {
        match left.compare_exchange_weak(seen, seen - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => seen = now,
        }
    }
    false
}

/// A supervisor whose teardown stops a [`Leaf`]. While `fail_init` counts
/// down, an instance starts a leaf in `on_started` and then fails its
/// initialization, so the teardown runs before the instance answers.
struct Mid {
    fail_init: Arc<AtomicUsize>,
}

impl Actor for Mid {
    type Message = MidMsg;
    type Response = ();

    async fn handle(&mut self, msg: MidMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            MidMsg::Ping => Ok(()),
            MidMsg::Leaf => {
                ctx.spawn_child(|| Leaf)
                    .named(uname("leaf"))
                    .restart_type(RestartType::Permanent)
                    .await?;
                Ok(())
            }
            MidMsg::Fail => Err(ActorError::user("mid fails its turn")),
        }
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let fail = take_one(&self.fail_init);
        if fail {
            ctx.spawn_child(|| Leaf)
                .named(uname("leaf"))
                .restart_type(RestartType::Permanent)
                .await?;
            return Err(ActorError::user("mid fails its initialization"));
        }
        Ok(())
    }
}

/// Supervises one [`Mid`] and reports every end of it.
struct Top {
    mid: String,
    restart: RestartType,
    fail_init: Arc<AtomicUsize>,
    events: mpsc::UnboundedSender<ChildEvent>,
}

impl Actor for Top {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let fail_init = self.fail_init.clone();
        ctx.spawn_child(move || Mid {
            fail_init: fail_init.clone(),
        })
        .named(self.mid.clone())
        .restart_type(self.restart)
        .with_config(ActorConfig::default().supervisor())
        .await?;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.events.send(event.clone());
        Ok(())
    }
}

/// A supervisor and its [`Mid`] child.
struct Rig {
    mid: String,
    fail_init: Arc<AtomicUsize>,
    events: mpsc::UnboundedReceiver<ChildEvent>,
}

async fn rig(restart: RestartType) -> Rig {
    let mid = uname("mid");
    let fail_init = Arc::new(AtomicUsize::new(0));
    let (tx, events) = mpsc::unbounded_channel();
    let top = Top {
        mid: mid.clone(),
        restart,
        fail_init: fail_init.clone(),
        events: tx,
    }
    .spawn()
    .named(uname("top"))
    .supervisor()
    .await
    .expect("top starts");
    top.send(()).await.expect("mid starts");
    Rig {
        mid,
        fail_init,
        events,
    }
}

/// A supervisor's teardown, reached from its stop, logs its leaf's failed
/// stop to the panicking subscriber: the warning is written, the panic is
/// contained, and the supervisor's plane and its supervisor's report of its
/// end read `Graceful`.
fn a_panicking_subscriber_in_a_teardown_leaves_the_supervisor_s_reason(flavor: &str) {
    on(flavor, async {
        let mut rig = rig(RestartType::Temporary).await;
        let mid = ActorSystem::default()
            .get::<Mid>(&rig.mid)
            .expect("mid is registered");
        mid.send(MidMsg::Leaf).await.expect("the leaf starts");
        arm(&rig.mid);
        mid.stop(StopReason::Graceful).await.expect("accepted");
        let plane = within("mid's end", mid.wait_stopped()).await;
        let event = within("top's report of mid", rig.events.recv())
            .await
            .expect("a report");
        assert_eq!(plane, StopReason::Graceful, "the plane reads the stop");
        assert_eq!(event.reason, plane, "the report reads the plane's reason");
        assert_eq!(warning_lines(&rig.mid), 1, "the warning is written once");
        assert!(
            no_uncaught_panic(&rig.mid),
            "the supervisor's poll did not unwind"
        );
    });
}

#[test]
fn a_panicking_subscriber_in_a_teardown_leaves_the_supervisor_s_reason_current_thread() {
    a_panicking_subscriber_in_a_teardown_leaves_the_supervisor_s_reason("current_thread");
}

#[test]
fn a_panicking_subscriber_in_a_teardown_leaves_the_supervisor_s_reason_multi_thread() {
    a_panicking_subscriber_in_a_teardown_leaves_the_supervisor_s_reason("multi_thread");
}

/// A supervisor whose initialization fails stops the leaf it started, and
/// its teardown logs the leaf's failed stop to the panicking subscriber
/// before it answers its awaited spawn: the panic is contained, and the
/// spawn reads the initialization's error.
fn a_panicking_subscriber_in_a_failed_init_s_teardown_leaves_the_init_error(flavor: &str) {
    on(flavor, async {
        let name = uname("mid");
        arm(&name);
        let spawned = Mid {
            fail_init: Arc::new(AtomicUsize::new(1)),
        }
        .spawn()
        .named(name.clone())
        .supervisor()
        .await;
        match spawned {
            Err(SpawnError::Init(err)) => {
                assert_eq!(*err, ActorError::user("mid fails its initialization"));
            }
            Err(other) => panic!("expected Init with the init error, got {other:?}"),
            Ok(_) => panic!("expected the spawn to fail"),
        }
        assert_eq!(warning_lines(&name), 1, "the warning is written once");
        assert!(
            no_uncaught_panic(&name),
            "the supervisor's poll did not unwind"
        );
    });
}

#[test]
fn a_panicking_subscriber_in_a_failed_init_s_teardown_leaves_the_init_error_current_thread() {
    a_panicking_subscriber_in_a_failed_init_s_teardown_leaves_the_init_error("current_thread");
}

#[test]
fn a_panicking_subscriber_in_a_failed_init_s_teardown_leaves_the_init_error_multi_thread() {
    a_panicking_subscriber_in_a_failed_init_s_teardown_leaves_the_init_error("multi_thread");
}

/// A restarted incarnation fails its initialization, and its teardown logs
/// to the panicking subscriber before it answers its restart attempt: the
/// panic is contained, and the supervisor's report of the failed restart
/// carries the initialization's error.
fn a_panicking_subscriber_in_a_restarted_teardown_leaves_the_init_error(flavor: &str) {
    on(flavor, async {
        let mut rig = rig(RestartType::Permanent).await;
        let mid = ActorSystem::default()
            .get::<Mid>(&rig.mid)
            .expect("mid is registered");
        // The first instance has finished its initialization before the
        // next one is set to fail its own.
        mid.send(MidMsg::Ping).await.expect("mid answers");
        rig.fail_init.store(1, Ordering::SeqCst);
        arm(&rig.mid);
        let _ = mid.send(MidMsg::Fail).await;
        let crash = within("top's report of mid's crash", rig.events.recv())
            .await
            .expect("a report");
        assert_eq!(
            crash.reason,
            StopReason::Failure(ActorError::user("mid fails its turn"))
        );
        let restart = within("top's report of the failed restart", rig.events.recv())
            .await
            .expect("a report");
        assert_eq!(
            restart.reason,
            StopReason::Failure(ActorError::user("mid fails its initialization")),
            "the attempt reads the incarnation's plane"
        );
        assert_eq!(warning_lines(&rig.mid), 1, "the warning is written once");
        assert!(
            no_uncaught_panic(&rig.mid),
            "the incarnation's poll did not unwind"
        );
    });
}

#[test]
fn a_panicking_subscriber_in_a_restarted_teardown_leaves_the_init_error_current_thread() {
    a_panicking_subscriber_in_a_restarted_teardown_leaves_the_init_error("current_thread");
}

#[test]
fn a_panicking_subscriber_in_a_restarted_teardown_leaves_the_init_error_multi_thread() {
    a_panicking_subscriber_in_a_restarted_teardown_leaves_the_init_error("multi_thread");
}
