#![allow(dead_code)]
//! The entry check of `on_runtime`: a runtime that has been shut down is a
//! valid placement and runs no panic hook, a runtime built without the time
//! driver panics before the factory runs, and the refusals of a spawn are
//! answered before the check.
//!
//! The panic hook is process-wide, so every test here holds one lock while
//! its counting hook is installed.

mod common;

use std::future::{Future, IntoFuture};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::runtime::{Builder, Handle, Runtime};
use tokio_actors::{
    actor::context::ActorContext, Actor, ActorExt, ActorHandle, ActorResult, ActorSystem,
    ChildEvent, SpawnError, StopReason, SupervisionAction, SupervisionConfig,
};

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

fn unique(base: &str) -> String {
    format!(
        "entry-{base}-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

// ---------------------------------------------------------------------------
// The counting panic hook
// ---------------------------------------------------------------------------

static HOOK_LOCK: Mutex<()> = Mutex::new(());
static HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);

type Hook = dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static;

/// Replaces the panic hook with one that counts its calls and forwards to the
/// previous hook, and puts the previous hook back when it is dropped. Holds
/// the lock for that whole time, so the tests of this binary take turns.
struct CountingHook {
    previous: Arc<Box<Hook>>,
    _lock: MutexGuard<'static, ()>,
}

impl CountingHook {
    fn install() -> Self {
        let lock = HOOK_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous: Arc<Box<Hook>> = Arc::new(panic::take_hook());
        HOOK_CALLS.store(0, Ordering::SeqCst);
        let forward = previous.clone();
        panic::set_hook(Box::new(move |info| {
            HOOK_CALLS.fetch_add(1, Ordering::SeqCst);
            (**forward)(info);
        }));
        Self {
            previous,
            _lock: lock,
        }
    }

    fn calls(&self) -> usize {
        HOOK_CALLS.load(Ordering::SeqCst)
    }
}

impl Drop for CountingHook {
    fn drop(&mut self) {
        // The hook cannot be changed from a thread that is panicking: a test
        // that failed leaves the counting hook in place, which forwards.
        if std::thread::panicking() {
            return;
        }
        let _ = panic::take_hook();
        let previous = self.previous.clone();
        panic::set_hook(Box::new(move |info| (**previous)(info)));
    }
}

// ---------------------------------------------------------------------------
// Runtimes
// ---------------------------------------------------------------------------

fn with_driver() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

fn current_thread_with_driver() -> Runtime {
    Builder::new_current_thread().enable_all().build().unwrap()
}

fn without_driver() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap()
}

/// Ends a runtime from a thread that is not inside one.
fn end_runtime(runtime: Runtime) {
    std::thread::spawn(move || drop(runtime)).join().unwrap();
}

/// The handle of a runtime that has been dropped.
fn ended(runtime: Runtime) -> Handle {
    let handle = runtime.handle().clone();
    end_runtime(runtime);
    handle
}

/// Polls `condition` until it holds. The bound only limits how long a real hang takes to report.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + HANG_BOUND;
    while !condition() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// Actors
// ---------------------------------------------------------------------------

struct Worker;

impl Actor for Worker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// What a spawn is told to do.
#[derive(Clone)]
struct Spec {
    name: String,
    runtime: Option<Handle>,
    /// Counts every call of the factory.
    built: Arc<AtomicUsize>,
}

impl Spec {
    fn new(base: &str, runtime: Option<Handle>) -> Self {
        Self {
            name: unique(base),
            runtime,
            built: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// The outcome of one poll of a spawn.
struct Attempt {
    /// The text of the panic the poll raised.
    panic: Option<String>,
    /// The answer, when the spawn finished in that poll.
    answer: Option<Result<(), SpawnError>>,
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .unwrap_or_default(),
    }
}

/// Polls the spawn of `spec` once, with a waker that does nothing, and
/// catches a panic.
fn attempt<A: Actor>(ctx: &mut ActorContext<A>, spec: &Spec) -> Attempt {
    let built = spec.built.clone();
    let mut builder = ctx
        .spawn_child(move || {
            built.fetch_add(1, Ordering::SeqCst);
            Worker
        })
        .named(spec.name.clone());
    if let Some(runtime) = &spec.runtime {
        builder = builder.on_runtime(runtime);
    }
    let caught = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut future = builder.into_future();
        let mut cx = Context::from_waker(Waker::noop());
        match pin!(&mut future).poll(&mut cx) {
            Poll::Ready(answer) => Some(answer.map(|_: ActorHandle<Worker>| ())),
            Poll::Pending => None,
        }
    }));
    match caught {
        Ok(answer) => Attempt {
            panic: None,
            answer,
        },
        Err(payload) => Attempt {
            panic: Some(panic_text(payload)),
            answer: None,
        },
    }
}

enum Cmd {
    /// Awaits the spawn.
    Spawn(Spec),
    /// Awaits the spawn in the poll that has spent the task's budget.
    SpawnSpent(Spec),
    /// Crosses the barrier, then awaits the spawn.
    SpawnAfter(Spec, Arc<std::sync::Barrier>),
    /// Polls the spawn once.
    Attempt(Spec),
    /// Holds the turn open until the receiver answers.
    Hold(tokio::sync::oneshot::Receiver<()>),
}

enum Reply {
    /// The answer of the spawn, and the panic hook calls it made.
    Spawned(Result<(), SpawnError>, usize),
    Attempted(Attempt),
    Held,
}

/// What the supervisor does with a pending death at its stop.
#[derive(Default)]
struct Delivery {
    /// The spawn to try from a final delivery.
    spec: Mutex<Option<Spec>>,
    /// Its outcome.
    outcome: Mutex<Option<Attempt>>,
    holding: AtomicBool,
}

struct Supervisor {
    delivery: Arc<Delivery>,
}

impl Actor for Supervisor {
    type Message = Cmd;
    type Response = Reply;

    async fn handle(&mut self, cmd: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        Ok(match cmd {
            Cmd::Spawn(spec) => spawn_counted(ctx, spec).await,
            Cmd::SpawnSpent(spec) => {
                spend_budget().await;
                spawn_counted(ctx, spec).await
            }
            Cmd::SpawnAfter(spec, gate) => {
                gate.wait();
                spawn_counted(ctx, spec).await
            }
            Cmd::Attempt(spec) => Reply::Attempted(attempt(ctx, &spec)),
            Cmd::Hold(release) => {
                self.delivery.holding.store(true, Ordering::SeqCst);
                let _ = release.await;
                Reply::Held
            }
        })
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if event.action == SupervisionAction::SupervisorStopping {
            let spec = self.delivery.spec.lock().unwrap().clone();
            if let Some(spec) = spec {
                let outcome = attempt(ctx, &spec);
                *self.delivery.outcome.lock().unwrap() = Some(outcome);
            }
        }
        Ok(())
    }
}

/// Awaits the spawn of `spec` and counts the panic hook calls it made.
async fn spawn_counted(ctx: &mut ActorContext<Supervisor>, spec: Spec) -> Reply {
    let before = HOOK_CALLS.load(Ordering::SeqCst);
    let built = spec.built.clone();
    let mut builder = ctx
        .spawn_child(move || {
            built.fetch_add(1, Ordering::SeqCst);
            Worker
        })
        .named(spec.name.clone());
    if let Some(runtime) = &spec.runtime {
        builder = builder.on_runtime(runtime);
    }
    let answer = builder.await.map(|_: ActorHandle<Worker>| ());
    Reply::Spawned(answer, HOOK_CALLS.load(Ordering::SeqCst) - before)
}

/// Spends the cooperative budget of the current task: reads a channel that
/// holds more values than the budget until Tokio answers `Pending` although a
/// value is waiting, and stays in the same poll, so the budget is still spent
/// when the caller goes on.
async fn spend_budget() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u8>();
    for _ in 0..1000 {
        tx.send(0).unwrap();
    }
    std::future::poll_fn(|cx| {
        while rx.poll_recv(cx).is_ready() {}
        Poll::Ready(())
    })
    .await;
    // One more value is waiting: a read that is held back now is held back by
    // the budget, not by an empty channel.
    tx.send(0).unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        rx.poll_recv(&mut cx).is_pending(),
        "the budget of the task is spent while a value waits"
    );
}

async fn supervisor(delivery: &Arc<Delivery>) -> ActorHandle<Supervisor> {
    Supervisor {
        delivery: delivery.clone(),
    }
    .spawn()
    .named(unique("supervisor"))
    .with_supervision(SupervisionConfig::one_for_one())
    .await
    .unwrap()
}

async fn spawn_via(sup: &ActorHandle<Supervisor>, spec: &Spec) -> (Result<(), SpawnError>, usize) {
    match sup.send(Cmd::Spawn(spec.clone())).await.unwrap() {
        Reply::Spawned(answer, hooks) => (answer, hooks),
        _ => unreachable!(),
    }
}

async fn attempt_via(sup: &ActorHandle<Supervisor>, spec: &Spec) -> Attempt {
    match sup.send(Cmd::Attempt(spec.clone())).await.unwrap() {
        Reply::Attempted(attempt) => attempt,
        _ => unreachable!(),
    }
}

async fn finish(sup: ActorHandle<Supervisor>) {
    let _ = sup.stop(StopReason::Graceful).await;
    tokio::time::timeout(HANG_BOUND, sup.wait_stopped())
        .await
        .expect("the supervisor ends");
}

// ---------------------------------------------------------------------------
// A runtime that has been shut down
// ---------------------------------------------------------------------------

#[test]
fn a_spawn_on_a_shut_down_runtime_returns_ok_and_runs_no_panic_hook() {
    let hook = CountingHook::install();
    let placements = [
        ended(with_driver()),
        ended(current_thread_with_driver()),
        ended(without_driver()),
    ];
    common::run(2, async {
        for placed in &placements {
            // One supervisor for each: the failed restarts of a child end it.
            let sup = supervisor(&Default::default()).await;
            let spec = Spec::new("ended", Some(placed.clone()));
            let (answer, hooks) = spawn_via(&sup, &spec).await;
            assert_eq!(answer, Ok(()), "the call is not refused");
            assert_eq!(hooks, 0, "no panic hook ran for the spawn");
            finish(sup).await;
        }
    });
    assert_eq!(hook.calls(), 0, "no panic hook ran at all");
}

#[test]
fn a_spawn_on_a_shut_down_runtime_is_found_when_the_caller_has_spent_its_budget() {
    let hook = CountingHook::install();
    // Without the driver the creation of a sleep panics, so a runtime that
    // is not found shut down is taken for one that never had the driver.
    let placements = [ended(without_driver()), ended(with_driver())];
    common::run(2, async {
        for placed in &placements {
            let sup = supervisor(&Default::default()).await;
            let spec = Spec::new("spent", Some(placed.clone()));
            let reply = sup.send(Cmd::SpawnSpent(spec)).await.unwrap();
            let Reply::Spawned(answer, hooks) = reply else {
                unreachable!()
            };
            assert_eq!(answer, Ok(()), "the call is not refused");
            assert_eq!(hooks, 0, "no panic hook ran for the spawn");
            finish(sup).await;
        }
    });
    assert_eq!(hook.calls(), 0);
}

// ---------------------------------------------------------------------------
// A runtime without the time driver
// ---------------------------------------------------------------------------

#[test]
fn a_runtime_without_the_driver_panics_before_the_factory_runs() {
    let hook = CountingHook::install();
    let bare = without_driver();
    let bare_handle = bare.handle().clone();
    common::run(2, async move {
        let sup = supervisor(&Default::default()).await;
        let spec = Spec::new("bare", Some(bare_handle));
        let attempted = attempt_via(&sup, &spec).await;
        let text = attempted.panic.expect("the call panics");
        assert!(text.contains("enable_time"), "{text}");
        assert!(text.contains("ChildSpawnBuilder::on_runtime"), "{text}");
        assert_eq!(spec.built.load(Ordering::SeqCst), 0, "no factory call");
        assert!(ActorSystem::default().get::<Worker>(&spec.name).is_none());
        finish(sup).await;
    });
    assert!(hook.calls() >= 1, "the probe's panic reached the hook");
    end_runtime(bare);
}

// ---------------------------------------------------------------------------
// The refusals come first
// ---------------------------------------------------------------------------

#[test]
fn a_spawn_under_a_taken_name_is_refused_before_the_entry_check() {
    let _hook = CountingHook::install();
    let bare = without_driver();
    let good = with_driver();
    let bare_handle = bare.handle().clone();
    let good_handle = good.handle().clone();
    common::run(2, async move {
        let sup = supervisor(&Default::default()).await;
        let first = Spec::new("taken", Some(good_handle));
        let (answer, _) = spawn_via(&sup, &first).await;
        assert_eq!(answer, Ok(()));
        until("the first child", || {
            ActorSystem::default().get::<Worker>(&first.name).is_some()
        })
        .await;

        let again = Spec {
            runtime: Some(bare_handle.clone()),
            ..first.clone()
        };
        let attempted = attempt_via(&sup, &again).await;
        assert_eq!(attempted.panic, None, "the refusal comes first");
        assert_eq!(
            attempted.answer,
            Some(Err(SpawnError::DuplicateChild(first.name.as_str().into())))
        );
        assert_eq!(
            first.built.load(Ordering::SeqCst),
            1,
            "no second factory call"
        );

        // The check is still live for a name nobody holds.
        let fresh = Spec::new("fresh", Some(bare_handle));
        let attempted = attempt_via(&sup, &fresh).await;
        let text = attempted.panic.expect("the call panics");
        assert!(text.contains("enable_time"), "{text}");
        assert_eq!(fresh.built.load(Ordering::SeqCst), 0);
        finish(sup).await;
    });
    end_runtime(bare);
    end_runtime(good);
}
