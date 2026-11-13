//! `shutdown_with` needs a Tokio runtime with the time driver that is driven
//! and that outlives the walk. On a runtime built without the driver a
//! shutdown with no root completes and one with a root panics at its first
//! wait on that root; when the runtime the walk runs on ends first, the roots
//! on another runtime keep running and a later shutdown finishes the job.
use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, handle::ActorHandle, Actor, ActorExt},
    ActorResult, ActorStatus, ActorSystem, ShutdownPolicy, SpawnError, StopOutcome, StopReason,
    SystemConfig,
};

/// The longest any wait may take. It only limits how long a real hang takes
/// to report; a slow host must not read as one.
const BOUND: Duration = Duration::from_secs(60);

fn unique(what: &str) -> String {
    format!("{what}-{}", uuid::Uuid::new_v4())
}

/// Runs `body` on its own thread and waits for its result at most `BOUND`,
/// for work that cannot use a Tokio timer because its runtime has none.
fn bounded_thread<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std_mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(body());
    });
    let result = rx
        .recv_timeout(BOUND)
        .expect("the body did not finish within the bound");
    worker.join().expect("the body's thread ended cleanly");
    result
}

fn without_timers() -> Runtime {
    Builder::new_current_thread().build().unwrap()
}

fn with_timers(workers: usize) -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
}

/// Does nothing and never vetoes.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Counts the stop requests it is asked about, reports each on `asked`, and
/// vetoes them while `veto` is set.
struct Root {
    veto: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    asked: mpsc::UnboundedSender<()>,
}

impl Actor for Root {
    type Message = ();
    type Response = ();

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.requests.fetch_add(1, Ordering::SeqCst);
        // The verdict is read before the request is reported, so a caller
        // that changes `veto` when it sees a report changes the verdict of
        // the next request, never of the one reported.
        let accept = !self.veto.load(Ordering::SeqCst);
        let _ = self.asked.send(());
        accept
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn panic_text(payload: &Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default()
}

/// On a runtime without the time driver, a shutdown with no root has nothing
/// to wait on and completes.
#[test]
fn a_shutdown_with_no_root_completes_on_a_runtime_without_the_time_driver() {
    let (outcomes, swept, reactivated) = bounded_thread(|| {
        let sys = ActorSystem::create(unique("timerless-empty")).unwrap();
        let rt = without_timers();
        let report = rt.block_on(sys.shutdown());
        (report.outcomes.len(), report.swept.len(), sys.reactivate())
    });
    assert_eq!(outcomes, 0, "no root was taken on");
    assert_eq!(swept, 0, "nothing was left over");
    assert!(reactivated, "the shutdown completed and none is in flight");
}

/// With a root, the same call panics at its first wait on that root, after
/// the system has entered shutdown, and the system keeps refusing
/// registrations.
#[test]
fn a_shutdown_with_a_root_panics_on_a_runtime_without_the_time_driver() {
    let (panic, refusal, name) = bounded_thread(|| {
        let name = unique("timerless-root");
        let sys = ActorSystem::create(name.clone()).unwrap();
        let rt = without_timers();
        rt.block_on(async { Idle.spawn().named("root").on_system(&sys).await })
            .unwrap();
        let caught = catch_unwind(AssertUnwindSafe(|| rt.block_on(sys.shutdown())));
        let panic = caught.err().map(|payload| panic_text(&payload));
        let refusal = rt
            .block_on(async { Idle.spawn().named("late").on_system(&sys).await })
            .err();
        (panic, refusal, name)
    });
    let panic = panic.expect("the shutdown panics at its first wait on the root");
    assert!(
        panic.contains("timers are disabled") || panic.contains("enable_time"),
        "the panic names the missing time driver, got: {panic}"
    );
    assert_eq!(
        refusal,
        Some(SpawnError::SystemShuttingDown(name)),
        "the system entered shutdown before the panic and stays there"
    );
}

/// A walk cut short when its runtime ends leaves the roots on another runtime
/// running, and a later shutdown on that runtime reports them.
#[test]
fn roots_on_another_runtime_outlive_a_walk_whose_runtime_ended() {
    let name = unique("walk-runtime-ended");
    let sys = ActorSystem::create_with(name.clone(), SystemConfig::default()).unwrap();
    let veto = Arc::new(AtomicBool::new(true));
    let b = with_timers(2);
    let (asked_tx, mut asked_rx) = mpsc::unbounded_channel();

    let spawn_root = |label: &'static str| {
        let requests = Arc::new(AtomicUsize::new(0));
        let handle: ActorHandle<Root> = b
            .block_on(async {
                timeout(
                    BOUND,
                    Root {
                        veto: veto.clone(),
                        requests: requests.clone(),
                        asked: asked_tx.clone(),
                    }
                    .spawn()
                    .named(label)
                    .on_system(&sys),
                )
                .await
                .expect("the spawn finished within the bound")
            })
            .unwrap();
        (handle, requests)
    };
    // The walk stops roots in reverse registration order: it reaches `second`
    // first and waits there on its veto, so `first` is never asked.
    let (first, first_requests) = spawn_root("first");
    let (second, second_requests) = spawn_root("second");

    // Runtime A hosts the call, so the call and its walk end with it.
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
            .expect("the walk asked its first root within the bound")
            .expect("the root reports its request");
    });
    a.shutdown_background();

    // The system entered shutdown with the call, and A's end did not undo it.
    let refusal = b.block_on(async {
        timeout(BOUND, Idle.spawn().named("late").on_system(&sys))
            .await
            .expect("the spawn finished within the bound")
            .err()
    });
    assert_eq!(refusal, Some(SpawnError::SystemShuttingDown(name)));

    // B's roots keep running and the walk never reached the first one.
    assert_eq!(first.status(), ActorStatus::Running);
    assert_eq!(second.status(), ActorStatus::Running);
    assert_eq!(second_requests.load(Ordering::SeqCst), 1);
    assert_eq!(first_requests.load(Ordering::SeqCst), 0);
    b.block_on(async {
        timeout(BOUND, first.send(()))
            .await
            .expect("the root answered within the bound")
            .expect("the root still takes calls");
    });

    // A later shutdown on B takes both roots, now that they no longer veto.
    veto.store(false, Ordering::SeqCst);
    let report = b.block_on(async {
        timeout(
            BOUND,
            sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(30),
                per_actor_timeout: Duration::from_secs(10),
            }),
        )
        .await
        .expect("the later shutdown finished within the bound")
    });
    assert_eq!(report.outcomes.len(), 2, "both roots are in the report");
    for (_, stop) in &report.outcomes {
        assert_eq!(stop.outcome, StopOutcome::Graceful);
    }
    let reported: Vec<_> = report.outcomes.iter().map(|(id, _)| id.clone()).collect();
    assert!(reported.contains(first.id()) && reported.contains(second.id()));
    assert!(report.swept.is_empty());
    b.block_on(async {
        timeout(BOUND, async {
            first.wait_stopped().await;
            second.wait_stopped().await;
        })
        .await
        .expect("both roots ended within the bound");
    });

    // The cut-short call left the walk's slot when A's tasks were dropped, so
    // the completed shutdown can leave `ShuttingDown`.
    b.block_on(async {
        timeout(BOUND, async {
            while !sys.reactivate() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the system left the shutdown within the bound");
    });
}
