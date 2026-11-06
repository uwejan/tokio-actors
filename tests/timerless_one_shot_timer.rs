//! A one-shot timer of an actor on a runtime built without the Tokio time
//! driver panics in its own task at its first poll, and the actor keeps
//! running without it.

use std::time::{Duration, Instant};

use tokio::runtime::{Builder, Runtime};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus,
};

fn without_timers() -> Runtime {
    Builder::new_current_thread().build().unwrap()
}

/// Registers a one-shot timer when it starts, and answers each message with
/// the last error its timers left.
struct OneShot;

impl Actor for OneShot {
    type Message = ();
    type Response = Option<String>;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.schedule(())
            .after(Duration::from_millis(1))
            .await
            .expect("the timer is registered");
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
fn a_one_shot_timer_panics_in_its_own_task_and_the_actor_keeps_running() {
    let rt = without_timers();
    rt.block_on(async {
        let actor = OneShot.spawn().await.expect("the actor starts");
        // The timer's task panics at its first poll; the actor's loop reaps
        // it between two requests, so a later request reads its error. The
        // wall-clock limit is reached only if the panic never reports.
        let started = Instant::now();
        let mut error = None;
        while error.is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the timer's task never reported its panic"
            );
            tokio::task::yield_now().await;
            error = actor.send(()).await.expect("the actor answers");
        }
        assert_eq!(actor.status(), ActorStatus::Running);
        actor.send(()).await.expect("the actor still answers");
    });
}
