//! A door that locks itself again: a state machine with a state time-out.

use std::time::Duration;

use tokio::sync::oneshot;
use tokio_actors::statem::{
    EnterContext, StateMachine, StatemActor, StatemContext, StatemEvent, Transition,
};
use tokio_actors::ActorExt;

#[derive(Clone, PartialEq)]
enum Door {
    Locked,
    Open,
}

struct Lock {
    code: u32,
    relocked: Option<oneshot::Sender<()>>,
}

impl StateMachine for Lock {
    type State = Door;
    type Event = u32;
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
            (Door::Locked, StatemEvent::Cast(code)) if code == self.code => {
                cx.state_timeout(Duration::from_millis(50), 0);
                Transition::Next(Door::Open)
            }
            (Door::Open, StatemEvent::StateTimeout(_)) => Transition::Next(Door::Locked),
            (state, StatemEvent::Call((), from)) => {
                cx.reply(from, *state == Door::Open);
                Transition::Keep
            }
            _ => Transition::Keep,
        }
    }

    fn on_enter(&mut self, old: &Door, new: &Door, _cx: &mut EnterContext<'_, Self>) {
        if (old, new) == (&Door::Open, &Door::Locked) {
            self.relocked.take().map(|relocked| relocked.send(()));
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (relocked, locked_again) = oneshot::channel();
    let lock = Lock {
        code: 1234,
        relocked: Some(relocked),
    };
    let door = StatemActor::new(lock, Door::Locked).spawn().await?;

    door.cast(1234).await?;
    locked_again.await?;
    assert!(!door.call(()).await?, "the state time-out locks it");
    println!("door locked again after timeout");
    Ok(())
}
