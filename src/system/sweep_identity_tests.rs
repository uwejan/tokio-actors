//! The turn watchdog names a stuck actor by its own record, not by the
//! address of the record: an actor that ends while the sweep writes the
//! lines before its own is still named by its own identity.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::actor::context::ActorContext;
use crate::actor::Actor;
use crate::error::ActorResult;

/// Parks inside its first turn.
struct Stuck {
    entered: Arc<AtomicBool>,
}

impl Actor for Stuck {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// Ends at once.
struct Quick;

impl Actor for Quick {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}
