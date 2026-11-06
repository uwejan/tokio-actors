//! A bounded stop polled after the runtime it was made on has ended panics at
//! its next timed wait unless the actor has reached a terminal status by
//! then, in which case it returns the outcome.

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, StopReason,
};

const BOUND: Duration = Duration::from_secs(10);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// Vetoes every stoppable stop and counts how many times it was asked.
struct Veto {
    asked: Arc<AtomicUsize>,
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
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.open();
    }
}

/// Holds its worker thread in its handler until the test opens the gate.
struct Wedge {
    entered: Option<std::sync::mpsc::Sender<()>>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    stopped_ran: Arc<AtomicBool>,
}

impl Actor for Wedge {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        let (open, changed) = &*self.gate;
        let mut open = open.lock().unwrap();
        while !*open {
            open = changed.wait(open).unwrap();
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped_ran.store(true, Ordering::SeqCst);
        Ok(())
    }
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

/// Waits, on the calling thread, until `ready` holds or the bound passes.
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

/// A call made on a runtime that is then dropped, with the handle left
/// entered so the call can still be polled: the stop task of the call never
/// ran past the window, and its answer closes with the runtime.
#[test]
fn a_call_polled_after_its_runtime_ended_panics_at_its_next_timed_wait() {
    let target_rt = multi_thread(2);
    let asked = Arc::new(AtomicUsize::new(0));
    let h = target_rt.block_on(async {
        Veto {
            asked: asked.clone(),
        }
        .spawn()
        .await
        .unwrap()
    });
    let caller_rt = current_thread();
    let caller_handle = caller_rt.handle().clone();
    let _entered = caller_handle.enter();
    let mut cx = Context::from_waker(Waker::noop());
    let mut call = Box::pin(h.stop_within(StopReason::Graceful, Duration::from_secs(30)));
    assert!(call.as_mut().poll(&mut cx).is_pending());
    wait_until("the veto", || asked.load(Ordering::SeqCst) == 1);

    drop(caller_rt);

    let polled = catch_unwind(AssertUnwindSafe(|| call.as_mut().poll(&mut cx)));
    assert!(
        polled.is_err(),
        "the next timed wait panics while the actor is not terminal: {:?}",
        polled.map(|p| matches!(p, Poll::Ready(_)))
    );
}

/// The same call polled after its runtime ended and after the actor ended
/// returns the outcome: the wait reads the status plane before it uses a
/// timer.
#[test]
fn a_call_polled_after_its_runtime_ended_returns_when_the_actor_is_terminal() {
    let target_rt = multi_thread(2);
    let asked = Arc::new(AtomicUsize::new(0));
    let h = target_rt.block_on(async {
        Veto {
            asked: asked.clone(),
        }
        .spawn()
        .await
        .unwrap()
    });
    let caller_rt = current_thread();
    let caller_handle = caller_rt.handle().clone();
    let _entered = caller_handle.enter();
    let mut cx = Context::from_waker(Waker::noop());
    let mut call = Box::pin(h.stop_within(StopReason::Graceful, Duration::from_secs(30)));
    assert!(call.as_mut().poll(&mut cx).is_pending());
    wait_until("the veto", || asked.load(Ordering::SeqCst) == 1);

    drop(caller_rt);
    target_rt.block_on(async {
        h.stop(StopReason::Kill).await.unwrap();
        timeout(BOUND, h.wait_stopped())
            .await
            .expect("the actor ends");
    });

    let polled = catch_unwind(AssertUnwindSafe(|| call.as_mut().poll(&mut cx)));
    assert!(
        matches!(polled, Ok(Poll::Ready(Ok(_)))),
        "a terminal actor is answered without a timer: {:?}",
        polled.map(|p| matches!(p, Poll::Ready(_)))
    );
}

/// A `Kill` call's first timed wait is the window of the call: polled after
/// its runtime ended, with the target still holding its thread, it panics.
#[test]
fn a_kill_call_polled_after_its_runtime_ended_panics_at_its_first_timed_wait() {
    let target_rt = multi_thread(2);
    let gate = Gate(Arc::new((Mutex::new(false), Condvar::new())));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let stopped_ran = Arc::new(AtomicBool::new(false));
    let h = target_rt.block_on(async {
        let h = Wedge {
            entered: Some(entered_tx),
            gate: gate.0.clone(),
            stopped_ran: stopped_ran.clone(),
        }
        .spawn()
        .kill_grace(Duration::from_secs(3600))
        .await
        .unwrap();
        h.notify(()).await.unwrap();
        h
    });
    entered_rx
        .recv_timeout(HANG_BOUND)
        .expect("the handler holds its thread");

    let caller_rt = current_thread();
    let caller_handle = caller_rt.handle().clone();
    let _entered = caller_handle.enter();
    let mut cx = Context::from_waker(Waker::noop());
    let mut call = Box::pin(h.stop_within(StopReason::Kill, Duration::from_secs(30)));
    assert!(call.as_mut().poll(&mut cx).is_pending());

    drop(caller_rt);

    let polled = catch_unwind(AssertUnwindSafe(|| call.as_mut().poll(&mut cx)));
    assert!(
        polled.is_err(),
        "the first timed wait of a Kill call panics while the actor is not terminal: {:?}",
        polled.map(|p| matches!(p, Poll::Ready(_)))
    );
    gate.open();
    drop(call);
    drop(h);
}
