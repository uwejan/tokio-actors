//! Behavioral suite for the structured error taxonomy.
//!
//! - `ActorError` gains `Timer`/`Stream`/`Supervision` variants that carry the
//!   source error structurally (no stringification); `User(String)` remains
//!   reserved for genuinely actor-authored errors.
//! - `ChildSpawnBuilder` fails with `SpawnError`, not `ActorError`.
//! - `SpawnError::DuplicateChild` is produced by `spawn_child` against a name
//!   that still has a live or kept-spec registry entry (OTP `already_present`
//!   parity); `SpawnError::NotASupervisor` is the spawn-time surface of the
//!   same fact `SupervisionError::NotASupervisor` reports to the child
//!   management APIs.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, AskError, SendError, SpawnError, StopReason,
};

// ---------------------------------------------------------------------------
// Existing coverage (unchanged)
// ---------------------------------------------------------------------------

#[test]
fn actor_error_spawn_preserves_type() {
    let spawn_err = SpawnError::NameTaken {
        name: "x".into(),
        system: "test".into(),
    };
    let actor_err: ActorError = spawn_err.into();
    match actor_err {
        ActorError::Spawn(_) => {}
        other => panic!("expected ActorError::Spawn, got: {other}"),
    }

    // A failed call to another actor becomes this actor's own `User` error
    // with the call's text, whatever the variant: the other actor's error is
    // never passed through as this actor's.
    let asks = [
        AskError::Closed,
        AskError::Actor(ActorError::Panic("p".into())),
        AskError::ResponseDropped {
            reason: StopReason::Kill,
        },
        AskError::Timeout { enqueued: true },
        AskError::CallingSelf,
    ];
    for err in asks {
        let text = err.to_string();
        assert_eq!(ActorError::from(err), ActorError::User(text));
    }
    for err in [SendError::Closed, SendError::NotFound, SendError::Full] {
        let text = err.to_string();
        assert_eq!(ActorError::from(err), ActorError::User(text));
    }
}

// ---------------------------------------------------------------------------
// Test 18: ?-conversion structures Timer/Stream/Supervision errors
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 19: budget exhaustion records the structured Supervision error
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 20: DuplicateChild (kept spec), then delete_child clears the way
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 21: spawn_child on a non-supervisor -> SpawnError::NotASupervisor
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ctx.last_error() after a failed turn
// ---------------------------------------------------------------------------

/// An actor whose `handle` fails (an `Err`, or a panic) and whose `on_stopped`
/// reads `ctx.last_error()`.
struct FailsInHandle {
    panics: bool,
    seen: Arc<Mutex<Option<Option<ActorError>>>>,
}

impl Actor for FailsInHandle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.panics {
            panic!("handler panicked");
        }
        Err(ActorError::user("handler failed"))
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.seen.lock().unwrap() = Some(ctx.last_error().cloned());
        Ok(())
    }
}

/// A turn that ends in an `Err` or a panic records the failure, so
/// `on_stopped` reads it from `ctx.last_error()` on both paths.
#[tokio::test(flavor = "multi_thread")]
async fn last_error_after_a_failed_handler_is_read_in_on_stopped() {
    for panics in [false, true] {
        let seen = Arc::new(Mutex::new(None));
        let handle = FailsInHandle {
            panics,
            seen: seen.clone(),
        }
        .spawn()
        .await
        .unwrap();
        let _ = handle.send(()).await;
        tokio::time::timeout(Duration::from_secs(20), handle.wait_stopped())
            .await
            .expect("the actor ends");
        let recorded = seen
            .lock()
            .unwrap()
            .clone()
            .expect("on_stopped ran")
            .unwrap_or_else(|| panic!("panics={panics}: last_error was not recorded"));
        match (panics, recorded) {
            (false, ActorError::User(_)) | (true, ActorError::Panic(_)) => {}
            (panics, other) => panic!("panics={panics}: recorded {other:?}"),
        }
    }
}
