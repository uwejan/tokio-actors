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
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, SpawnError, StopReason,
};

const BOUND: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_millis(150);

/// The start timeout of the row whose hook waits at an await: long enough
/// that the hook has begun when the cut lands.
const YIELDS_START_TIMEOUT: Duration = Duration::from_secs(2);

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

struct OpenOnDrop(Gate);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
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

/// The start hook that holds its worker thread.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hook {
    PreStart,
    OnStarted,
}

/// One start hook holds its worker thread until the gate opens and never
/// yields.
struct Blocks {
    trace: Arc<Trace>,
    blocks: Hook,
    entered: mpsc::Sender<()>,
    gate: Gate,
}

impl Blocks {
    fn block_in(&self, hook: Hook) {
        if self.blocks == hook {
            let _ = self.entered.send(());
            self.gate.wait();
        }
    }
}

impl Actor for Blocks {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.block_in(Hook::PreStart);
        self.trace.pre_start_finished.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.trace.on_started_ran.store(true, Ordering::SeqCst);
        self.block_in(Hook::OnStarted);
        self.trace.on_started_finished.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.trace.handled.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.trace.on_stopped_ran.store(true, Ordering::SeqCst);
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
            .start_timeout(YIELDS_START_TIMEOUT)
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

/// Starts `Blocks` with a start timeout that it outruns, queues a message
/// through a handle from the system while the hook still holds its thread,
/// lets the hook finish after the caller was answered and waits for the actor
/// to end.
fn outrun_start_by_blocking(blocks: Hook) -> (Arc<Trace>, StopReason) {
    let mut out = None;
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(unique("start-timeout-blocks")).unwrap();
        let trace = Arc::new(Trace::default());
        let name = unique("blocks");
        let gate = Gate::default();
        let _opens = OpenOnDrop(gate.clone());
        let (entered, entered_rx) = mpsc::channel();

        let result = timeout(
            BOUND,
            Blocks {
                trace: trace.clone(),
                blocks,
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
        .expect("the caller is answered");
        assert!(
            matches!(result, Err(SpawnError::StartTimeout)),
            "{result:?}"
        );
        entered_rx
            .try_recv()
            .expect("the initialization was running");
        let hook_finished = match blocks {
            Hook::PreStart => &trace.pre_start_finished,
            Hook::OnStarted => &trace.on_started_finished,
        };
        assert!(
            !hook_finished.load(Ordering::SeqCst),
            "the hook still holds its thread when the caller is answered"
        );

        let actor = sys
            .get::<Blocks>(&name)
            .expect("the name stays taken while the hook blocks");
        actor
            .try_notify(())
            .expect("the mailbox takes a message while the hook blocks");
        gate.open();
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends once the hook returns");
        out = Some((trace, reason));
    });
    out.unwrap()
}
