#![cfg(feature = "tracing")]
//! The turn-watchdog sweep logs one warning per callback that runs over the
//! system's threshold: a callback that stays stuck across several checks is
//! warned once, the next stuck callback of the same actor is warned again, a
//! callback that stays under the threshold is never warned, and a system on a
//! runtime without the Tokio time driver logs nothing.

mod common;

use std::future::IntoFuture;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, SystemConfig,
};

const BOUND: Duration = Duration::from_secs(60);
const THRESHOLD: Duration = Duration::from_millis(200);
const WARNING: &str = "actor turn exceeds the turn warning threshold";

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "turn-watchdog-log-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

async fn within<F: IntoFuture>(what: &str, fut: F) -> F::Output {
    match timeout(BOUND, fut.into_future()).await {
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

/// The warnings whose text names the actor `name` and contains `text`.
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

/// Every warning that names a turn-watchdog sweep.
fn sweep_lines() -> Vec<String> {
    warnings::install()
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("turn-watchdog sweep"))
        .cloned()
        .collect()
}

/// An actor whose handler parks at an await until the test releases it, and
/// says when each callback began.
struct Stuck {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for Stuck {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

/// An actor whose callbacks are always short but are in flight for several
/// scheduler turns each.
struct Brief;

impl Actor for Brief {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        Ok(())
    }
}

struct Gate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Gate {
    fn new() -> Self {
        Self {
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        }
    }

    fn actor(&self) -> Stuck {
        Stuck {
            entered: self.entered.clone(),
            release: self.release.clone(),
        }
    }
}

fn config() -> SystemConfig {
    SystemConfig::default().with_turn_warning_threshold(THRESHOLD)
}

/// Keeps the short-callback actor busy until dropped.
struct Driver {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Driver {
    fn start(handle: tokio_actors::ActorHandle<Brief>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn({
            let stop = stop.clone();
            async move {
                while !stop.load(Ordering::Relaxed) {
                    if handle.notify(()).await.is_err() {
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            }
        });
        Self { stop, task }
    }

    async fn finish(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = within("the driver to end", self.task).await;
    }
}

/// A system on a runtime without the time driver, with an actor inside a
/// callback that never ends. Its runtime is returned so it outlives the test.
fn driverless_with_a_stuck_actor(name: &str) -> tokio::runtime::Runtime {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime without the time driver");
    let sys = ActorSystem::create_with(name.to_string(), config()).unwrap();
    let gate = Gate::new();
    rt.block_on(async {
        let handle = gate
            .actor()
            .spawn()
            .named(format!("{name}-actor"))
            .on_system(&sys)
            .await
            .unwrap();
        handle.notify(()).await.unwrap();
        gate.entered.notified().await;
        // Let every task the registration may have spawned run.
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
    });
    rt
}

#[test]
fn a_stuck_callback_is_warned_once_and_a_short_one_never() {
    let _ = warnings::install();
    let driverless_name = uname("driverless");
    let rt_without_driver = driverless_with_a_stuck_actor(&driverless_name);

    common::run(2, async {
        let sys = ActorSystem::create_with(uname("sweep"), config()).unwrap();
        let brief_name = uname("brief");
        let brief = within(
            "the short actor to start",
            Brief.spawn().named(brief_name.clone()).on_system(&sys),
        )
        .await
        .unwrap();
        let driver = Driver::start(brief);

        // The first stuck callback is warned.
        let a_name = uname("stuck-a");
        let a_gate = Gate::new();
        let a = within(
            "actor a to start",
            a_gate.actor().spawn().named(a_name.clone()).on_system(&sys),
        )
        .await
        .unwrap();
        a.notify(()).await.unwrap();
        within("a's callback to begin", a_gate.entered.notified()).await;
        until("the warning for a", || {
            !lines_naming(&a_name, WARNING).is_empty()
        })
        .await;

        // A second actor is warned later, by a later check; that check saw
        // the first callback still stuck and did not warn again.
        let c_name = uname("stuck-c");
        let c_gate = Gate::new();
        let c = within(
            "actor c to start",
            c_gate.actor().spawn().named(c_name.clone()).on_system(&sys),
        )
        .await
        .unwrap();
        c.notify(()).await.unwrap();
        within("c's callback to begin", c_gate.entered.notified()).await;
        until("the warning for c", || {
            !lines_naming(&c_name, WARNING).is_empty()
        })
        .await;
        assert_eq!(
            lines_naming(&a_name, WARNING).len(),
            1,
            "one warning for one stuck callback: {:?}",
            lines_naming(&a_name, WARNING)
        );
        assert_eq!(lines_naming(&c_name, WARNING).len(), 1);

        // The same actor's next stuck callback is a new warning.
        a_gate.release.notify_one();
        a.notify(()).await.unwrap();
        within("a's second callback to begin", a_gate.entered.notified()).await;
        until("the second warning for a", || {
            lines_naming(&a_name, WARNING).len() == 2
        })
        .await;
        // Let c's later checks pass over a's two callbacks and c's own.
        c_gate.release.notify_one();
        a_gate.release.notify_one();

        driver.finish().await;
        assert!(
            lines_naming(&brief_name, "").is_empty(),
            "a callback under the threshold was warned: {:?}",
            lines_naming(&brief_name, "")
        );
    });

    let driverless_actor = format!("{driverless_name}-actor");
    assert!(
        lines_naming(&driverless_actor, "").is_empty(),
        "a system on a runtime without the time driver logged: {:?}",
        lines_naming(&driverless_actor, "")
    );
    assert!(
        sweep_lines().is_empty(),
        "a sweep ended on a runtime without the time driver: {:?}",
        sweep_lines()
    );
    drop(rt_without_driver);
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
