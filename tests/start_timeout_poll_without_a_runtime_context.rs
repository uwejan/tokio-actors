//! A spawn with a `start_timeout` whose runtime ended before the start was
//! decided, polled by a thread that has no Tokio runtime context at all: the
//! wait for the actor's end uses a Tokio timer, so the poll panics as any
//! Tokio timer does, unless the start was acknowledged, which answers with the
//! handle and needs no timer.
//!
//! The runtimes here are current-thread runtimes that the test drives by
//! hand, so the order of the start and of the runtime's end is fixed by
//! observation; the flavor variable of the other suites does not apply.

use std::future::{Future, IntoFuture};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, SpawnError,
};

/// Long enough that a real hang fails the test, never a condition's wait.
const BOUND: Duration = Duration::from_secs(30);

fn unique(base: &str) -> String {
    format!("{base}-{}", uuid::Uuid::new_v4())
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

type Spawned = Pin<Box<dyn Future<Output = Result<ActorHandle<Quick>, SpawnError>> + Send>>;

/// A waker that records that it was woken.
#[derive(Default)]
struct WokenFlag(AtomicBool);

impl Wake for WokenFlag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn timed_spawn(name: &str) -> Spawned {
    Box::pin(
        Quick
            .spawn()
            .named(unique(name))
            .start_timeout(Duration::from_secs(5))
            .into_future(),
    )
}

/// Polls `call` once from a plain thread of its own, with no runtime entered
/// and a waker that does nothing; the panic of the poll, if any, comes back as
/// its message.
fn poll_without_a_runtime_context(
    mut call: Spawned,
) -> Result<Poll<Result<ActorHandle<Quick>, SpawnError>>, String> {
    std::thread::spawn(move || {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "the polling thread has a runtime context"
        );
        catch_unwind(AssertUnwindSafe(|| {
            call.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        }))
        .map_err(|payload| {
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|text| text.to_string()))
                .unwrap_or_else(|| "a panic that carries no text".to_string())
        })
    })
    .join()
    .expect("the polling thread ended without an outcome")
}

/// The premise: with no runtime context, creating a Tokio timer panics.
#[test]
fn a_tokio_timer_created_without_a_runtime_context_panics() {
    let outcome = std::thread::spawn(|| {
        catch_unwind(AssertUnwindSafe(|| {
            drop(sleep(Duration::from_millis(1)));
        }))
        .is_err()
    })
    .join()
    .unwrap();
    assert!(outcome, "a timer was created with no runtime context");
}

/// The runtime ended before the system decided the start: the system's task
/// was dropped with it, the answer channel is closed and the gate holds no
/// decision, so the caller must wait for the actor's end on a timer, and with
/// no runtime context the poll panics.
#[test]
fn a_poll_with_no_runtime_context_panics_when_the_start_was_not_decided() {
    let rt = current_thread();
    let entered_on = rt.handle().clone();
    let mut call = timed_spawn("undecided");
    {
        let _entered = entered_on.enter();
        let first = call.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(first.is_pending(), "the system has not answered yet");
    }
    // Nothing drove the runtime, so neither the actor nor the system's task
    // ever ran.
    drop(rt);

    let outcome = poll_without_a_runtime_context(call);
    let message = match outcome {
        Err(message) => message,
        Ok(polled) => panic!(
            "the poll answered with no runtime context: {:?}",
            polled.map(|answer| answer.map(|_| ()))
        ),
    };
    assert!(
        message.to_lowercase().contains("runtime"),
        "the panic does not name the missing runtime: {message}"
    );
}

/// The start was acknowledged and the answer is waiting: the poll hands the
/// handle back and uses no timer, so it answers with no runtime context, even
/// after the runtime ended.
#[test]
fn a_poll_with_no_runtime_context_answers_a_start_that_was_acknowledged() {
    let rt = current_thread();
    let flag = Arc::new(WokenFlag::default());
    let waker = Waker::from(flag.clone());
    let mut call = timed_spawn("acknowledged");
    {
        let _entered = rt.handle().enter();
        let first = call.as_mut().poll(&mut Context::from_waker(&waker));
        assert!(first.is_pending(), "the system has not answered yet");
    }
    // The system's task answers once the actor has acknowledged its start,
    // and the answer wakes the caller's waker.
    rt.block_on(async {
        timeout(BOUND, async {
            while !flag.0.load(Ordering::SeqCst) {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the start was never answered");
    });
    drop(rt);

    let polled = match poll_without_a_runtime_context(call) {
        Ok(polled) => polled,
        Err(message) => panic!("an acknowledged start panicked with no runtime context: {message}"),
    };
    assert!(
        matches!(polled, Poll::Ready(Ok(_))),
        "the poll did not hand the handle back: {:?}",
        polled.map(|answer| answer.map(|_| ()))
    );
}
