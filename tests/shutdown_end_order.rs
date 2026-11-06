//! Two shutdowns of one system that overlap end in one outcome whatever order
//! they end in: once a shutdown has completed, the system reads `Defunct`
//! (and `reactivate` works) when the last shutdown in flight ends, whether
//! that last one is the completed shutdown or a shutdown that was cut short
//! when its runtime ended.
//!
//! The root is held inside a handler that does not return until a gate opens,
//! so a stop request cannot reach it: a walk with a long window waits on it
//! for as long as the gate stays shut, and a walk with a short window gives
//! up on it after its ladder and completes.
use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::runtime::{Builder, Runtime};
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, ShutdownPolicy, ShutdownReport,
};

/// The longest any wait may take. It only limits how long a real hang takes
/// to report; a slow host must not read as one.
const BOUND: Duration = Duration::from_secs(60);

fn unique(what: &str) -> String {
    format!("{what}-{}", uuid::Uuid::new_v4())
}

fn runtime(workers: usize) -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
}

/// A gate that holds a handler until a test opens it, and gives up on its own
/// after `BOUND` so a failed test cannot leave a thread parked for good.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *self.0 .0.lock().unwrap() = true;
        self.0 .1.notify_all();
    }

    fn wait(&self) {
        let guard = self.0 .0.lock().unwrap();
        let _ = self.0 .1.wait_timeout_while(guard, BOUND, |open| !*open);
    }
}

/// Holds its thread in the handler of its first message until the gate opens.
struct Held {
    entered: Option<std_mpsc::Sender<()>>,
    gate: Gate,
}

impl Actor for Held {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        self.gate.wait();
        Ok(())
    }
}

/// Does nothing.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A system with one root that sits inside its handler until `gate` opens.
struct Fixture {
    sys: Arc<ActorSystem>,
    gate: Gate,
    /// Hosts the root; kept alive for the whole test.
    _roots: Runtime,
}

fn fixture(what: &str) -> Fixture {
    let sys = ActorSystem::create(unique(what)).unwrap();
    let roots = runtime(2);
    let gate = Gate::default();
    let (entered_tx, entered_rx) = std_mpsc::channel();
    let held = roots.block_on(async {
        timeout(
            BOUND,
            Held {
                entered: Some(entered_tx),
                gate: gate.clone(),
            }
            .spawn()
            .named("held")
            .on_system(&sys),
        )
        .await
        .expect("the spawn finished within the bound")
        .unwrap()
    });
    roots.block_on(async {
        timeout(BOUND, held.notify(()))
            .await
            .expect("the message was accepted within the bound")
            .unwrap();
    });
    entered_rx
        .recv_timeout(BOUND)
        .expect("the root entered its handler within the bound");
    Fixture {
        sys,
        gate,
        _roots: roots,
    }
}

/// A window long enough that a walk never reaches its end by the clock.
fn long_window() -> ShutdownPolicy {
    ShutdownPolicy {
        timeout: Duration::from_secs(600),
        per_actor_timeout: Duration::from_secs(600),
    }
}

/// A window that ends the ladder of a root that cannot yield within about a
/// second.
fn short_window() -> ShutdownPolicy {
    ShutdownPolicy {
        timeout: Duration::from_secs(20),
        per_actor_timeout: Duration::from_millis(100),
    }
}

type Call<'a> = Pin<Box<dyn Future<Output = ShutdownReport> + 'a>>;

/// Polls a shutdown call once on `rt`: the call enters the shutdown and starts
/// its walk on `rt` in that first poll, and then waits for the walk's report.
fn start_call<'a>(rt: &Runtime, sys: &'a Arc<ActorSystem>, policy: ShutdownPolicy) -> Call<'a> {
    let _entered = rt.enter();
    let mut call: Call<'a> = Box::pin(sys.shutdown_with(policy));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(call.as_mut().poll(&mut cx), Poll::Pending),
        "the call waits for its walk"
    );
    call
}

/// Ends a runtime and its tasks, and returns once every task is dropped.
fn end_runtime(rt: Runtime) {
    std::thread::spawn(move || drop(rt))
        .join()
        .expect("the runtime ended cleanly");
}

/// Whether `reactivate` works within the bound, which is the system reading
/// `Defunct` with no shutdown in flight.
fn reactivates_within_the_bound(sys: &ActorSystem) -> bool {
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if sys.reactivate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

fn registers_again(sys: &Arc<ActorSystem>, rt: &Runtime) -> bool {
    rt.block_on(async {
        timeout(BOUND, Idle.spawn().named("again").on_system(sys))
            .await
            .expect("the spawn finished within the bound")
            .is_ok()
    })
}

/// The shutdown that completes ends first and the cut-short one ends last:
/// the last to end moves the system to `Defunct`.
#[test]
fn two_overlapping_shutdowns_leave_defunct_when_the_completed_one_ends_first() {
    let fx = fixture("end-order-completed-first");
    let sys = &fx.sys;
    let a = runtime(1);
    let c = runtime(1);

    // The long call is cut short by its runtime ending; the short call
    // completes with the held root reported unresponsive.
    let first = start_call(&a, sys, long_window());
    drop(first);
    let second = start_call(&c, sys, short_window());
    let report = c
        .block_on(async { timeout(BOUND, second).await })
        .expect("the short call returned within the bound");
    assert_eq!(report.outcomes.len(), 1, "the held root is in the report");
    assert!(
        !sys.reactivate(),
        "the cut-short shutdown is still in flight"
    );

    end_runtime(a);
    assert!(
        reactivates_within_the_bound(sys),
        "the system reads Defunct once the cut-short shutdown ends after the completed one"
    );
    fx.gate.open();
    assert!(registers_again(sys, &c));
    end_runtime(c);
}

/// The cut-short shutdown ends first and the one that completes ends last:
/// the last to end moves the system to `Defunct`.
#[test]
fn two_overlapping_shutdowns_leave_defunct_when_the_cut_short_one_ends_first() {
    let fx = fixture("end-order-cut-short-first");
    let sys = &fx.sys;
    let a = runtime(1);
    let c = runtime(1);

    let first = start_call(&a, sys, long_window());
    drop(first);
    let second = start_call(&c, sys, long_window());

    end_runtime(a);
    assert!(
        !sys.reactivate(),
        "the shutdown that has not completed is still in flight"
    );

    // The held root takes its stop request once the gate opens, and the
    // second walk completes.
    fx.gate.open();
    let report = c
        .block_on(async { timeout(BOUND, second).await })
        .expect("the second call returned within the bound");
    assert_eq!(report.outcomes.len(), 1, "the held root is in the report");
    assert!(
        reactivates_within_the_bound(sys),
        "the system reads Defunct once the completed shutdown ends after the cut-short one"
    );
    assert!(registers_again(sys, &c));
    end_runtime(c);
}
