//! A harness whose time-outs run on an actor's timers, one timer per setting.

use std::time::Duration;

use tokio_actors::actor::{context::ActorContext, Actor, ActorExt};
use tokio_actors::statem::{
    Harness, StateMachine, StatemContext, StatemEvent, Step, TimeoutId, Transition,
};
use tokio_actors::{ActorError, ActorResult, StopReason};

#[derive(Clone, PartialEq)]
enum Door {
    Locked,
    Open,
}

enum Ev {
    Code(u32),
    Hold,
    Relock,
}

struct Lock;

impl StateMachine for Lock {
    type State = Door;
    type Event = Ev;
    type Call = ();
    type Reply = bool;
    type TimerName = ();

    fn handle_event(
        &mut self,
        state: &Door,
        event: StatemEvent<Self>,
        cx: &mut StatemContext<'_, Self>,
    ) -> Transition<Door> {
        match (state, event) {
            (Door::Locked, StatemEvent::Cast(Ev::Code(1234))) => {
                cx.state_timeout(Duration::from_millis(50), Ev::Relock);
                Transition::Next(Door::Open)
            }
            (Door::Open, StatemEvent::Cast(Ev::Hold)) => {
                cx.state_timeout(Duration::from_millis(150), Ev::Relock);
                Transition::Keep
            }
            (Door::Open, StatemEvent::StateTimeout(_)) => Transition::Next(Door::Locked),
            _ => Transition::Keep,
        }
    }
}

enum Msg {
    Script(Vec<Ev>),
    Fire(TimeoutId<()>),
}

struct Driver {
    door: Harness<Lock>,
}

impl Driver {
    // Each setting with a non-zero duration becomes one one-shot timer whose
    // message carries the setting's id. A driver may also cancel the timers of
    // `step.cancelled` with `ctx.cancel_timer`; a timer whose message is
    // already queued arrives anyway, and `fire_id` ignores it.
    async fn apply(&mut self, step: Step<Lock>, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if step.stale {
            println!("stale time-out ignored");
            return Ok(());
        }
        // This borrow of the harness lives across the awaits below, so the
        // `Send` future of `handle` needs `Harness` to be `Sync`.
        let state = self.door.state();
        for (id, after) in step.set {
            if !after.is_zero() {
                ctx.schedule(Msg::Fire(id)).after(after).await?;
            }
        }
        match (state, step.entered.is_empty()) {
            (Door::Open, false) => println!("door open"),
            (Door::Open, true) => println!("door kept open"),
            (Door::Locked, _) => {
                println!("door locked again");
                ctx.stop(StopReason::Graceful)
                    .map_err(|e| ActorError::user(e.to_string()))?;
            }
        }
        Ok(())
    }
}

impl Actor for Driver {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            // Both events are fed inside this one turn, so the 50 ms setting
            // is replaced by the 150 ms one before the actor handles any timer
            // message, whatever the clock does; the first timer's message
            // then names a replaced setting and reads stale.
            Msg::Script(events) => {
                for event in events {
                    let step = self.door.cast(event);
                    self.apply(step, ctx).await?;
                }
            }
            Msg::Fire(id) => {
                let step = self.door.fire_id(&id);
                self.apply(step, ctx).await?;
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (door, _) = Harness::new(Lock, Door::Locked);
    let driver = Driver { door }.spawn().await?;
    driver
        .send(Msg::Script(vec![Ev::Code(1234), Ev::Hold]))
        .await?;
    driver.wait_stopped().await;
    Ok(())
}
