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

use tokio::sync::{oneshot, Notify};
use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorStatus, ActorSystem, SendError, ShutdownPolicy, StopOutcome,
    StopReason,
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

fn live_bytes() -> isize {
    LIVE.with(|live| live.get())
}

// ---------------------------------------------------------------------------
// Actors
// ---------------------------------------------------------------------------

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

/// Parks in its handler until the gate opens (when it has one), and tells the
/// test when it is parked.
struct Parked {
    entered: Option<oneshot::Sender<()>>,
    gate: Option<Arc<Notify>>,
}

impl Parked {
    fn forever() -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                entered: Some(tx),
                gate: None,
            },
            rx,
        )
    }

    fn until(gate: Arc<Notify>) -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                entered: Some(tx),
                gate: Some(gate),
            },
            rx,
        )
    }
}

impl Actor for Parked {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        match &self.gate {
            Some(gate) => gate.notified().await,
            None => std::future::pending::<()>().await,
        }
        Ok(())
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

async fn park<A>(handle: &ActorHandle<A>, entered: oneshot::Receiver<()>)
where
    A: Actor<Message = ()>,
{
    handle.notify(()).await.unwrap();
    timeout(Duration::from_secs(5), entered)
        .await
        .expect("the handler is entered")
        .unwrap();
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

/// The caller stops waiting right after its request committed the system to a
/// 300 ms window: the actor, stuck in a handler, is still taken by the
/// ladder's abort.
#[test]
fn an_abandoned_bounded_stop_still_reaches_its_kill_step() {
    common::run(2, async {
        let (stuck, entered) = Parked::forever();
        let h = stuck.spawn().await.unwrap();
        park(&h, entered).await;
        let mut call = Box::pin(h.stop_within(StopReason::Graceful, Duration::from_millis(300)));
        poll_once(&mut call).await;
        drop(call);
        assert!(
            timeout(Duration::from_secs(10), h.wait_stopped())
                .await
                .is_ok(),
            "the bounded stop the caller abandoned never escalated"
        );
        assert_eq!(h.status(), ActorStatus::Stopped);
    });
}

/// The same call, kept alive and never polled again.
#[test]
fn a_stored_bounded_stop_that_is_never_polled_again_still_escalates() {
    common::run(2, async {
        let (stuck, entered) = Parked::forever();
        let h = stuck.spawn().await.unwrap();
        park(&h, entered).await;
        let mut call = Box::pin(h.stop_within(StopReason::Graceful, Duration::from_millis(300)));
        poll_once(&mut call).await;
        assert!(
            timeout(Duration::from_secs(10), h.wait_stopped())
                .await
                .is_ok(),
            "the stored call never escalated"
        );
        assert_eq!(h.status(), ActorStatus::Stopped);
        drop(call);
    });
}

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

/// An abandoned `kill_by_id` still ends the actor.
#[test]
fn an_abandoned_kill_by_id_still_ends_the_actor() {
    common::run(2, async {
        let sys = ActorSystem::create("ladder-owned-kill-by-id").unwrap();
        let (stuck, entered) = Parked::forever();
        let h = stuck
            .spawn()
            .named("stuck")
            .on_system(&sys)
            .kill_grace(Duration::from_millis(200))
            .await
            .unwrap();
        park(&h, entered).await;
        let id = h.id().clone();
        let mut call = Box::pin(sys.kill_by_id(&id));
        poll_once(&mut call).await;
        drop(call);
        let reason = timeout(Duration::from_secs(10), h.wait_stopped())
            .await
            .expect("the abandoned kill never ended the actor");
        assert_eq!(h.status(), ActorStatus::Stopped);
        assert_eq!(reason, StopReason::Kill);
    });
}

/// A `Kill` through the handle whose future is not awaited further still ends
/// the actor.
#[test]
fn a_handle_kill_that_is_not_awaited_further_still_ends_the_actor() {
    common::run(2, async {
        let sys = ActorSystem::create("ladder-owned-handle-kill").unwrap();
        let (stuck, entered) = Parked::forever();
        let h = stuck
            .spawn()
            .named("stuck")
            .on_system(&sys)
            .kill_grace(Duration::from_millis(200))
            .await
            .unwrap();
        park(&h, entered).await;
        h.stop(StopReason::Kill).await.unwrap();
        let reason = timeout(Duration::from_secs(10), h.wait_stopped())
            .await
            .expect("the unawaited kill never ended the actor");
        assert_eq!(h.status(), ActorStatus::Stopped);
        assert_eq!(reason, StopReason::Kill);
    });
}

// ---------------------------------------------------------------------------
// A waiting caller gets every outcome it got when it ran the ladder itself
// ---------------------------------------------------------------------------

#[test]
fn a_graceful_end_reads_graceful() {
    common::run_paused(async {
        let h = Idle.spawn().await.unwrap();
        let outcome = h
            .stop_within(StopReason::Graceful, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(outcome, StopOutcome::Graceful);
    });
}

#[test]
fn a_vetoing_actor_is_killed_after_the_window() {
    common::run_paused(async {
        let asked = Arc::new(AtomicUsize::new(0));
        let h = Veto {
            asked: asked.clone(),
        }
        .spawn()
        .await
        .unwrap();
        let outcome = h
            .stop_within(StopReason::Graceful, Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(outcome, StopOutcome::Killed);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    });
}

/// A turn that ends inside the kill grace, after the ladder's `Kill`, reads
/// `Killed`.
#[test]
fn a_slow_turn_that_ends_inside_the_grace_reads_killed() {
    common::run_paused(async {
        let gate = Arc::new(Notify::new());
        let (slow, entered) = Parked::until(gate.clone());
        let h = slow
            .spawn()
            .kill_grace(Duration::from_secs(5))
            .await
            .unwrap();
        park(&h, entered).await;
        let call = tokio::spawn({
            let h = h.clone();
            async move {
                h.stop_within(StopReason::Graceful, Duration::from_millis(50))
                    .await
            }
        });
        sleep(Duration::from_millis(200)).await;
        gate.notify_one();
        let outcome = call.await.unwrap().unwrap();
        assert_eq!(outcome, StopOutcome::Killed);
    });
}

#[test]
fn a_stuck_actor_is_aborted() {
    common::run_paused(async {
        let (stuck, entered) = Parked::forever();
        let h = stuck
            .spawn()
            .kill_grace(Duration::from_millis(300))
            .await
            .unwrap();
        park(&h, entered).await;
        let started = tokio::time::Instant::now();
        let outcome = h
            .stop_within(StopReason::Graceful, Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(outcome, StopOutcome::Aborted);
        assert_eq!(started.elapsed(), Duration::from_millis(350));
    });
}

#[test]
fn a_zero_kill_grace_aborts_at_once() {
    common::run_paused(async {
        let (stuck, entered) = Parked::forever();
        let h = stuck.spawn().kill_grace(Duration::ZERO).await.unwrap();
        park(&h, entered).await;
        let outcome = h
            .stop_within(StopReason::Graceful, Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(outcome, StopOutcome::Aborted);
    });
}

#[test]
fn an_actor_that_already_ended_refuses_the_stop() {
    common::run_paused(async {
        let h = Idle.spawn().await.unwrap();
        h.stop(StopReason::Graceful).await.unwrap();
        h.wait_stopped().await;
        let refused = h
            .stop_within(StopReason::Graceful, Duration::from_secs(1))
            .await;
        assert!(matches!(refused, Err(SendError::Closed)), "{refused:?}");
    });
}

/// A `Kill` raised by a third party before the window passes ends the actor
/// at its next turn boundary: the call saw a terminal status before its own
/// `Kill` step.
#[test]
fn a_third_party_kill_before_the_deadline_reads_graceful() {
    common::run_paused(async {
        let gate = Arc::new(Notify::new());
        let (slow, entered) = Parked::until(gate.clone());
        let h = slow.spawn().await.unwrap();
        park(&h, entered).await;
        let call = tokio::spawn({
            let h = h.clone();
            async move {
                h.stop_within(StopReason::Graceful, Duration::from_secs(5))
                    .await
            }
        });
        sleep(Duration::from_millis(10)).await;
        h.stop(StopReason::Kill).await.unwrap();
        gate.notify_one();
        let outcome = call.await.unwrap().unwrap();
        assert_eq!(outcome, StopOutcome::Graceful);
    });
}

/// The same third-party `Kill` on an actor that never reaches a turn
/// boundary: the abort the `Kill` owes ends it, and the call reads `Aborted`.
#[test]
fn a_third_party_kill_that_is_backed_by_the_abort_reads_aborted() {
    common::run_paused(async {
        let (stuck, entered) = Parked::forever();
        let h = stuck
            .spawn()
            .kill_grace(Duration::from_millis(200))
            .await
            .unwrap();
        park(&h, entered).await;
        let call = tokio::spawn({
            let h = h.clone();
            async move {
                h.stop_within(StopReason::Graceful, Duration::from_secs(5))
                    .await
            }
        });
        sleep(Duration::from_millis(10)).await;
        h.stop(StopReason::Kill).await.unwrap();
        let outcome = call.await.unwrap().unwrap();
        assert_eq!(outcome, StopOutcome::Aborted);
    });
}

#[test]
fn a_kill_call_reads_killed_or_aborted() {
    common::run_paused(async {
        let h = Idle.spawn().await.unwrap();
        let outcome = h
            .stop_within(StopReason::Kill, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(outcome, StopOutcome::Killed);

        let (stuck, entered) = Parked::forever();
        let h = stuck.spawn().await.unwrap();
        park(&h, entered).await;
        let outcome = h
            .stop_within(StopReason::Kill, Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(outcome, StopOutcome::Aborted);
    });
}

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

/// Spawns `n` actors, abandons a `stop_within(Graceful, 1 h)` on each, and
/// waits until they have all ended.
async fn abandon_on_idle_actors(sys: &Arc<ActorSystem>, n: usize) {
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let h = Idle.spawn().on_system(sys).await.unwrap();
        let mut call = Box::pin(h.stop_within(StopReason::Graceful, Duration::from_secs(3600)));
        poll_once(&mut call).await;
        drop(call);
        handles.push(h);
    }
    for h in &handles {
        h.wait_stopped().await;
    }
}

/// What a finished stop may leave behind, per actor, in bytes. The unit is
/// the allocator's own count of the thread that drives the runtime.
const RETAINED_CEILING: isize = 64;

/// What a pending call may hold, per call, in bytes: the one system task it
/// keeps and its bookkeeping. The unit is the allocator's own count of the
/// thread that drives the runtime, read while the calls are pending.
const PENDING_CEILING: isize = 2048;

/// The call holds one system task while its ladder or its target is pending,
/// at most `PENDING_CEILING` bytes, and gives all of it back when the target
/// ends.
#[test]
fn bytes_per_abandoned_stop_within() {
    let measured = std::thread::spawn(|| {
        MEASURED.with(|on| on.set(true));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let sys = ActorSystem::create("ladder-owned-memory").unwrap();
            // The registry's tables keep the capacity their peak load needed,
            // so the warm-up reaches a peak above the measured load's: what is
            // measured below is what a finished stop leaves behind, not the
            // growth of a table.
            abandon_on_idle_actors(&sys, 4000).await;
            sleep(Duration::from_millis(200)).await;
            let before = live_bytes();
            abandon_on_idle_actors(&sys, 1000).await;
            sleep(Duration::from_millis(300)).await;
            let retained = (live_bytes() - before) / 1000;

            // The cost while the calls are pending: 1000 actors that veto
            // their stop, each with an abandoned call, against the same
            // actors without one.
            let asked = Arc::new(AtomicUsize::new(0));
            let mut handles = Vec::new();
            for _ in 0..1000 {
                handles.push(
                    Veto {
                        asked: asked.clone(),
                    }
                    .spawn()
                    .on_system(&sys)
                    .await
                    .unwrap(),
                );
            }
            sleep(Duration::from_millis(100)).await;
            let without = live_bytes();
            for h in &handles {
                let mut call =
                    Box::pin(h.stop_within(StopReason::Graceful, Duration::from_secs(3600)));
                poll_once(&mut call).await;
                drop(call);
            }
            sleep(Duration::from_millis(100)).await;
            let pending = (live_bytes() - without) / 1000;
            for h in &handles {
                let _ = h.stop(StopReason::Kill).await;
            }
            for h in &handles {
                h.wait_stopped().await;
            }
            (retained, pending)
        })
    })
    .join()
    .unwrap();
    println!(
        "retained per ended actor: {} bytes, held per pending call: {} bytes",
        measured.0, measured.1
    );
    assert!(
        measured.0 <= RETAINED_CEILING,
        "an ended actor kept {} bytes",
        measured.0
    );
    assert!(
        measured.1 <= PENDING_CEILING,
        "a pending call held {} bytes",
        measured.1
    );
    assert!(
        measured.1 > 0,
        "the measurement saw the task of a pending call"
    );
}

// ---------------------------------------------------------------------------
// A shutdown still finishes with calls in flight
// ---------------------------------------------------------------------------

#[test]
fn a_shutdown_with_an_abandoned_bounded_stop_in_flight_finishes() {
    common::run(2, async {
        let sys = ActorSystem::create("ladder-owned-shutdown").unwrap();
        let asked = Arc::new(AtomicUsize::new(0));
        let h = Veto { asked }
            .spawn()
            .named("veto")
            .on_system(&sys)
            .await
            .unwrap();
        let mut call = Box::pin(h.stop_within(StopReason::Graceful, Duration::from_secs(3600)));
        poll_once(&mut call).await;
        drop(call);
        let report = timeout(
            Duration::from_secs(10),
            sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(5),
                per_actor_timeout: Duration::from_millis(150),
            }),
        )
        .await
        .expect("the shutdown finishes");
        assert!(report.outcomes.len() <= 1);
        assert_eq!(h.status(), ActorStatus::Stopped);
    });
}
