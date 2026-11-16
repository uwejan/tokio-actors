//! Behavioral suite for the structured error taxonomy.
//!
//! - `ActorError` gains `Timer`/`Supervision` variants that carry the
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
    ActorConfig, ActorError, ActorResult, AskError, RestartType, SendError, SpawnError, StopReason,
    SupervisionConfig, SupervisionError,
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
// Test 18: ?-conversion structures Timer/Supervision errors
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 19: budget exhaustion records the structured Supervision error
// ---------------------------------------------------------------------------

/// A child whose `pre_start` always panics: every (re)incarnation burns one
/// restart, driving the supervisor's budget to exhaustion quickly.
#[derive(Debug, Default)]
struct AlwaysFailsInit;

impl Actor for AlwaysFailsInit {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("init always fails (budget-exhaustion driver)");
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A supervisor with a tight restart budget. `on_stopped` captures
/// `ctx.last_error()` externally so the test can inspect it after the
/// supervisor exits: the exit reason itself is `StopReason::ParentRequest`
/// (OTP intensity-exceeded parity), which carries no error detail - only
/// `last_error` does.
struct BudgetSupervisor {
    captured: Arc<Mutex<Option<ActorError>>>,
}

impl Actor for BudgetSupervisor {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(AlwaysFailsInit::default)
            .restart_type(RestartType::Permanent)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.captured.lock().unwrap() = ctx.last_error().cloned();
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn budget_exhaustion_records_structured_supervision_error() {
    let captured = Arc::new(Mutex::new(None));
    let sup = BudgetSupervisor {
        captured: captured.clone(),
    }
    .spawn()
    .with_config(ActorConfig::default().with_supervision(
        SupervisionConfig::one_for_one().max_restarts(2, Duration::from_secs(60)),
    ))
    .await
    .unwrap();

    let reason = tokio::time::timeout(Duration::from_secs(10), sup.wait_stopped())
        .await
        .expect("supervisor must exit once its restart budget is exhausted");
    assert_eq!(reason, StopReason::ParentRequest, "the OTP shutdown reason");

    let err = captured.lock().unwrap().take();
    assert!(
        matches!(
            err,
            Some(ActorError::Supervision(SupervisionError::BudgetExhausted))
        ),
        "expected Some(Supervision(BudgetExhausted)), got: {err:?}"
    );
}

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
