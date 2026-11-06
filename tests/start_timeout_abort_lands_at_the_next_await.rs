#![allow(dead_code)]
//! When a start outruns its `start_timeout` the caller reads `StartTimeout`
//! and the system cuts the start: it raises `Kill` on the actor's stop lane
//! and aborts the task. The abort takes effect when the task next suspends: a
//! hook that yields is cut there, so no later hook begins and a message queued
//! after the caller read `StartTimeout` is never handled. A hook that never
//! yields runs to its end first; the `Kill` is then read before another hook
//! begins or a message is handled, and the actor ends as `Kill`.

mod common;

use std::future::IntoFuture;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, SpawnError,
};

const BOUND: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_millis(150);

fn unique(base: &str) -> String {
    format!("{base}-{}", uuid::Uuid::new_v4())
}

/// What the actor's hooks and handler did.
#[derive(Default)]
struct Trace {
    pre_start_finished: AtomicBool,
    on_started_ran: AtomicBool,
    on_started_finished: AtomicBool,
    handled: AtomicBool,
    on_stopped_ran: AtomicBool,
    /// Signalled when the future of `pre_start` is dropped.
    pre_start_dropped: Notify,
}

/// Signals the trace when the hook's future is dropped, by abort or by end.
struct SignalsOnDrop(Arc<Trace>);

impl Drop for SignalsOnDrop {
    fn drop(&mut self) {
        self.0.pre_start_dropped.notify_one();
    }
}

/// A gate that opens on drop too, so a failed assertion never leaves a worker
/// thread blocked.
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

/// `pre_start` waits at an await that never completes.
struct Yields {
    trace: Arc<Trace>,
}

impl Actor for Yields {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _signal = SignalsOnDrop(self.trace.clone());
        std::future::pending::<()>().await;
        self.trace.pre_start_finished.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.trace.on_started_ran.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.trace.handled.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn an_initialization_that_yields_is_cut_at_its_await_and_no_later_hook_runs_and_a_later_message_is_never_handled(
) {
    common::run(2, async {
        let sys = ActorSystem::create(unique("start-timeout-yields")).unwrap();
        let trace = Arc::new(Trace::default());
        let name = unique("yields");

        let result = timeout(
            BOUND,
            Yields {
                trace: trace.clone(),
            }
            .spawn()
            .named(name.as_str())
            .on_system(&sys)
            .start_timeout(START_TIMEOUT)
            .into_future(),
        )
        .await
        .expect("the caller is answered");
        assert!(
            matches!(result, Err(SpawnError::StartTimeout)),
            "{result:?}"
        );

        // A message queued after the caller read `StartTimeout`, while the name
        // may still be registered.
        if let Some(actor) = sys.get::<Yields>(&name) {
            let _ = actor.try_notify(());
        }

        // The abort cut `pre_start` at its await: its future is dropped.
        timeout(BOUND, trace.pre_start_dropped.notified())
            .await
            .expect("the abort cuts the hook at its await");
        assert!(!trace.pre_start_finished.load(Ordering::SeqCst));
        assert!(
            !trace.on_started_ran.load(Ordering::SeqCst),
            "no hook begins after the abort"
        );
        assert!(
            !trace.handled.load(Ordering::SeqCst),
            "the actor handles nothing"
        );
    });
}
