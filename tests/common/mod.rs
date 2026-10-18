#![allow(dead_code)]

use std::future::Future;

/// Runs `body` on a Tokio runtime whose flavor is chosen by the
/// `TOKIO_ACTORS_TEST_FLAVOR` environment variable: `current_thread`
/// selects a single-threaded runtime, anything else (or unset) selects a
/// multi-threaded runtime with `workers` worker threads (0 = Tokio's default).
pub fn run<F: Future<Output = ()>>(workers: usize, body: F) {
    let runtime = if std::env::var("TOKIO_ACTORS_TEST_FLAVOR").as_deref() == Ok("current_thread") {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build current-thread test runtime")
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        if workers > 0 {
            builder.worker_threads(workers);
        }
        builder
            .enable_all()
            .build()
            .expect("failed to build multi-thread test runtime")
    };
    runtime.block_on(body);
}

/// Always multi-threaded, for a test that needs real parallelism.
pub fn run_multi_thread_only<F: Future<Output = ()>>(workers: usize, body: F) {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if workers > 0 {
        builder.worker_threads(workers);
    }
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
    let runtime = tokio::runtime::Builder::new_current_thread()
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
    let runtime = tokio::runtime::Builder::new_current_thread()
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
