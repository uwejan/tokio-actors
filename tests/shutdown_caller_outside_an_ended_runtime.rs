//! A shutdown call whose walk ended without a report because the runtime it
//! ran on ended, and whose caller is polled from outside that runtime, gets a
//! report or a panic, never a hang: the caller walks the system itself in its
//! own future, and that walk needs a timer on the ended runtime only for a
//! root that is still running.
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, handle::ActorHandle, Actor, ActorExt},
    ActorResult, ActorSystem, ShutdownPolicy, ShutdownReport, StopReason,
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

/// Reports each stop request it is asked about and vetoes it while `veto` is
/// set.
struct Root {
    veto: Arc<AtomicBool>,
    asked: mpsc::UnboundedSender<()>,
}

impl Actor for Root {
    type Message = ();
    type Response = ();

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        let _ = self.asked.send(());
        !self.veto.load(Ordering::SeqCst)
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Wakes the thread that parked in `drive`.
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

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default()
}

/// What the caller's thread saw.
enum Seen {
    Report(ShutdownReport),
    Panic(String),
}

/// A shutdown call polled by a hand-rolled executor on a thread that has
/// `rt_handle` entered, so the runtime that is current at the call is `rt`,
/// which then ends. The thread polls the call once, reports `polled`, waits
/// for `resume`, and then drives the call to its end.
fn caller_thread(
    sys: Arc<ActorSystem>,
    rt_handle: tokio::runtime::Handle,
    polled: std_mpsc::Sender<()>,
    resume: std_mpsc::Receiver<()>,
) -> std::thread::JoinHandle<Seen> {
    std::thread::spawn(move || {
        let _entered = rt_handle.enter();
        let wake = Arc::new(ThreadWaker {
            woken: Mutex::new(false),
            signal: Condvar::new(),
        });
        let waker = Waker::from(wake.clone());
        let mut cx = Context::from_waker(&waker);
        let mut call: Pin<Box<dyn Future<Output = ShutdownReport>>> =
            Box::pin(sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(600),
                per_actor_timeout: Duration::from_secs(600),
            }));
        assert!(matches!(call.as_mut().poll(&mut cx), Poll::Pending));
        polled.send(()).unwrap();
        resume
            .recv_timeout(BOUND)
            .expect("the test let the caller go on within the bound");
        let outcome = catch_unwind(AssertUnwindSafe(|| loop {
            if let Poll::Ready(report) = call.as_mut().poll(&mut cx) {
                return report;
            }
            let mut woken = wake.woken.lock().unwrap();
            while !*woken {
                woken = wake.signal.wait(woken).unwrap();
            }
            *woken = false;
        }));
        match outcome {
            Ok(report) => Seen::Report(report),
            Err(payload) => Seen::Panic(panic_text(payload.as_ref())),
        }
    })
}

/// Joins a thread within `BOUND`; a caller that waits for ever fails here.
fn join_within_the_bound<T: Send + 'static>(thread: std::thread::JoinHandle<T>) -> T {
    let (tx, rx) = std_mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(thread.join());
    });
    rx.recv_timeout(BOUND)
        .expect("the caller did not finish within the bound: it waits for ever")
        .expect("the caller's thread ended cleanly")
}

/// Ends a runtime and returns once every task on it is dropped.
fn end_runtime(rt: Runtime) {
    std::thread::spawn(move || drop(rt))
        .join()
        .expect("the runtime ended cleanly");
}

struct Scene {
    sys: Arc<ActorSystem>,
    root: ActorHandle<Root>,
    veto: Arc<AtomicBool>,
    /// Hosts the root.
    b: Runtime,
    /// The runtime current at the call, ended before the caller goes on.
    a: Runtime,
    caller: std::thread::JoinHandle<Seen>,
    resume: std_mpsc::Sender<()>,
}

/// A root on runtime B that vetoes its stop, a call made with runtime A
/// current whose walk has asked that root, and A ended with the caller's
/// thread parked after its first poll.
fn scene(what: &str) -> Scene {
    let sys = ActorSystem::create(unique(what)).unwrap();
    let veto = Arc::new(AtomicBool::new(true));
    let b = with_timers(2);
    let (asked_tx, mut asked_rx) = mpsc::unbounded_channel();
    let root = b
        .block_on(async {
            timeout(
                BOUND,
                Root {
                    veto: veto.clone(),
                    asked: asked_tx,
                }
                .spawn()
                .named("root")
                .on_system(&sys),
            )
            .await
            .expect("the spawn finished within the bound")
        })
        .unwrap();

    let a = with_timers(1);
    let (polled_tx, polled_rx) = std_mpsc::channel();
    let (resume_tx, resume_rx) = std_mpsc::channel();
    let caller = caller_thread(sys.clone(), a.handle().clone(), polled_tx, resume_rx);
    polled_rx
        .recv_timeout(BOUND)
        .expect("the caller polled its call within the bound");
    b.block_on(async {
        timeout(BOUND, asked_rx.recv())
            .await
            .expect("the walk asked the root within the bound")
            .expect("the root reports its request");
    });
    Scene {
        sys,
        root,
        veto,
        b,
        a,
        caller,
        resume: resume_tx,
    }
}

/// The root has already stopped when the caller goes on: its walk has no root
/// to wait on, needs no timer, completes and returns the report.
#[test]
fn a_caller_outside_the_ended_runtime_returns_the_report_when_the_root_has_stopped() {
    let Scene {
        sys,
        root,
        veto,
        b,
        a,
        caller,
        resume,
    } = scene("ended-runtime-stopped-root");
    end_runtime(a);
    veto.store(false, Ordering::SeqCst);
    b.block_on(async {
        timeout(BOUND, root.stop(StopReason::Graceful))
            .await
            .expect("the stop was accepted within the bound")
            .unwrap();
        timeout(BOUND, root.wait_stopped())
            .await
            .expect("the root stopped within the bound");
    });
    resume.send(()).unwrap();
    match join_within_the_bound(caller) {
        Seen::Report(report) => {
            // The stopped root has left the registry, so the caller's walk
            // has no root to stop and completes.
            assert!(report.outcomes.is_empty());
            assert!(report.swept.is_empty());
        }
        Seen::Panic(text) => panic!("a stopped root needs no timer, got a panic: {text}"),
    }
    assert!(
        sys.reactivate(),
        "the caller's own walk completed and left the system defunct"
    );
    end_runtime(b);
}
