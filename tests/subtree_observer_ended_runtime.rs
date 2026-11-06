//! A `wait_ended` polled after the runtime it was started on has ended, with
//! that runtime still entered, passes the entry check (a runtime that has
//! ended is not one without the time driver) and panics at its next timed
//! wait, unless the subtree has ended by then, in which case it answers.

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, SubtreeObserver,
};

const BOUND: Duration = Duration::from_secs(10);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// A supervisor with one named child that parks in its handler until
/// released.
struct Boss {
    child: String,
    release: Arc<Notify>,
    entered: Arc<Notify>,
}

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (release, entered) = (self.release.clone(), self.entered.clone());
        ctx.spawn_child(move || Parked {
            release: release.clone(),
            entered: entered.clone(),
        })
        .named(self.child.clone())
        .kill_grace(Duration::from_secs(30))
        .await?;
        Ok(())
    }
}

/// Enters its handler on its first message and stays there until released.
struct Parked {
    release: Arc<Notify>,
    entered: Arc<Notify>,
}

impl Actor for Parked {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

fn multi_thread(workers: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
}

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// A killed boss on `target`, with a child still parked in its handler: the
/// boss's observer, and the notification that releases the child.
fn killed_boss_with_a_live_child(
    target: &tokio::runtime::Runtime,
) -> (SubtreeObserver, Arc<Notify>) {
    target.block_on(async {
        let sys = ActorSystem::create(format!("ended-runtime-{}", uuid::Uuid::new_v4())).unwrap();
        let (release, entered) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let child = format!("parked-{}", uuid::Uuid::new_v4());
        let boss = Boss {
            child: child.clone(),
            release: release.clone(),
            entered: entered.clone(),
        }
        .spawn()
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        sys.get::<Parked>(&child)
            .expect("the child is registered")
            .notify(())
            .await
            .unwrap();
        timeout(BOUND, entered.notified())
            .await
            .expect("the child is in its handler");
        let observer = boss.observe_subtree();
        timeout(BOUND, sys.kill_by_id(boss.id()))
            .await
            .expect("kill_by_id returns")
            .expect("registered");
        (observer, release)
    })
}

#[test]
fn a_call_polled_after_its_runtime_ended_panics_at_its_next_timed_wait() {
    let target = multi_thread(2);
    let (observer, _release) = killed_boss_with_a_live_child(&target);

    let caller = current_thread();
    let caller_handle = caller.handle().clone();
    let _entered = caller_handle.enter();
    let mut cx = Context::from_waker(Waker::noop());
    let mut call = Box::pin(observer.wait_ended());
    assert!(
        call.as_mut().poll(&mut cx).is_pending(),
        "the boss ended and its child lives: the call waits on a timer of the caller's runtime"
    );

    drop(caller);

    // The boss's task may still be letting go of its status plane and its
    // hold on the subtree when the call is polled again, and the call then
    // waits for that in its first part, which needs no timer. It goes on to
    // its timed wait once the boss has let go of both, and that wait panics:
    // poll until it does.
    let started = Instant::now();
    let polled = loop {
        let polled = catch_unwind(AssertUnwindSafe(|| call.as_mut().poll(&mut cx)));
        match polled {
            Ok(Poll::Pending) if started.elapsed() < HANG_BOUND => {
                std::thread::sleep(Duration::from_millis(1));
            }
            other => break other,
        }
    };
    assert!(
        polled.is_err(),
        "the next timed wait panics while the child below still runs: {:?}",
        polled.map(|p| matches!(p, Poll::Ready(_)))
    );
}
