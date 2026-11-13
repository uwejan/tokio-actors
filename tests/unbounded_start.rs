#![allow(dead_code)]
//! `ChildSpawnBuilder::unbounded_start` writes the default start choice out: a
//! restart's initialization waits without a bound. It and `start_timeout`
//! share one setting, so the last call of the two wins, and the builder's
//! `Debug` output names the choice.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{sleep, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, ActorStatus, ActorSystem, ChildEvent, RestartType, Shutdown,
    SpawnError, StopReason, SupervisionConfig,
};

mod common;

/// The longest a test may take in real time before it reads as hung.
const REAL_DEADLINE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
enum Choice {
    /// `start_timeout(d)` and then `unbounded_start()`.
    BoundThenUnbounded,
    /// `unbounded_start()` and then `start_timeout(d)`.
    UnboundedThenBound,
    /// Neither call: the default start choice.
    Default,
}

#[derive(Default)]
struct Probe {
    built: AtomicUsize,
    began: AtomicUsize,
    dropped: AtomicUsize,
    /// Releases the restarts' initialization: each restart waits for one
    /// permit and counts itself in `initialized` when it has it. Nothing
    /// takes the permit unless a test gives it.
    release: Notify,
    initialized: AtomicUsize,
}

struct Hang {
    nth: usize,
    probe: Arc<Probe>,
}

#[derive(Clone)]
enum Cmd {
    Crash,
}

impl Actor for Hang {
    type Message = Cmd;
    type Response = ();

    async fn handle(&mut self, _msg: Cmd, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("crash on command");
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.probe.began.fetch_add(1, Ordering::SeqCst);
        if self.nth > 1 {
            // Every restart's initialization lasts until a test releases it,
            // which most tests never do.
            self.probe.release.notified().await;
            self.probe.initialized.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

impl Drop for Hang {
    fn drop(&mut self) {
        self.probe.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

struct Sup {
    name: String,
    choice: Choice,
    bound: Duration,
    shutdown: Shutdown,
    probe: Arc<Probe>,
    events: Arc<Mutex<Vec<ChildEvent>>>,
    debug_lines: Arc<Mutex<Vec<String>>>,
}

impl Actor for Sup {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let probe = self.probe.clone();
        let factory = move || Hang {
            nth: probe.built.fetch_add(1, Ordering::SeqCst) + 1,
            probe: probe.clone(),
        };
        let mut lines = vec![];
        lines.push(format!("{:?}", ctx.spawn_child(factory.clone())));
        lines.push(format!(
            "{:?}",
            ctx.spawn_child(factory.clone()).start_timeout(self.bound)
        ));
        lines.push(format!(
            "{:?}",
            ctx.spawn_child(factory.clone()).unbounded_start()
        ));
        lines.push(format!(
            "{:?}",
            ctx.spawn_child(factory.clone())
                .start_timeout(self.bound)
                .unbounded_start()
        ));
        lines.push(format!(
            "{:?}",
            ctx.spawn_child(factory.clone())
                .unbounded_start()
                .start_timeout(self.bound)
        ));
        lines.push(format!(
            "{:?}",
            ctx.spawn_child(factory.clone())
                .start_timeout(Duration::MAX)
        ));
        *self.debug_lines.lock().unwrap() = lines;

        let builder = ctx
            .spawn_child(factory)
            .named(self.name.clone())
            .restart_type(RestartType::Permanent)
            .shutdown(self.shutdown);
        let builder = match self.choice {
            Choice::Default => builder,
            Choice::BoundThenUnbounded => builder.start_timeout(self.bound).unbounded_start(),
            Choice::UnboundedThenBound => builder.unbounded_start().start_timeout(self.bound),
        };
        builder.await?;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

struct Rig {
    sup: tokio_actors::ActorHandle<Sup>,
    sys: Arc<ActorSystem>,
    name: String,
    probe: Arc<Probe>,
    events: Arc<Mutex<Vec<ChildEvent>>>,
    debug_lines: Arc<Mutex<Vec<String>>>,
}

async fn rig(tag: &str, choice: Choice, bound: Duration) -> Rig {
    rig_with_shutdown(tag, choice, bound, Shutdown::Kill).await
}

async fn rig_with_shutdown(tag: &str, choice: Choice, bound: Duration, shutdown: Shutdown) -> Rig {
    let sys = ActorSystem::create(format!("unbounded-start-{tag}")).unwrap();
    let name = format!("unbounded-start-{tag}-child");
    let probe = Arc::new(Probe::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let debug_lines = Arc::new(Mutex::new(Vec::new()));
    let sup = Sup {
        name: name.clone(),
        choice,
        bound,
        shutdown,
        probe: probe.clone(),
        events: events.clone(),
        debug_lines: debug_lines.clone(),
    }
    .spawn()
    .named(format!("unbounded-start-{tag}-sup"))
    .on_system(&sys)
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(100, Duration::from_secs(3600)))
    .await
    .unwrap();
    Rig {
        sup,
        sys,
        name,
        probe,
        events,
        debug_lines,
    }
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3600);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(1)).await;
    }
}

async fn crash_the_child(rig: &Rig) {
    let child = loop {
        if let Some(h) = rig.sys.get::<Hang>(&rig.name) {
            break h;
        }
        sleep(Duration::from_millis(1)).await;
    };
    child.notify(Cmd::Crash).await.unwrap();
}

fn is_start_timeout(event: &ChildEvent) -> bool {
    matches!(
        &event.reason,
        StopReason::Failure(ActorError::Spawn(SpawnError::StartTimeout))
    )
}

/// Runs `scenario` on a paused clock in a thread of its own and fails when it
/// has not ended within a real-time deadline, so a hang of the test itself is
/// a failure and not a stuck run.
fn run_paused_with_a_real_deadline<F>(scenario: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            common::run_paused(scenario);
        }));
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(REAL_DEADLINE) {
        Ok(Ok(())) => {}
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("the scenario did not end within {REAL_DEADLINE:?} of real time"),
    }
}

/// A restart's initialization that lasts far longer than any bound the crate
/// could default to still completes, and the child then runs: the wait has no
/// bound whatever the child's `Shutdown`, because only `start_timeout` sets one.
async fn a_slow_restart_initialization_completes_under(tag: &str, shutdown: Shutdown) {
    let rig = rig_with_shutdown(tag, Choice::Default, Duration::from_millis(200), shutdown).await;
    wait_for("the first incarnation", || {
        rig.probe.began.load(Ordering::SeqCst) == 1
    })
    .await;
    crash_the_child(&rig).await;
    wait_for("the restart to begin its initialization", || {
        rig.probe.began.load(Ordering::SeqCst) == 2
    })
    .await;

    // A day of virtual time passes while the initialization waits: the
    // paused clock advances through it.
    sleep(Duration::from_secs(24 * 3600)).await;
    assert_eq!(
        rig.probe.initialized.load(Ordering::SeqCst),
        0,
        "the initialization is still waiting"
    );
    assert_eq!(
        rig.probe.dropped.load(Ordering::SeqCst),
        1,
        "only the crashed first incarnation ended; the waiting restart was not cut"
    );

    rig.probe.release.notify_one();
    wait_for("the restart's initialization to complete", || {
        rig.probe.initialized.load(Ordering::SeqCst) == 1
    })
    .await;
    let child = loop {
        if let Some(h) = rig.sys.get::<Hang>(&rig.name) {
            break h;
        }
        sleep(Duration::from_millis(1)).await;
    };
    assert_eq!(
        child.get_status().await.unwrap().status,
        ActorStatus::Running,
        "the restarted child runs once its initialization completes"
    );
    assert_eq!(
        rig.probe.began.load(Ordering::SeqCst),
        2,
        "one restart began, and no other"
    );
    assert_eq!(
        rig.probe.dropped.load(Ordering::SeqCst),
        1,
        "the restarted incarnation was never cut"
    );
    let events = rig.events.lock().unwrap().clone();
    assert!(
        !events.iter().any(is_start_timeout),
        "no restart was cut by a start timeout: {events:?}"
    );

    let _ = rig.sup.stop(StopReason::Graceful).await;
}

#[test]
fn a_slow_restart_initialization_completes_under_a_timeout_shutdown() {
    run_paused_with_a_real_deadline(a_slow_restart_initialization_completes_under(
        "slow-timeout",
        Shutdown::Timeout(Duration::from_millis(300)),
    ));
}
