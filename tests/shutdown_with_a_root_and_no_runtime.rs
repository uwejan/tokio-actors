//! A shutdown driven with no Tokio runtime current walks in the caller's own
//! future. With no root and no leftover it completes. Once a root exists the
//! walk panics at its first wait on that root, because the wait needs a driven
//! runtime with the time driver, and the system stays in shutdown, refusing
//! registrations.
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, ShutdownPolicy, SpawnError,
};

/// The longest any wait may take. It only limits how long a real hang takes
/// to report; a slow host must not read as one.
const BOUND: Duration = Duration::from_secs(60);

fn unique(what: &str) -> String {
    format!("{what}-{}", uuid::Uuid::new_v4())
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

/// Wakes the thread that parked in `block_on_without_a_runtime`.
struct ThreadWaker {
    woken: Mutex<bool>,
    signal: Condvar,
}

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        *self.woken.lock().unwrap() = true;
        self.signal.notify_one();
    }
}

/// Drives `fut` on the calling thread with no Tokio runtime current.
fn block_on_without_a_runtime<F: Future>(fut: F) -> F::Output {
    let wake = Arc::new(ThreadWaker {
        woken: Mutex::new(false),
        signal: Condvar::new(),
    });
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    let mut fut = Pin::from(Box::new(fut));
    loop {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return out;
        }
        let mut woken = wake.woken.lock().unwrap();
        while !*woken {
            woken = wake.signal.wait(woken).unwrap();
        }
        *woken = false;
    }
}

/// Runs `body` on its own thread and waits for its result at most `BOUND`, so
/// a call that waits for ever fails the test instead of hanging it.
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

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default()
}

fn policy() -> ShutdownPolicy {
    ShutdownPolicy {
        timeout: Duration::from_secs(5),
        per_actor_timeout: Duration::from_secs(1),
    }
}

/// With no runtime current, `shutdown` of a system that has a root panics at
/// its first wait on the root, and the system keeps refusing registrations
/// and does not reactivate.
#[test]
fn a_shutdown_with_no_runtime_current_and_a_root_panics_and_the_system_stays_in_shutdown() {
    let name = unique("no-runtime-root");
    let sys = ActorSystem::create(name.clone()).unwrap();
    // The root lives on a runtime of its own; the call below runs on a thread
    // that has no runtime at all.
    let roots: Runtime = Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    roots
        .block_on(async {
            timeout(BOUND, Idle.spawn().named("root").on_system(&sys))
                .await
                .expect("the spawn finished within the bound")
        })
        .unwrap();

    let caller_system = sys.clone();
    let panic = bounded_thread(move || {
        assert!(tokio::runtime::Handle::try_current().is_err());
        catch_unwind(AssertUnwindSafe(|| {
            block_on_without_a_runtime(caller_system.shutdown_with(policy()))
        }))
        .err()
        .map(|payload| panic_text(payload.as_ref()))
    });
    let panic = panic.expect("the walk panics at its first wait on the root");
    assert!(
        panic.contains("no reactor running") || panic.contains("Tokio 1.x"),
        "the panic is the missing runtime, got: {panic}"
    );

    // The shutdown was entered before the panic and never completed.
    let refusal = roots.block_on(async {
        timeout(BOUND, Idle.spawn().named("late").on_system(&sys))
            .await
            .expect("the spawn finished within the bound")
            .err()
    });
    assert_eq!(refusal, Some(SpawnError::SystemShuttingDown(name)));
    assert!(
        !sys.reactivate(),
        "a shutdown that never completed leaves the system in shutdown"
    );
}
