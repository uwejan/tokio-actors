//! `kill_by_id_with_observer` takes the observer and the kill from one
//! lookup. A registry that changes hands between the lookup and the raise (a
//! supervised child that restarted by itself) must not split them: the
//! observer names the incarnation the kill was raised on.

use std::time::Duration;

use crate::actor::context::ActorContext;
use crate::actor::Actor;
use crate::error::ActorResult;

/// The longest a call may take. It only limits how long a real hang takes to
/// report.
const BOUND: Duration = Duration::from_secs(10);

/// An actor that only answers.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}
