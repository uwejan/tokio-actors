//! A spawn with a `start_timeout`: how long the caller waits after the system
//! aborts a stuck initialization, what keeps the name while an initialization
//! never yields, what the call needs from the runtime it is polled on, and
//! what a caller polled after its runtime ended is answered with.

use std::future::{Future, IntoFuture};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorSystem, SpawnError, StopReason,
};

mod common;

/// Long enough that a real hang fails the test, never a condition's wait.
const BOUND: Duration = Duration::from_secs(30);

/// The longest a call may take beyond its own windows before the test reads a
/// stalled caller.
const SLACK: Duration = Duration::from_secs(5);

/// The time the system gives an aborted task to end before it answers the
/// caller.
const ABORT_SETTLE: Duration = Duration::from_millis(100);

fn unique(base: &str) -> String {
    format!("{base}-{}", uuid::Uuid::new_v4())
}

/// A std gate that opens on drop too, so a failed assertion never leaves a
/// worker thread blocked.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *(self.0).0.lock().unwrap() = true;
        (self.0).1.notify_all();
    }

    fn wait(&self) {
        let (open, changed) = &*self.0;
        let mut open = open.lock().unwrap();
        while !*open {
            open = changed.wait(open).unwrap();
        }
    }
}

struct OpenOnDrop(Gate);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// An actor whose `pre_start` holds its worker thread until the gate opens,
/// without ever yielding to the runtime.
struct Blocks {
    entered: mpsc::Sender<()>,
    gate: Gate,
}

impl Actor for Blocks {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _ = self.entered.send(());
        self.gate.wait();
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// An actor that starts at once.
struct Quick;

impl Actor for Quick {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

type Spawned = Pin<Box<dyn Future<Output = Result<ActorHandle<Quick>, SpawnError>> + Send>>;

fn poll_once(call: &mut Spawned) -> Poll<Result<ActorHandle<Quick>, SpawnError>> {
    call.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

// ---------------------------------------------------------------------------
// The abort within the settle time; the name while an initialization never
// yields
// ---------------------------------------------------------------------------

/// An initialization that never yields is beyond the abort: the caller is
/// told `StartTimeout` once the timeout and the settle time have passed, not
/// when the task ends, and the actor keeps its name until the hook returns,
/// when the `Kill` raised at the cut ends it before another hook begins.
#[test]
fn a_start_that_never_yields_keeps_its_name_after_the_caller_is_answered() {
    const START_TIMEOUT: Duration = Duration::from_millis(200);
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(unique("never-yields")).unwrap();
        let name = unique("blocks");
        let gate = Gate::default();
        let _opens = OpenOnDrop(gate.clone());
        let (entered, entered_rx) = mpsc::channel();

        let started = Instant::now();
        let result = timeout(
            START_TIMEOUT + ABORT_SETTLE + SLACK,
            Blocks {
                entered,
                gate: gate.clone(),
            }
            .spawn()
            .named(name.as_str())
            .on_system(&sys)
            .start_timeout(START_TIMEOUT)
            .into_future(),
        )
        .await
        .expect("the caller waited for the task to end instead of the settle time");
        let took = started.elapsed();

        assert!(
            matches!(result, Err(SpawnError::StartTimeout)),
            "{result:?}"
        );
        entered_rx
            .try_recv()
            .expect("the initialization was running");
        assert!(
            took >= START_TIMEOUT + ABORT_SETTLE,
            "the caller was answered before the settle time passed: {took:?}"
        );
        let actor = sys
            .get::<Blocks>(&name)
            .expect("the name stays taken while the initialization blocks");
        assert!(actor.is_alive());

        // The hook returns once the gate opens, and the actor ends on the
        // `Kill` raised at the cut.
        gate.open();
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends once the hook returns");
        assert_eq!(reason, StopReason::Kill);
        assert!(sys.get::<Blocks>(&name).is_none(), "the name is free");
    });
}

// ---------------------------------------------------------------------------
// A runtime that is entered but not driven
// ---------------------------------------------------------------------------

/// The system's task of a timed spawn runs on the caller's runtime, so the
/// call needs that runtime driven, not only entered: polled under a runtime
/// nothing drives it stays pending, and it completes once the runtime runs.
#[test]
fn a_timed_spawn_under_an_entered_undriven_runtime_completes_only_when_driven() {
    let rt = current_thread();
    let entered = rt.enter();
    let mut call: Spawned = Box::pin(
        Quick
            .spawn()
            .named(unique("undriven"))
            .start_timeout(Duration::from_secs(5))
            .into_future(),
    );
    assert!(
        poll_once(&mut call).is_pending(),
        "the system's task has not run"
    );
    assert!(
        poll_once(&mut call).is_pending(),
        "polling again drives nothing"
    );
    drop(entered);

    let handle = rt
        .block_on(async { timeout(BOUND, call).await })
        .expect("the runtime runs the system's task")
        .expect("the start is answered once the runtime is driven");
    assert!(handle.is_alive());
}

// ---------------------------------------------------------------------------
// A caller polled after its runtime ended
// ---------------------------------------------------------------------------

/// A caller whose runtime ended before the system answered is answered from
/// what the system decided: the actor ended with its runtime, so the spawn
/// reads `Killed`, and no timer of the ended runtime is used.
#[test]
fn a_caller_polled_after_its_runtime_ended_reads_the_end_of_the_actor() {
    let rt = current_thread();
    let ended = rt.handle().clone();
    let mut call: Spawned = Box::pin(
        Quick
            .spawn()
            .named(unique("ended-runtime"))
            .start_timeout(Duration::from_secs(5))
            .into_future(),
    );
    {
        let _entered = ended.enter();
        assert!(poll_once(&mut call).is_pending());
    }
    drop(rt);

    let _entered = ended.enter();
    let answer = catch_unwind(AssertUnwindSafe(|| poll_once(&mut call)))
        .expect("the answer needs no timer of the ended runtime");
    assert!(
        matches!(answer, Poll::Ready(Err(SpawnError::Killed))),
        "{:?}",
        answer.map(|r| r.map(|_| ()))
    );
}

/// When the end of the actor is not yet final, the caller waits on the timers
/// of the runtime that polls it, up to the settle time, and reads
/// `StartTimeout`.
#[test]
fn a_caller_whose_actor_has_not_ended_waits_on_the_timers_of_the_runtime_that_polls_it() {
    const START_TIMEOUT: Duration = Duration::from_millis(400);
    let actor_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let ended = actor_rt.handle().clone();
    let gate = Gate::default();
    let _opens = OpenOnDrop(gate.clone());
    let (entered, entered_rx) = mpsc::channel();
    let sys_name = unique("not-final");
    let mut call: std::pin::Pin<
        Box<dyn Future<Output = Result<ActorHandle<Blocks>, SpawnError>> + Send>,
    > = Box::pin(
        Blocks {
            entered,
            gate: gate.clone(),
        }
        .spawn()
        .named(sys_name)
        .start_timeout(START_TIMEOUT)
        .into_future(),
    );
    {
        let _entered = ended.enter();
        let polled = call.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(polled.is_pending());
    }
    entered_rx
        .recv_timeout(BOUND)
        .expect("the initialization blocks a worker thread");
    // The runtime ends while the blocked task still runs: the system's task
    // is dropped undecided and the actor has no final reason yet.
    actor_rt.shutdown_background();

    let other = current_thread();
    let answer = other
        .block_on(async { timeout(BOUND, call).await })
        .expect("the wait ends at the settle time");
    assert!(
        matches!(answer, Err(SpawnError::StartTimeout)),
        "{answer:?}"
    );
}
