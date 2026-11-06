//! What a runtime built without the Tokio time driver does with actors: plain
//! actors run, an actor timer panics in its own task and leaves the actor
//! running, and a `Kill` aborts the task at once with no kill grace.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::{Builder, Runtime};
use tokio::sync::Notify;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, StopReason,
};

fn without_timers() -> Runtime {
    Builder::new_current_thread().build().unwrap()
}

fn with_timers() -> Runtime {
    Builder::new_current_thread().enable_all().build().unwrap()
}

struct Plain;

impl Actor for Plain {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, msg: u32, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        Ok(msg + 1)
    }
}

#[test]
fn a_plain_actor_runs_and_stops_gracefully_on_a_runtime_without_the_time_driver() {
    let rt = without_timers();
    let reason = rt.block_on(async {
        let actor = Plain.spawn().await.expect("the actor starts");
        assert_eq!(actor.send(41).await.expect("the actor answers"), 42);
        actor.stop(StopReason::Graceful).await.expect("accepted");
        actor.wait_stopped().await
    });
    assert_eq!(reason, StopReason::Graceful);
}

/// Holds a recurring timer and answers each message with the last error its
/// timers left.
struct Ticker;

impl Actor for Ticker {
    type Message = ();
    type Response = Option<String>;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.every_with(Duration::from_millis(1), || ())
            .await
            .expect("the timer starts");
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: (),
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Option<String>> {
        Ok(ctx.last_forwarder_error().map(ToString::to_string))
    }
}

#[test]
fn an_actor_timer_panics_in_its_own_task_and_the_actor_keeps_running() {
    let rt = without_timers();
    rt.block_on(async {
        let actor = Ticker.spawn().await.expect("the actor starts");
        // The timer's task panics at its first poll; the actor's loop reaps
        // it between two requests, so a later request reads its error.
        let mut error = None;
        for _ in 0..1000 {
            tokio::task::yield_now().await;
            error = actor.send(()).await.expect("the actor answers");
            if error.is_some() {
                break;
            }
        }
        assert!(error.is_some(), "the timer's task panicked");
        assert_eq!(actor.status(), ActorStatus::Running);
        actor.send(()).await.expect("the actor still answers");
    });
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

/// Spawns a parked actor with a 3 second kill grace, kills it, and returns
/// how long the kill took to end the task and the reason it ended with.
fn time_to_end_after_a_kill(rt: &Runtime) -> (Duration, StopReason) {
    rt.block_on(async {
        let entered = Arc::new(Notify::new());
        let actor = Stuck {
            entered: entered.clone(),
        }
        .spawn()
        .kill_grace(Duration::from_secs(3))
        .await
        .expect("the actor starts");
        actor.notify(()).await.expect("accepted");
        entered.notified().await;
        let started = Instant::now();
        actor.stop(StopReason::Kill).await.expect("accepted");
        let reason = actor.wait_stopped().await;
        (started.elapsed(), reason)
    })
}

#[test]
fn a_kill_waits_the_grace_on_a_runtime_with_the_time_driver() {
    let (elapsed, reason) = time_to_end_after_a_kill(&with_timers());
    assert_eq!(reason, StopReason::Kill);
    assert!(
        elapsed >= Duration::from_millis(2500),
        "the abort waited the grace: {elapsed:?}"
    );
}

#[test]
fn a_kill_aborts_at_once_with_no_grace_on_a_runtime_without_the_time_driver() {
    let (elapsed, reason) = time_to_end_after_a_kill(&without_timers());
    assert_eq!(reason, StopReason::Kill);
    assert!(
        elapsed < Duration::from_millis(1500),
        "the abort took no grace: {elapsed:?}"
    );
}
