//! Runs a test on a runtime and a thread of its own and fails it when it has
//! not finished, runtime shutdown included, within a limit. A supervisor that
//! loops without ever yielding keeps its worker, so a test body on the same
//! thread is never polled again, and a runtime with a task that never yields
//! cannot be dropped: a timeout inside the test cannot fire, and the test
//! hangs. The limit is read on a thread of the test harness, which is not
//! driven by that runtime.

use std::future::Future;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tokio::runtime::Builder;

/// The longest a test and the shutdown of its runtime may take. It only limits
/// how long a real hang takes to report.
const LIMIT: Duration = Duration::from_secs(60);

/// The runtime a test runs on.
#[derive(Clone, Copy)]
pub(crate) enum Flavor {
    CurrentThread,
    MultiThread,
}

/// Runs the future `test` builds on a runtime of `flavor`, on a thread of its
/// own. Fails when the future panics, or when the future and the drop of its
/// runtime together take longer than [`LIMIT`].
pub(crate) fn run<F, Fut>(flavor: Flavor, test: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        let mut builder = match flavor {
            Flavor::CurrentThread => Builder::new_current_thread(),
            Flavor::MultiThread => {
                let mut builder = Builder::new_multi_thread();
                builder.worker_threads(2);
                builder
            }
        };
        let runtime = builder.enable_all().build().expect("a runtime");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(test());
        }));
        drop(runtime);
        let _ = done.send(outcome);
    });
    match finished.recv_timeout(LIMIT) {
        Ok(Ok(())) => {}
        Ok(Err(payload)) => std::panic::resume_unwind(payload),
        Err(_) => panic!("the test and its runtime did not finish within {LIMIT:?}"),
    }
}
