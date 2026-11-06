//! A bounded stop polled by a foreign executor, outside the runtime that
//! drives it.
//!
//! A call with a reason other than `Kill` that is polled by a thread with no
//! Tokio runtime context at all, after the calling runtime ended, panics at
//! its closing wait whatever the actor's status. A call polled under a
//! runtime that is entered but never driven completes when the actor ends
//! from the `Kill` that a zero window raises inside the call. A `Kill` call
//! under such a runtime completes when the actor ends, from the `Kill` or from
//! the abort the system armed on the actor's own runtime.
//!
//! The calling runtimes here are current-thread runtimes that the test drives
//! by hand or leaves undriven, so the order of the poll, of the runtime's end
//! and of the actor's end is fixed by observation. The actor's runtime in the
//! `Kill` tests follows the flavor variable of the other suites
//! (`TOKIO_ACTORS_TEST_FLAVOR=current_thread` selects a current-thread
//! runtime) and is driven on a thread of its own.

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::JoinHandle;
use std::thread::Thread;
use std::time::{Duration, Instant};

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorStatus, StopOutcome, StopReason,
};

/// Long enough that a real hang fails the test, never a condition's wait.
const BOUND: Duration = Duration::from_secs(10);

/// The window of a call that must not end by its own deadline during a test.
const LONG_WINDOW: Duration = Duration::from_secs(30);

/// Vetoes every stoppable stop, and counts the stop hooks that ran.
struct Veto {
    asked: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
}

impl Actor for Veto {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.asked.fetch_add(1, Ordering::SeqCst);
        false
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Counters {
    asked: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
}

fn counters() -> (Counters, Veto) {
    let asked = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicUsize::new(0));
    let veto = Veto {
        asked: asked.clone(),
        stopped: stopped.clone(),
    };
    (Counters { asked, stopped }, veto)
}

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn multi_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Waits, on the calling thread, until `ready` holds or the bound passes.
fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(started.elapsed() < BOUND, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A waker that unparks the thread that polls the call.
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Sets a flag on drop, so a polling thread gives up when the test unwinds.
struct GiveUp(Arc<AtomicBool>);

impl Drop for GiveUp {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The message of a caught panic.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|text| text.to_string()))
        .unwrap_or_else(|| "a panic that carries no text".to_string())
}

/// Starts a `Graceful` call with a long window under an entered, undriven
/// runtime, polls it once from this thread, and ends the runtime. The call is
/// returned with the runtime gone and no runtime context left on this thread.
fn call_whose_runtime_ended<'a>(
    target: &'a ActorHandle<Veto>,
    counters: &Counters,
) -> impl Future<Output = Result<StopOutcome, tokio_actors::SendError>> + Send + 'a {
    let caller_rt = current_thread();
    let mut call = Box::pin(target.stop_within(StopReason::Graceful, LONG_WINDOW));
    {
        let _entered = caller_rt.handle().enter();
        let first = call.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(
            first.is_pending(),
            "the call has nothing to answer with yet"
        );
    }
    // The actor took the request and vetoed it, so it is running and the
    // call has nothing to wait for but the stop task.
    wait_until("the veto", || counters.asked.load(Ordering::SeqCst) == 1);
    // The stop task was spawned on the undriven runtime and never ran; its
    // answer channel closes with the runtime.
    drop(caller_rt);
    call
}

/// Polls `call` once from a plain thread of its own, with no runtime context
/// and a waker that does nothing; the panic of the poll, if any, comes back
/// as its message.
fn poll_without_a_runtime_context<F>(
    call: F,
) -> Result<Poll<Result<StopOutcome, tokio_actors::SendError>>, String>
where
    F: Future<Output = Result<StopOutcome, tokio_actors::SendError>> + Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                assert!(
                    tokio::runtime::Handle::try_current().is_err(),
                    "the polling thread has a runtime context"
                );
                let mut call = Box::pin(call);
                catch_unwind(AssertUnwindSafe(|| {
                    call.as_mut().poll(&mut Context::from_waker(Waker::noop()))
                }))
                .map_err(panic_message)
            })
            .join()
            .expect("the polling thread ended without an outcome")
    })
}

fn assert_panics_for_the_missing_runtime(
    outcome: Result<Poll<Result<StopOutcome, tokio_actors::SendError>>, String>,
    why: &str,
) {
    let message = match outcome {
        Err(message) => message,
        Ok(polled) => panic!("the poll did not panic ({why}): {polled:?}"),
    };
    assert!(
        message.to_lowercase().contains("runtime") || message.to_lowercase().contains("reactor"),
        "the panic does not name the missing runtime: {message}"
    );
}

/// The same call when the actor has already ended: the poll still panics,
/// because the closing wait creates its timer before it reads the status
/// plane, so the actor's status does not decide it.
#[test]
fn a_call_polled_with_no_runtime_context_after_its_runtime_ended_panics_for_a_stopped_actor() {
    let target_rt = multi_thread();
    let (counts, veto) = counters();
    let target = target_rt.block_on(async { veto.spawn().await.unwrap() });

    let call = call_whose_runtime_ended(&target, &counts);
    target_rt.block_on(async {
        target.stop(StopReason::Kill).await.unwrap();
        timeout(BOUND, target.wait_stopped())
            .await
            .expect("the actor ends");
    });
    assert_eq!(target.status(), ActorStatus::Stopped, "the actor ended");

    let outcome = poll_without_a_runtime_context(call);
    assert_panics_for_the_missing_runtime(outcome, "the actor had stopped");
}

/// A zero window raises the `Kill` inside the call, so a caller polled by a
/// foreign executor under a runtime that is entered but never driven needs
/// no ladder: the call completes with `Killed` when the actor, which would
/// have vetoed a graceful stop, ends. The actor runs on a runtime of its own
/// that is driven only after the call was polled, so it has not run since the
/// request and skips its stop hooks.
#[test]
fn a_zero_window_call_under_an_undriven_runtime_completes_when_the_kill_ends_the_actor() {
    let target_rt = current_thread();
    let (counts, veto) = counters();
    let target = target_rt.block_on(async { veto.spawn().await.unwrap() });
    let caller_rt = current_thread();
    let caller_handle = caller_rt.handle().clone();

    let give_up = Arc::new(AtomicBool::new(false));
    let (polled_tx, polled_rx) = std::sync::mpsc::channel();

    let outcome = std::thread::scope(|scope| {
        let _release = GiveUp(give_up.clone());
        let poller = scope.spawn(|| {
            let _entered = caller_handle.enter();
            let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
            let mut cx = Context::from_waker(&waker);
            let mut call = Box::pin(target.stop_within(StopReason::Graceful, Duration::ZERO));
            let started = Instant::now();
            let mut first = true;
            loop {
                match call.as_mut().poll(&mut cx) {
                    Poll::Ready(outcome) => return Some(outcome),
                    Poll::Pending => {
                        if first {
                            first = false;
                            polled_tx.send(()).unwrap();
                        }
                    }
                }
                if give_up.load(Ordering::SeqCst) || started.elapsed() > 2 * BOUND {
                    return None;
                }
                std::thread::park_timeout(Duration::from_millis(20));
            }
        });
        polled_rx
            .recv_timeout(BOUND)
            .expect("the call was polled once and is waiting for the actor");
        assert_eq!(counts.asked.load(Ordering::SeqCst), 0);
        // Only now does the actor's runtime run: it finds the `Kill` first.
        target_rt.block_on(async {
            timeout(BOUND, target.wait_stopped())
                .await
                .expect("the Kill of the zero window ends the actor");
        });
        poller.join().expect("the polling thread panicked")
    });

    assert!(
        matches!(outcome, Some(Ok(StopOutcome::Killed))),
        "the call did not complete with Killed: {outcome:?}"
    );
    assert_eq!(counts.asked.load(Ordering::SeqCst), 0, "pre_stop ran");
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 0, "on_stopped ran");
    drop(caller_rt);
}

/// Does nothing, and parks forever inside `handle` when it gets a message.
struct Parker {
    entered: Arc<AtomicBool>,
}

impl Actor for Parker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// The runtime an actor lives on in the `Kill` tests: the flavor variable
/// of the other suites picks current-thread or multi-thread.
fn actor_runtime() -> tokio::runtime::Runtime {
    if std::env::var("TOKIO_ACTORS_TEST_FLAVOR").as_deref() == Ok("current_thread") {
        current_thread()
    } else {
        multi_thread()
    }
}

/// A runtime that a thread of its own drives until it is released.
struct Driven {
    release: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Driven {
    fn start(runtime: tokio::runtime::Runtime) -> Self {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            runtime.block_on(async {
                let _ = released.await;
            });
        });
        Self {
            release: Some(release),
            thread: Some(thread),
        }
    }
}

impl Drop for Driven {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Polls a `Kill` call from a plain thread that has the undriven `caller_rt`
/// entered, with a waker that unparks the thread and a std-time deadline,
/// and returns the call's result (`None` if it was not ready by then) with
/// the time it took.
fn kill_call_under_an_undriven_runtime<A: Actor>(
    caller_rt: &tokio::runtime::Runtime,
    target: &ActorHandle<A>,
    window: Duration,
) -> (
    Option<Result<StopOutcome, tokio_actors::SendError>>,
    Duration,
) {
    let caller_handle = caller_rt.handle().clone();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _entered = caller_handle.enter();
                let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
                let mut cx = Context::from_waker(&waker);
                let mut call = Box::pin(target.stop_within(StopReason::Kill, window));
                let started = Instant::now();
                loop {
                    if let Poll::Ready(outcome) = call.as_mut().poll(&mut cx) {
                        return (Some(outcome), started.elapsed());
                    }
                    if started.elapsed() > BOUND {
                        return (None, started.elapsed());
                    }
                    std::thread::park_timeout(Duration::from_millis(20));
                }
            })
            .join()
            .expect("the polling thread panicked")
    })
}

/// A `Kill` call under a calling runtime that is entered but never driven,
/// for an actor parked in a handler that never returns: it cannot take the
/// `Kill` at a turn boundary, so the call completes with `Aborted` once the
/// abort that the system armed on the actor's own runtime ends it, after the
/// window and not much later.
#[test]
fn a_kill_call_under_an_undriven_runtime_completes_when_the_abort_ends_a_parked_actor() {
    let entered = Arc::new(AtomicBool::new(false));
    let target_rt = actor_runtime();
    let target = target_rt.block_on(async {
        let target = Parker {
            entered: entered.clone(),
        }
        .spawn()
        .await
        .unwrap();
        target.notify(()).await.unwrap();
        target
    });
    let caller_rt = current_thread();
    let window = Duration::from_millis(400);
    let margin = Duration::from_secs(2);

    let driven = Driven::start(target_rt);
    wait_until("the handler", || entered.load(Ordering::SeqCst));
    let (outcome, elapsed) = kill_call_under_an_undriven_runtime(&caller_rt, &target, window);

    assert!(
        matches!(outcome, Some(Ok(StopOutcome::Aborted))),
        "the call did not complete with Aborted: {outcome:?}"
    );
    assert!(
        elapsed >= window,
        "the abort came before the window: {elapsed:?}"
    );
    assert!(
        elapsed < window + margin,
        "the abort came later than the window and the margin: {elapsed:?}"
    );
    drop(driven);
    drop(caller_rt);
}
