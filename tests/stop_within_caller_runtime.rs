#![allow(dead_code)]
//! A bounded stop runs its ladder as a task of the system on the runtime of
//! the caller, and the caller only waits: the shape of the two runtimes, a
//! runtime that is entered but not driven, a runtime that ends mid-call and a
//! caller with no runtime at all each get an answer that does not depend on
//! anything but the actor.
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, StopOutcome, StopReason,
};

const BOUND: Duration = Duration::from_secs(10);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// Answers every message and stops on request.
#[derive(Default)]
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Vetoes every stoppable stop and counts how many times it was asked.
struct Veto {
    asked: Arc<AtomicUsize>,
}

impl Veto {
    fn new(asked: &Arc<AtomicUsize>) -> Self {
        Self {
            asked: asked.clone(),
        }
    }
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
}

/// The gate a `Wedge` waits on. It opens on drop too, so a failed assertion
/// never leaves a runtime blocked.
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *(self.0).0.lock().unwrap() = true;
        (self.0).1.notify_all();
    }

    fn is_open(&self) -> bool {
        *(self.0).0.lock().unwrap()
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.open();
    }
}

struct ThreadWake(std::thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Polls `fut` on the calling thread, outside any runtime's executor, until
/// it is ready or `limit` has passed.
fn foreign_block_on<F: Future>(fut: F, limit: Duration) -> Option<F::Output> {
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = pin!(fut);
    let started = Instant::now();
    while started.elapsed() < limit {
        if let Poll::Ready(output) = fut.as_mut().poll(&mut cx) {
            return Some(output);
        }
        std::thread::park_timeout(Duration::from_millis(5));
    }
    None
}

fn multi_thread(workers: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
}

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Waits, on the calling thread, until `ready` holds or the bound passes. The bound only limits how long a real hang takes to report.
fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < HANG_BOUND,
            "timed out waiting for: {what}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

// ---------------------------------------------------------------------------
// Two runtimes
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A runtime that is entered but not driven
// ---------------------------------------------------------------------------

/// A graceful stop of an actor that cooperates completes for a caller polled
/// by a foreign executor under a runtime that is entered and never driven:
/// the call waits on the actor's end, which needs no task of the runtime.
#[test]
fn a_cooperative_stop_completes_under_an_undriven_runtime() {
    let target_rt = multi_thread(2);
    let h = target_rt.block_on(async { Idle.spawn().await.unwrap() });
    let caller_rt = current_thread();
    let _entered = caller_rt.enter();
    let outcome = foreign_block_on(
        h.stop_within(StopReason::Graceful, Duration::from_millis(200)),
        BOUND,
    )
    .expect("the call completes without the runtime being driven")
    .unwrap();
    assert_eq!(outcome, StopOutcome::Graceful);
}

/// A vetoing actor cannot be escalated by a runtime nothing drives: the call
/// stays pending, and when the runtime ends, the task it never ran hands the
/// steps to the actor's own runtime, which kills the actor.
#[test]
fn a_vetoed_stop_stays_pending_under_an_undriven_runtime_and_ends_with_the_runtime() {
    let target_rt = multi_thread(2);
    let asked = Arc::new(AtomicUsize::new(0));
    let h = target_rt.block_on(async { Veto::new(&asked).spawn().await.unwrap() });
    let caller_rt = current_thread();
    let entered = caller_rt.enter();
    let mut cx = Context::from_waker(Waker::noop());
    let mut call = Box::pin(h.stop_within(StopReason::Graceful, Duration::from_millis(100)));
    assert!(call.as_mut().poll(&mut cx).is_pending());
    wait_until("the veto", || asked.load(Ordering::SeqCst) == 1);
    assert!(
        call.as_mut().poll(&mut cx).is_pending(),
        "nothing but the actor's end can complete the call"
    );
    drop(call);
    drop(entered);
    drop(caller_rt);
    let reason = target_rt
        .block_on(async { timeout(BOUND, h.wait_stopped()).await })
        .expect("the steps the runtime never ran were handed to the actor's runtime");
    assert_eq!(reason, StopReason::Kill);
}

// ---------------------------------------------------------------------------
// A runtime that ends mid-call
// ---------------------------------------------------------------------------

/// The target's runtime has no time driver and the caller's runtime ends with
/// a bounded stop pending: nothing can take the steps there, so the actor is
/// left alone, as it is when the caller's future is dropped, and is not ended
/// before the window.
#[test]
fn a_caller_runtime_that_ends_leaves_a_target_on_a_runtime_without_timers_alone() {
    let target_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let caller_rt = multi_thread(2);
    let asked = Arc::new(AtomicUsize::new(0));
    let h = target_rt.block_on(async { Veto::new(&asked).spawn().await.unwrap() });
    {
        let h = h.clone();
        caller_rt.spawn(async move {
            let _ = h
                .stop_within(StopReason::Graceful, Duration::from_secs(30))
                .await;
        });
    }
    wait_until("the veto", || asked.load(Ordering::SeqCst) == 1);
    caller_rt.shutdown_background();
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        h.status(),
        ActorStatus::Running,
        "the actor was ended long before the window of the call"
    );
    drop(target_rt);
}

// ---------------------------------------------------------------------------
// No runtime
// ---------------------------------------------------------------------------

/// A bounded stop polled with no runtime current panics at its entry, before
/// it raises anything: the actor is never asked to stop.
#[test]
fn a_bounded_stop_without_a_runtime_panics_before_it_raises_anything() {
    let target_rt = multi_thread(2);
    let asked = Arc::new(AtomicUsize::new(0));
    let h = target_rt.block_on(async { Veto::new(&asked).spawn().await.unwrap() });
    let polled = catch_unwind(AssertUnwindSafe(|| {
        foreign_block_on(
            h.stop_within(StopReason::Graceful, Duration::from_millis(100)),
            Duration::from_secs(2),
        )
    }));
    assert!(polled.is_err(), "the call panics at its entry");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(asked.load(Ordering::SeqCst), 0, "nothing was raised");
    assert_eq!(h.status(), ActorStatus::Running);
}

// ---------------------------------------------------------------------------
// An actor that has already ended
// ---------------------------------------------------------------------------
