//! `ActorSystem::kill` and `ActorSystem::kill_by_id` on an actor whose runtime
//! has no time driver abort a parked callback at once, whatever the actor's
//! kill grace. `kill_by_id` answers a registered target that takes the kill
//! even when it is polled on a runtime that is shutting down or has ended: the
//! entry check lets it pass and the kill is raised.

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::task::{Context, Waker};
use std::time::{Duration, Instant};

use tokio::runtime::{Builder, Runtime};
use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, handle::ActorHandle, Actor, ActorExt},
    ActorResult, ActorStatus, ActorSystem, StopOutcome, StopReason,
};

const GRACE: Duration = Duration::from_secs(3);

/// Well under the grace, and far above the cost of an abort.
const AT_ONCE: Duration = Duration::from_millis(1500);

const HANG_BOUND: Duration = Duration::from_secs(60);

/// A runtime whose worker thread drives the actor and which has no time
/// driver.
fn driverless_actor_runtime() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap()
}

fn driven_caller_runtime() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Parks inside its handler until the runtime aborts it.
struct Stuck {
    entered: Arc<Notify>,
}

impl Actor for Stuck {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// Spawns a parked `Stuck` with a 3 s grace under `name` in `sys`, on `rt`.
fn parked_actor(rt: &Runtime, sys: &Arc<ActorSystem>, name: &str) -> ActorHandle<Stuck> {
    let entered = Arc::new(Notify::new());
    let actor = rt.block_on(async {
        let actor = Stuck {
            entered: entered.clone(),
        }
        .spawn()
        .named(name)
        .kill_grace(GRACE)
        .on_system(sys)
        .await
        .expect("the actor starts");
        actor.notify(()).await.expect("accepted");
        entered.notified().await;
        actor
    });
    actor
}

fn unique(what: &str) -> String {
    format!("{what}-{}", uuid::Uuid::new_v4())
}

#[test]
fn kill_by_name_aborts_a_parked_actor_at_once_when_its_runtime_has_no_time_driver() {
    let actor_rt = driverless_actor_runtime();
    let caller = driven_caller_runtime();
    let sys = ActorSystem::create(unique("kill-by-name-driverless")).unwrap();
    let actor = parked_actor(&actor_rt, &sys, "stuck");

    let started = Instant::now();
    let reason = caller.block_on(async {
        sys.kill("stuck").await.expect("the kill is accepted");
        timeout(HANG_BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends")
    });
    assert_eq!(reason, StopReason::Kill);
    assert!(
        started.elapsed() < AT_ONCE,
        "no kill grace was waited: {:?}",
        started.elapsed()
    );
}

#[test]
fn kill_by_id_aborts_a_parked_actor_at_once_when_its_runtime_has_no_time_driver() {
    let actor_rt = driverless_actor_runtime();
    let caller = driven_caller_runtime();
    let sys = ActorSystem::create(unique("kill-by-id-driverless")).unwrap();
    let actor = parked_actor(&actor_rt, &sys, "stuck");
    let id = actor.id().clone();

    let started = Instant::now();
    let report = caller
        .block_on(async { timeout(HANG_BOUND, sys.kill_by_id(&id)).await })
        .expect("the call returns")
        .expect("a registered target that takes the kill answers a report");
    assert_eq!(report.outcome, StopOutcome::Aborted);
    assert_eq!(report.reason, Some(StopReason::Kill));
    assert!(
        started.elapsed() < AT_ONCE,
        "no kill grace was waited: {:?}",
        started.elapsed()
    );
}

/// Idle actor that takes a kill.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn kill_by_id_polled_on_a_runtime_that_has_ended_raises_the_kill_instead_of_refusing_at_entry() {
    let host = driven_caller_runtime();
    let sys = ActorSystem::create(unique("kill-by-id-ended-runtime")).unwrap();
    let target = host
        .block_on(async { Idle.spawn().named("target").on_system(&sys).await })
        .unwrap();
    let id = target.id().clone();

    // A runtime without the time driver, ended before the call is polled.
    let caller = Builder::new_current_thread().build().unwrap();
    let caller_handle = caller.handle().clone();
    drop(caller);
    let _entered = caller_handle.enter();

    let mut call = Box::pin(sys.kill_by_id(&id));
    let mut cx = Context::from_waker(Waker::noop());
    // The entry check lets the call pass; the wait that follows may panic on
    // the ended runtime, which is not what is pinned here.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _ = call.as_mut().poll(&mut cx);
    }));
    drop(call);

    let reason = host.block_on(async {
        timeout(HANG_BOUND, target.wait_stopped())
            .await
            .expect("the kill the call raised ends the target")
    });
    assert_eq!(reason, StopReason::Kill);
    assert_eq!(target.status(), ActorStatus::Stopped);
}
