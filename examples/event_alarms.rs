//! An alarm manager with two handlers, one of which is removed.

use tokio_actors::event::{EventHandler, EventManager, EventManagerRef, Handled};
use tokio_actors::ActorExt;

struct Count(u32);

impl EventHandler for Count {
    type Event = String;
    type Call = ();
    type Reply = u32;

    fn handle_event(&mut self, _alarm: &String) -> Handled {
        self.0 += 1;
        Handled::Keep
    }

    fn handle_call(&mut self, _: ()) -> (Handled, Option<u32>) {
        (Handled::Keep, Some(self.0))
    }
}

struct Print;

impl EventHandler for Print {
    type Event = String;
    type Call = ();
    type Reply = u32;

    fn handle_event(&mut self, alarm: &String) -> Handled {
        println!("alarm: {alarm}");
        Handled::Keep
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manager = EventManager::<String, (), u32>::new().spawn().await?;
    let alarms = EventManagerRef::of(&manager);

    let counter = alarms.add_handler(Count(0)).await?;
    let printer = alarms.add_handler(Print).await?;
    alarms.sync_notify("door forced".to_string()).await?;
    assert_eq!(alarms.call(counter, ()).await?, 1);

    alarms.delete_handler(printer).await?;
    println!(
        "{} handler left after remove",
        alarms.which_handlers().await?.len()
    );
    Ok(())
}
