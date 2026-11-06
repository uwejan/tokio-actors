//! After a shutdown's walk is cut short (the runtime it ran on ended), the
//! system keeps refusing every supervised restart, and a refused restart is
//! charged to the restart budget like any failed restart: a supervisor whose
//! child dies then exhausts a budget whose window holds its restarts, while a
//! zero window with a `max_restarts` above zero keeps retrying until a
//! shutdown reaches the supervisor. A `max_restarts` of zero exhausts the
//! budget at once whatever the window.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorStatus, ActorSystem, ChildEvent, ShutdownPolicy,
    StopReason, SupervisionAction, SupervisionConfig,
};

/// The longest any wait may take. It only limits how long a real hang takes
/// to report; a slow host must not read as one.
const BOUND: Duration = Duration::from_secs(60);

fn unique(what: &str) -> String {
    format!("{what}-{}", uuid::Uuid::new_v4())
}

fn with_timers(workers: usize) -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
}

/// Ends a runtime and returns once every task on it is dropped.
fn end_runtime(rt: Runtime) {
    std::thread::spawn(move || drop(rt))
        .join()
        .expect("the runtime ended cleanly");
}

/// Crashes on its first message.
struct Crasher;

impl Actor for Crasher {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("crashed on command"))
    }
}

/// What a supervisor saw: how many child events it received, and whether one
/// of them was the budget running out.
struct Log {
    events: AtomicUsize,
    exhausted: AtomicBool,
    tick: watch::Sender<usize>,
}

impl Log {
    fn new() -> Arc<Log> {
        Arc::new(Log {
            events: AtomicUsize::new(0),
            exhausted: AtomicBool::new(false),
            tick: watch::channel(0).0,
        })
    }

    fn record(&self, action: SupervisionAction) {
        if matches!(action, SupervisionAction::BudgetExhausted) {
            self.exhausted.store(true, Ordering::SeqCst);
        }
        let seen = self.events.fetch_add(1, Ordering::SeqCst) + 1;
        self.tick.send_replace(seen);
    }

    async fn wait_exhausted(&self) {
        self.wait(usize::MAX).await;
    }

    /// Waits until the log holds `count` events or a `BudgetExhausted`.
    async fn wait(&self, count: usize) {
        let mut tick = self.tick.subscribe();
        timeout(
            BOUND,
            tick.wait_for(|_| {
                self.events.load(Ordering::SeqCst) >= count || self.exhausted.load(Ordering::SeqCst)
            }),
        )
        .await
        .expect("the supervisor's events arrived within the bound")
        .expect("the log is open");
    }
}

/// A supervisor that spawns its one `Permanent` child on its first message
/// and logs the action of every child event it receives.
struct Supervisor {
    child: String,
    log: Arc<Log>,
}

impl Actor for Supervisor {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Crasher)
            .named(self.child.clone())
            .await?;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.log.record(event.action);
        Ok(())
    }
}

/// Vetoes a stop request while `veto` is set and reports each request.
struct Veto {
    veto: Arc<AtomicBool>,
    asked: mpsc::UnboundedSender<()>,
}

impl Actor for Veto {
    type Message = ();
    type Response = ();

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        let _ = self.asked.send(());
        !self.veto.load(Ordering::SeqCst)
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

struct Scene {
    sys: Arc<ActorSystem>,
    /// Hosts every actor; the walk's runtime has ended.
    b: Runtime,
    veto: Arc<AtomicBool>,
    supervisor: ActorHandle<Supervisor>,
    log: Arc<Log>,
    child: String,
}

/// A system with a supervisor (its child running), a root that vetoes its
/// stop, and a shutdown call whose walk ran on a runtime that has ended after
/// asking that root. The system is left refusing registrations and restarts.
fn cut_short_scene(what: &str, config: SupervisionConfig) -> Scene {
    let sys = ActorSystem::create(unique(what)).unwrap();
    let b = with_timers(2);
    let log = Log::new();
    let child = unique("crasher");
    let supervisor = b.block_on(async {
        let supervisor = timeout(
            BOUND,
            Supervisor {
                child: child.clone(),
                log: log.clone(),
            }
            .spawn()
            .named(unique("supervisor"))
            .on_system(&sys)
            .with_supervision(config),
        )
        .await
        .expect("the spawn finished within the bound")
        .unwrap();
        timeout(BOUND, supervisor.send(()))
            .await
            .expect("the supervisor answered within the bound")
            .unwrap();
        supervisor
    });

    // Registered last, so the walk reaches it first and waits on its veto.
    let veto = Arc::new(AtomicBool::new(true));
    let (asked_tx, mut asked_rx) = mpsc::unbounded_channel();
    b.block_on(async {
        timeout(
            BOUND,
            Veto {
                veto: veto.clone(),
                asked: asked_tx,
            }
            .spawn()
            .named(unique("veto"))
            .on_system(&sys),
        )
        .await
        .expect("the spawn finished within the bound")
        .unwrap();
    });

    let a = with_timers(1);
    let _call = a.spawn({
        let sys = sys.clone();
        async move {
            sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(600),
                per_actor_timeout: Duration::from_secs(600),
            })
            .await
        }
    });
    b.block_on(async {
        timeout(BOUND, asked_rx.recv())
            .await
            .expect("the walk asked the root within the bound")
            .expect("the root reports its request");
    });
    end_runtime(a);
    Scene {
        sys,
        b,
        veto,
        supervisor,
        log,
        child,
    }
}

impl Scene {
    /// Makes the supervisor's child crash.
    fn crash_the_child(&self) {
        self.b.block_on(async {
            let crasher = self
                .sys
                .get::<Crasher>(&self.child)
                .expect("the child is registered");
            timeout(BOUND, crasher.notify(()))
                .await
                .expect("the crash message was accepted within the bound")
                .unwrap();
        });
    }
}

/// A budget whose window holds its restarts runs out: each refused restart is
/// charged, and the supervisor ends.
#[test]
fn refused_restarts_after_a_cut_short_walk_exhaust_a_budget_with_a_window() {
    let scene = cut_short_scene(
        "cut-short-budget-window",
        SupervisionConfig::default().max_restarts(3, Duration::from_secs(600)),
    );
    scene.crash_the_child();
    scene.b.block_on(async {
        scene.log.wait_exhausted().await;
        assert!(scene.log.exhausted.load(Ordering::SeqCst));
        timeout(BOUND, scene.supervisor.wait_stopped())
            .await
            .expect("the supervisor ended once its budget ran out");
    });
    end_runtime(scene.b);
}

/// A zero window with `max_restarts` above zero never fills, so the refused
/// restarts keep coming and the supervisor keeps running, until a shutdown
/// reaches it.
#[test]
fn refused_restarts_after_a_cut_short_walk_keep_retrying_under_a_zero_window() {
    let scene = cut_short_scene(
        "cut-short-zero-window",
        SupervisionConfig::default().max_restarts(3, Duration::ZERO),
    );
    scene.crash_the_child();
    scene.b.block_on(async {
        // Far more events than the budget of 3 allows a window that holds
        // its restarts.
        scene.log.wait(25).await;
        assert!(
            !scene.log.exhausted.load(Ordering::SeqCst),
            "a zero window does not exhaust the budget"
        );
        assert_eq!(scene.supervisor.status(), ActorStatus::Running);

        // A shutdown that completes reaches the supervisor and ends the
        // retries.
        scene.veto.store(false, Ordering::SeqCst);
        let report = timeout(
            BOUND,
            scene.sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(30),
                per_actor_timeout: Duration::from_secs(10),
            }),
        )
        .await
        .expect("the later shutdown finished within the bound");
        assert!(
            report
                .outcomes
                .iter()
                .any(|(id, _)| id == scene.supervisor.id()),
            "the later shutdown reached the supervisor"
        );
        timeout(BOUND, scene.supervisor.wait_stopped())
            .await
            .expect("the supervisor ended");
    });
    end_runtime(scene.b);
}

/// A `max_restarts` of zero exhausts the budget at the first restart the
/// strategy would make, whatever the window, so a zero window does not keep a
/// supervisor retrying.
#[test]
fn a_budget_of_zero_restarts_is_exhausted_at_once_under_a_zero_window() {
    let scene = cut_short_scene(
        "cut-short-zero-restarts",
        SupervisionConfig::default().max_restarts(0, Duration::ZERO),
    );
    scene.crash_the_child();
    scene.b.block_on(async {
        scene.log.wait_exhausted().await;
        assert!(scene.log.exhausted.load(Ordering::SeqCst));
        timeout(BOUND, scene.supervisor.wait_stopped())
            .await
            .expect("the supervisor ended once its budget ran out");
    });
    end_runtime(scene.b);
}
