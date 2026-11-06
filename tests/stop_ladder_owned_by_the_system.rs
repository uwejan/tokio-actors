//! A bounded stop is owned by the system from the moment the call is made:
//! the escalation ladder runs in a task of the system on the target's
//! runtime, so a caller that stops waiting, or never polls its future again,
//! does not stop the ladder, and a caller that waits is answered exactly as
//! it was when the caller ran the ladder itself.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, StopOutcome, StopReason,
};

mod common;

/// How far past its exact instant an outcome may land on the paused clock:
/// one poll step of a waiting loop and the timers' rounding, far short of any
/// window a test configures.
const PAUSED_SLACK: Duration = Duration::from_millis(30);

// ---------------------------------------------------------------------------
// The allocator: counts the live bytes of the threads that opt in.
// ---------------------------------------------------------------------------

thread_local! {
    static MEASURED: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<isize> = const { Cell::new(0) };
}

struct Counting;

fn note(delta: isize) {
    let _ = MEASURED.try_with(|on| {
        if on.get() {
            let _ = LIVE.try_with(|live| live.set(live.get() + delta));
        }
    });
}

// SAFETY: every call is forwarded unchanged to the system allocator; the
// counters are thread-local cells that never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size() as isize);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note(-(layout.size() as isize));
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

// ---------------------------------------------------------------------------
// Actors
// ---------------------------------------------------------------------------

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

/// Blocks its worker thread in its handler until the test lets it go.
struct Blocker {
    entered: Option<std::sync::mpsc::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}

impl Actor for Blocker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        let _ = self.release.recv();
        Ok(())
    }
}

/// Polls `call` once, which is when the call commits the system to its
/// ladder, and leaves it where it is.
async fn poll_once<F: Future>(call: &mut Pin<Box<F>>) {
    poll_fn(|cx| {
        let _ = call.as_mut().poll(cx);
        Poll::Ready(())
    })
    .await;
}

// ---------------------------------------------------------------------------
// An abandoned call is still escalated
// ---------------------------------------------------------------------------

/// An abandoned call raises the ladder's `Kill` itself, not only the abort:
/// a vetoing actor that is idle ends when the window passes, not when the
/// kill grace after it passes as well. The paused clock makes both instants
/// exact.
#[test]
fn an_abandoned_bounded_stop_raises_its_kill_at_the_end_of_its_window() {
    common::run_paused(async {
        let asked = Arc::new(AtomicUsize::new(0));
        let h = Veto {
            asked: asked.clone(),
        }
        .spawn()
        .kill_grace(Duration::from_secs(5))
        .await
        .unwrap();
        let started = tokio::time::Instant::now();
        let mut call = Box::pin(h.stop_within(StopReason::Graceful, Duration::from_millis(200)));
        poll_once(&mut call).await;
        drop(call);
        let reason = timeout(Duration::from_secs(60), h.wait_stopped())
            .await
            .expect("the abandoned call's ladder ends the actor");
        assert_eq!(reason, StopReason::Kill);
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(200) && took < Duration::from_millis(200) + PAUSED_SLACK,
            "the Kill came at the end of the window, took {took:?}"
        );
        assert_eq!(asked.load(Ordering::SeqCst), 1, "a Kill is not vetoable");
    });
}

// ---------------------------------------------------------------------------
// A waiting caller gets every outcome it got when it ran the ladder itself
// ---------------------------------------------------------------------------

/// A handler that blocks its thread is beyond every bound: the call reports
/// `Unresponsive` once the window, the kill grace and the settle wait have
/// passed, and the actor is not given up on: it ends when the handler returns.
#[test]
fn a_blocked_thread_reads_unresponsive() {
    common::run_multi_thread_only(2, async {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let h = Blocker {
            entered: Some(entered_tx),
            release: release_rx,
        }
        .spawn()
        .kill_grace(Duration::from_millis(100))
        .await
        .unwrap();
        h.notify(()).await.unwrap();
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .expect("the handler is entered");
        let started = Instant::now();
        let outcome = h
            .stop_within(StopReason::Graceful, Duration::from_millis(50))
            .await
            .unwrap();
        let took = started.elapsed();
        release_tx.send(()).unwrap();
        assert_eq!(outcome, StopOutcome::Unresponsive);
        assert!(
            took >= Duration::from_millis(250),
            "the window, the grace and the settle wait, took {took:?}"
        );
        timeout(Duration::from_secs(5), h.wait_stopped())
            .await
            .expect("the actor ends once its handler returns");
    });
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A shutdown still finishes with calls in flight
// ---------------------------------------------------------------------------
