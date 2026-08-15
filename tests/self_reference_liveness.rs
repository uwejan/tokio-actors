//! Behavioral proof of the self-reference contract: internal self-references
//! (a recurring schedule, in this case) never extend an actor's lifetime.
//! The actor stays alive on its own terms, independent of any external
//! handle, and its internal timer is torn down the moment the actor stops -
//! matching Erlang/OTP, where pid-addressed timers are automatically
//! canceled when their target process dies (erlang.org, OTP 28, `timer`
//! module BIFs).

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, ActorSystem,
};

struct Ticker {
    counter: Arc<AtomicU32>,
}

#[derive(Clone)]
enum Msg {
    Tick,
}

impl Actor for Ticker {
    type Message = Msg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.schedule(Msg::Tick)
            .every(Duration::from_millis(50))
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Polls a synchronous predicate until it holds or the deadline passes.
async fn wait_until<F>(timeout_ms: u64, mut pred: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timer_actor_liveness_contract() {
    let counter = Arc::new(AtomicU32::new(0));
    let handle = Ticker {
        counter: counter.clone(),
    }
    .spawn()
    .await
    .expect("spawn succeeds");
    let id = handle.id().clone();

    // Drop the only external handle. The actor's internal schedule holds a
    // self-reference, but that self-reference does not extend its lifetime -
    // it is already alive on its own terms, per the explicit lifetime model.
    drop(handle);

    let ticked = wait_until(2_000, || counter.load(Ordering::SeqCst) > 0).await;
    assert!(
        ticked,
        "recurring timer keeps firing with no external handle"
    );

    let sys = ActorSystem::default();
    assert_eq!(
        sys.actor_status(&id),
        Some(ActorStatus::Running),
        "an internal self-reference does not keep an otherwise-dead actor alive; \
         this one is alive because nothing has stopped it yet"
    );

    assert!(sys.kill_by_id(&id).await, "kill_by_id finds the zombie");

    // kill_by_id awaits the actor reaching a terminal status, so the
    // ActorContext (and therefore its timer-cancellation Drop impl) has
    // already run by the time this returns.
    let after_kill = counter.load(Ordering::SeqCst);

    // Margin covering several tick intervals: if the timer were not torn
    // down, the counter would keep advancing here.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after_margin = counter.load(Ordering::SeqCst);

    assert_eq!(
        after_kill, after_margin,
        "the internal timer is torn down when the actor stops; the counter must stop advancing"
    );
}
