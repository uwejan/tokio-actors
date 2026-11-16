#![allow(dead_code)]

use std::cell::RefCell;
use std::future::Future;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

pub mod seeded;

/// The change signal of one test: woken at every change of the state the
/// test waits on. Each runner below makes a new one for the test it runs and
/// installs it on every thread of that test's runtime (the thread that runs
/// the body, every worker and every blocking thread), so a test announces and
/// waits through its own signal only and never through another test's.
#[derive(Default)]
pub struct Changes {
    notify: tokio::sync::Notify,
    generation: Mutex<u64>,
    advanced: Condvar,
}

impl Changes {
    /// Announces a change to every waiter of this test, async or blocked.
    pub fn ping(&self) {
        *self
            .generation
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += 1;
        self.advanced.notify_all();
        self.notify.notify_waiters();
    }

    /// The next announced change. A waiter enables it before it checks its
    /// condition, so a change between the check and the wait is not lost.
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    /// Holds the calling thread while `held` reads true, for at most `cap`,
    /// checked again at every announced change.
    pub fn hold_thread_while(&self, cap: Duration, held: impl Fn() -> bool) {
        let generation = self
            .generation
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _ = self
            .advanced
            .wait_timeout_while(generation, cap, |_| held());
    }
}

thread_local! {
    static CHANGES: RefCell<Option<Arc<Changes>>> = const { RefCell::new(None) };
}

/// The change signal of the test whose runtime runs the calling thread.
///
/// # Panics
///
/// On a thread that no runner of this module started for a test.
pub fn changes() -> Arc<Changes> {
    CHANGES
        .with(|slot| slot.borrow().clone())
        .expect("a change signal is read on a thread of the test's runtime")
}

/// Announces a change through the calling thread's test signal. A thread
/// whose local storage is already torn down is ending, and no waiter waits
/// on it.
///
/// # Panics
///
/// On a thread that no runner of this module started for a test.
pub fn ping() {
    if let Ok(changes) = CHANGES.try_with(|slot| slot.borrow().clone()) {
        changes
            .expect("a change is announced on a thread of the test's runtime")
            .ping();
    }
}

/// [`ping`] for a hook every thread may run, such as a process-wide log
/// subscriber: it announces through the calling thread's test signal and does
/// nothing on a thread that carries none, where no test waits.
pub fn ping_if_installed() {
    if let Ok(Some(changes)) = CHANGES.try_with(|slot| slot.borrow().clone()) {
        changes.ping();
    }
}

/// Puts `changes` on the calling thread until the returned guard drops, which
/// puts back what the thread held before.
#[must_use]
pub fn install(changes: Arc<Changes>) -> Installed {
    Installed(CHANGES.with(|slot| slot.replace(Some(changes))))
}

/// Restores the thread's previous change signal when dropped.
pub struct Installed(Option<Arc<Changes>>);

impl Drop for Installed {
    fn drop(&mut self) {
        let previous = self.0.take();
        let _ = CHANGES.try_with(|slot| slot.replace(previous));
    }
}

/// A new change signal, installed on the calling thread and on every thread
/// `builder` will start.
fn new_changes(builder: &mut tokio::runtime::Builder) -> Installed {
    let changes = Arc::new(Changes::default());
    let shared = changes.clone();
    builder.on_thread_start(move || {
        CHANGES.with(|slot| *slot.borrow_mut() = Some(shared.clone()));
    });
    install(changes)
}

/// Runs `body` on a Tokio runtime whose flavor is chosen by the
/// `TOKIO_ACTORS_TEST_FLAVOR` environment variable: `current_thread`
/// selects a single-threaded runtime, anything else (or unset) selects a
/// multi-threaded runtime with `workers` worker threads (0 = Tokio's default).
pub fn run<F: Future<Output = ()>>(workers: usize, body: F) {
    let mut builder = flavor_builder(workers);
    let _changes = new_changes(&mut builder);
    let runtime = builder
        .enable_all()
        .build()
        .expect("failed to build the test runtime");
    runtime.block_on(body);
}

/// The builder of the flavor `TOKIO_ACTORS_TEST_FLAVOR` selects.
fn flavor_builder(workers: usize) -> tokio::runtime::Builder {
    if std::env::var("TOKIO_ACTORS_TEST_FLAVOR").as_deref() == Ok("current_thread") {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        if workers > 0 {
            builder.worker_threads(workers);
        }
        builder
    }
}

/// [`run`] for a test whose failure can leave a runtime thread stuck (a task
/// blocked on a lock a blocked thread holds): when `body` panics the runtime
/// is leaked instead of dropped, since dropping it waits for that thread, and
/// the panic reaches the harness at once. The leaked threads end with the
/// process.
pub fn run_or_leak<F: Future<Output = ()>>(workers: usize, body: F) {
    let mut builder = flavor_builder(workers);
    let _changes = new_changes(&mut builder);
    let runtime = builder
        .enable_all()
        .build()
        .expect("failed to build the test runtime");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(body)));
    if let Err(panic) = outcome {
        std::mem::forget(runtime);
        std::panic::resume_unwind(panic);
    }
}

/// Always multi-threaded, for a test that needs real parallelism.
pub fn run_multi_thread_only<F: Future<Output = ()>>(workers: usize, body: F) {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if workers > 0 {
        builder.worker_threads(workers);
    }
    let _changes = new_changes(&mut builder);
    let runtime = builder
        .enable_all()
        .build()
        .expect("failed to build multi-thread test runtime");
    runtime.block_on(body);
}

/// Always a current-thread runtime, for a test whose shape needs two events
/// to land in one supervisor turn: with a single thread, nothing else runs
/// between two statements that do not await.
pub fn run_current_thread_only<F: Future<Output = ()>>(body: F) {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    let _changes = new_changes(&mut builder);
    let runtime = builder
        .enable_all()
        .build()
        .expect("failed to build current-thread test runtime");
    runtime.block_on(body);
}

/// Always a current-thread runtime on a paused clock, for a test that pins a
/// tight upper bound on elapsed time: with the clock paused a timed outcome
/// lands on its exact virtual instant, which no stalled host can move. The
/// body must not block a thread or read the wall clock.
pub fn run_paused<F: Future<Output = ()>>(body: F) {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    let _changes = new_changes(&mut builder);
    let runtime = builder
        .enable_all()
        .start_paused(true)
        .build()
        .expect("failed to build a paused current-thread test runtime");
    runtime.block_on(body);
}

/// The environment variable that marks a child process of a test binary.
const CHILD_PROCESS: &str = "TOKIO_ACTORS_TEST_CHILD";

/// Runs the test `name` (its full path inside its test binary) alone in a
/// child process of this test binary, and asserts the child exited
/// successfully. Returns `true` in the child, where the test runs its
/// scenario and its assertions, and `false` in the parent once the child
/// passed. A scenario whose failure could abort the process runs this way,
/// so the abort fails one test instead of ending the binary.
pub fn in_a_child_process(name: &str) -> bool {
    if std::env::var_os(CHILD_PROCESS).is_some() {
        return true;
    }
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads", "1"])
        .env(CHILD_PROCESS, "1")
        .output()
        .expect("the child process runs");
    let stdout = String::from_utf8_lossy(&child.stdout);
    assert!(
        child.status.success(),
        "the scenario failed or ended the process: {}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(
        stdout.contains("running 1 test") && stdout.contains("1 passed"),
        "the child ran exactly the one test: {stdout}"
    );
    false
}
