//! The state a turn leaves on the thread and in the clock: the caller token a
//! hook polls under, and the turn in flight when a task ends.

use std::sync::{Arc, Mutex};

use super::token;
use super::turn_tests::{both_flavors, unique};
use crate::actor::context::ActorContext;
use crate::actor::{Actor, ActorExt};
use crate::error::ActorResult;
use crate::system::ActorSystem;
use crate::types::TurnClock;

/// Reads the caller token from a hook and from a task the hook spawns.
struct Reader {
    seen: Arc<Mutex<Vec<(u64, u64, u64)>>>,
}

impl Actor for Reader {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let inside = token::current();
        let spawned = tokio::spawn(async { token::current() }).await.unwrap();
        self.seen
            .lock()
            .unwrap()
            .push((inside, ctx.turn_clock().id(), spawned));
        Ok(())
    }
}

#[test]
fn a_hook_polls_under_its_token_only_while_wait_cycle_detection_is_on() {
    both_flavors(|| async {
        let sys = ActorSystem::create(unique("token")).unwrap();
        let seen = Arc::default();
        let reader = Reader {
            seen: Arc::clone(&seen),
        };
        let handle = reader.spawn().on_system(&sys).await.unwrap();
        for on in [false, true, false] {
            sys.set_wait_cycle_detection(on);
            handle.send(()).await.unwrap();
        }
        let seen = seen.lock().unwrap().clone();
        let [(off, _, off_spawned), (on, id, on_spawned), (off_again, _, _)] = seen[..] else {
            panic!("three turns, saw {seen:?}");
        };
        assert_eq!(off, 0, "detection off");
        assert_ne!(id, 0);
        assert_eq!(
            on, id,
            "detection on: the hook's token is its clock's identity"
        );
        assert_eq!(off_again, 0, "detection off again");
        assert_eq!(
            (off_spawned, on_spawned),
            (0, 0),
            "a spawned task has no token"
        );
    });
}

/// Starts a turn and never ends it, handing its clock to the test.
struct Stuck {
    clock: Arc<Mutex<Option<TurnClock>>>,
}

impl Actor for Stuck {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        *self.clock.lock().unwrap() = Some(ctx.turn_clock());
        std::future::pending::<()>().await;
        Ok(())
    }
}
