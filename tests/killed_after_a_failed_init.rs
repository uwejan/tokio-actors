//! A `Kill` that reaches an actor after its initialization failed and
//! before its reason is final makes the reason `Kill`. The awaited spawn
//! then reads `SpawnError::Killed`, the one answer the status plane gives;
//! with the `tracing` feature the initialization's error is logged once,
//! naming the actor.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, ActorSystem, Shutdown, SpawnError, StopReason,
};

const BOUND: Duration = Duration::from_secs(10);

/// The error the supervisor's initialization fails with.
const INIT_ERROR: &str = "the supervisor refuses to start";

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "killed-after-init-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {BOUND:?} waiting for: {what}"),
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

/// The gates a [`Child`]'s `pre_stop` signals and waits on.
#[derive(Default)]
struct Gates {
    entered: Notify,
    release: Notify,
}

/// Signals that its `pre_stop` began, then waits for its release.
struct Child {
    gates: Arc<Gates>,
}

impl Actor for Child {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.gates.entered.notify_one();
        self.gates.release.notified().await;
        true
    }
}

/// Starts a [`Child`] under `Shutdown::Infinity` in `on_started`, then fails
/// its initialization: the teardown of its failed initialization waits in
/// the child's `pre_stop`.
struct Parent {
    gates: Arc<Gates>,
}

impl Actor for Parent {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let gates = self.gates.clone();
        ctx.spawn_child(move || Child {
            gates: gates.clone(),
        })
        .named(uname("child"))
        .shutdown(Shutdown::Infinity)
        .await?;
        Err(ActorError::user(INIT_ERROR))
    }
}

fn a_kill_after_a_failed_init_reads_killed(flavor: &str) {
    on(flavor, async {
        let system = ActorSystem::create(uname("system")).unwrap();
        let name = uname("parent");
        let gates = Arc::new(Gates::default());
        let spawn = tokio::spawn({
            let system = system.clone();
            let name = name.clone();
            let gates = gates.clone();
            async move {
                Parent { gates }
                    .spawn()
                    .named(name)
                    .on_system(&system)
                    // The parent's own end, not the abort that backs the
                    // Kill, ends it: the backstop waits longer than the test.
                    .kill_grace(Duration::from_secs(10))
                    .supervisor()
                    .await
                    .map(|_| ())
            }
        });
        // The failed initialization's teardown is in the child's pre_stop:
        // the parent's lane is narrowed to `Kill` and its reason not final.
        within("the child's pre_stop", gates.entered.notified()).await;
        let parent = system
            .get::<Parent>(&name)
            .expect("the parent is registered");
        parent
            .stop(StopReason::Kill)
            .await
            .expect("a Kill is accepted");
        gates.release.notify_one();
        let spawned = within("the spawn's answer", spawn)
            .await
            .expect("the spawn's task joins");
        assert!(
            matches!(spawned, Err(SpawnError::Killed)),
            "the spawn reads Killed: {spawned:?}"
        );
        assert_eq!(
            within("the parent's end", parent.wait_stopped()).await,
            StopReason::Kill
        );
        #[cfg(feature = "tracing")]
        {
            let token = format!("actor_id={name} ");
            let lines: Vec<String> = warnings::install()
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains(&token) && line.contains(INIT_ERROR))
                .cloned()
                .collect();
            assert_eq!(lines.len(), 1, "the init's error is logged once: {lines:?}");
        }
    });
}

#[test]
fn a_kill_after_a_failed_init_reads_killed_current_thread() {
    #[cfg(feature = "tracing")]
    let _ = warnings::install();
    a_kill_after_a_failed_init_reads_killed("current_thread");
}

#[test]
fn a_kill_after_a_failed_init_reads_killed_multi_thread() {
    #[cfg(feature = "tracing")]
    let _ = warnings::install();
    a_kill_after_a_failed_init_reads_killed("multi_thread");
}

#[cfg(feature = "tracing")]
mod warnings {
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex, OnceLock};

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

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
