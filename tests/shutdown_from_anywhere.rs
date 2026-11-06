//! A system shutdown that began runs to its end whoever calls it and however
//! the caller's future ends: awaited inside a root's own handler, dropped,
//! stored and never polled again, or driven with no Tokio runtime current.
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use tokio_actors::error::SpawnError;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, ActorSystem, ShutdownPolicy, SystemConfig,
};

mod common;

const BOUND: Duration = Duration::from_secs(20);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

fn uname(what: &str) -> String {
    format!("{what}-{}", uuid::Uuid::new_v4())
}

/// A system whose roots get `per_actor` each and `total` overall.
fn system_with(what: &str, total: Duration, per_actor: Duration) -> Arc<ActorSystem> {
    ActorSystem::create_with(
        uname(what),
        SystemConfig {
            shutdown_policy: ShutdownPolicy {
                timeout: total,
                per_actor_timeout: per_actor,
            },
            ..Default::default()
        },
    )
    .unwrap()
}

/// Waits until `condition` holds, yielding between looks, for at most
/// `HANG_BOUND`. The bound only limits how long a real hang takes to report.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let waited = timeout(HANG_BOUND, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "{what}");
}

/// Signals when its first message is being handled, then never returns from
/// the handler.
struct Stuck {
    entered: Option<oneshot::Sender<()>>,
}

impl Actor for Stuck {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// Does nothing.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Awaits a shutdown of its system from inside its own handler.
struct Caller {
    system: Arc<ActorSystem>,
    entered: Option<oneshot::Sender<()>>,
}

impl Actor for Caller {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        let _ = self.system.shutdown().await;
        Ok(())
    }
}

/// Spawns a `Stuck` root and brings it into its handler.
async fn stuck_root(
    sys: &Arc<ActorSystem>,
    name: &str,
) -> tokio_actors::actor::handle::ActorHandle<Stuck> {
    let (tx, rx) = oneshot::channel();
    let handle = Stuck { entered: Some(tx) }
        .spawn()
        .named(name)
        .on_system(sys)
        .await
        .unwrap();
    handle.notify(()).await.unwrap();
    timeout(BOUND, rx).await.unwrap().unwrap();
    handle
}

/// Whether the system has left the shutdown for good: `reactivate` only
/// succeeds from a completed shutdown with none in flight.
fn completed(sys: &ActorSystem) -> bool {
    sys.reactivate()
}

/// A shutdown awaited inside a root's own handler stops that root with the
/// rest, and the walk goes on to every root after it.
#[test]
fn a_shutdown_awaited_inside_a_root_handler_walks_every_root() {
    common::run(2, async {
        let sys = system_with(
            "from-inside",
            Duration::from_secs(10),
            Duration::from_millis(300),
        );
        // Registered first, so the walk reaches it after the caller.
        let stuck = stuck_root(&sys, "stuck").await;
        let (tx, rx) = oneshot::channel();
        let caller = Caller {
            system: sys.clone(),
            entered: Some(tx),
        }
        .spawn()
        .named("caller")
        .on_system(&sys)
        .await
        .unwrap();
        caller.notify(()).await.unwrap();
        timeout(BOUND, rx).await.unwrap().unwrap();

        timeout(BOUND, stuck.wait_stopped())
            .await
            .expect("the root after the caller is stopped by the walk");
        timeout(BOUND, caller.wait_stopped())
            .await
            .expect("the caller's root is stopped by the walk");
        assert_eq!(stuck.status(), ActorStatus::Stopped);
        assert_eq!(caller.status(), ActorStatus::Stopped);
        until("the walk completes and leaves the system defunct", || {
            completed(&sys)
        })
        .await;
    });
}

/// The caller stops waiting before the walk's `Kill` step; the walk still
/// reaches it.
#[test]
fn an_abandoned_shutdown_still_reaches_its_kill_step() {
    common::run(2, async {
        let sys = system_with(
            "abandoned",
            Duration::from_secs(5),
            Duration::from_millis(300),
        );
        let stuck = stuck_root(&sys, "stuck").await;
        // The caller gives up right after the call's first poll, before the
        // 300 ms Kill step.
        let mut abandoned = Box::pin(sys.shutdown());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(abandoned.as_mut().poll(&mut cx).is_pending());
        drop(abandoned);
        timeout(BOUND, stuck.wait_stopped())
            .await
            .expect("the walk the caller abandoned still stops the root");
        assert_eq!(stuck.status(), ActorStatus::Stopped);
        until("the abandoned walk completes", || completed(&sys)).await;
    });
}

/// A registration attempted right after one poll of the shutdown call is
/// refused: the shutdown is entered in the caller's first poll, before the
/// walk's task has had a turn.
#[test]
fn a_registration_right_after_one_poll_of_the_call_is_refused() {
    common::run_current_thread_only(async {
        let sys = system_with("first-poll", Duration::from_secs(5), Duration::from_secs(1));
        let root = Idle.spawn().named("root").on_system(&sys).await.unwrap();
        let mut shut = Box::pin(sys.shutdown());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(shut.as_mut().poll(&mut cx).is_pending());

        let mut late = Box::pin(Idle.spawn().named("late").on_system(&sys).into_future());
        match late.as_mut().poll(&mut cx) {
            Poll::Ready(Err(SpawnError::SystemShuttingDown(name))) => {
                assert_eq!(name, sys.name());
            }
            Poll::Ready(other) => {
                panic!("the registration must be refused, got {:?}", other.err())
            }
            Poll::Pending => panic!("the registration was accepted and is waiting for its ack"),
        }
        drop(late);
        let report = timeout(BOUND, shut).await.expect("the shutdown returns");
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(root.status(), ActorStatus::Stopped);
    });
}

/// Wakes the thread that parked in `block_on`.
struct ThreadWaker {
    woken: Mutex<bool>,
    signal: Condvar,
}

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        *self.woken.lock().unwrap() = true;
        self.signal.notify_one();
    }
}

/// Drives `fut` on the calling thread with no Tokio runtime current.
fn block_on_without_a_runtime<F: Future>(fut: F) -> F::Output {
    let wake = Arc::new(ThreadWaker {
        woken: Mutex::new(false),
        signal: Condvar::new(),
    });
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    let mut fut = Pin::from(Box::new(fut));
    loop {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return out;
        }
        let mut woken = wake.woken.lock().unwrap();
        while !*woken {
            woken = wake.signal.wait(woken).unwrap();
        }
        *woken = false;
    }
}

/// With no Tokio runtime current the walk runs in the caller's own future.
#[test]
fn a_shutdown_driven_with_no_runtime_current_walks_in_the_caller() {
    let sys = system_with("no-runtime", Duration::from_secs(5), Duration::from_secs(1));
    assert!(tokio::runtime::Handle::try_current().is_err());
    let report = block_on_without_a_runtime(sys.shutdown());
    assert!(report.outcomes.is_empty());
    assert!(report.swept.is_empty());
    assert!(
        completed(&sys),
        "the walk completed and left the system defunct"
    );
}
