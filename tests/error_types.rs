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
    ActorConfig, ActorError, ActorHandle, ActorResult, AskError, RestartType, SpawnError,
    StopReason, StreamError, StreamEvent, SupervisionConfig, SupervisionError, TimerError,
};

// ---------------------------------------------------------------------------
// Existing coverage (unchanged)
// ---------------------------------------------------------------------------

#[test]
fn name_taken_error_includes_system() {
    let err = SpawnError::NameTaken {
        name: "counter".into(),
        system: "default".into(),
    };
    let msg = err.to_string();
    assert!(msg.contains("counter"), "error should mention the name");
    assert!(msg.contains("default"), "error should mention the system");
}

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
}

#[test]
fn system_name_taken_is_visible() {
    let err = SpawnError::SystemNameTaken("prod".into());
    assert!(err.to_string().contains("prod"));
}

// ---------------------------------------------------------------------------
// Test 18: ?-conversion structures Timer/Stream/Supervision errors
// ---------------------------------------------------------------------------

/// A plain actor whose `handle` double-cancels a one-shot timer: the second
/// `cancel_timer` returns `TimerError::NotFound`, and `?` must surface it as
/// `ActorError::Timer(TimerError::NotFound)` - a real crash, not just a
/// direct `From` call.
#[derive(Debug, Default)]
struct TimerCrasher;

impl Actor for TimerCrasher {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let id = ctx.schedule(()).after(Duration::from_secs(60)).await?;
        ctx.cancel_timer(id)?;
        ctx.cancel_timer(id)?; // already cancelled -> NotFound
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn timer_error_arrives_structured() {
    // Direct conversion: no stringification.
    let direct: ActorError = TimerError::NotFound.into();
    assert!(
        matches!(direct, ActorError::Timer(TimerError::NotFound)),
        "expected ActorError::Timer(TimerError::NotFound), got: {direct}"
    );

    // Real actor path: the same structured variant crosses `?` inside
    // `handle` and comes back through `send` as `AskError::Actor`.
    let handle = TimerCrasher.spawn().await.unwrap();
    let err = handle.send(()).await.expect_err("double-cancel must crash");
    match err {
        AskError::Actor(ActorError::Timer(TimerError::NotFound)) => {}
        other => panic!("expected AskError::Actor(ActorError::Timer(NotFound)), got: {other:?}"),
    }
}

/// Message type for the stream-crash actor: only `Go` is ever sent by the
/// test; `Data`/`Done` exist to satisfy `add_stream`'s `From<StreamEvent<_>>`
/// bound and are harmless no-ops if they are ever actually delivered.
#[derive(Clone)]
enum StreamMsg {
    Go,
    Data,
    Done,
}

impl From<StreamEvent<u32>> for StreamMsg {
    fn from(event: StreamEvent<u32>) -> Self {
        match event {
            StreamEvent::Data(_) => StreamMsg::Data,
            StreamEvent::Finished => StreamMsg::Done,
        }
    }
}

/// A plain actor whose `handle` double-cancels an attached stream: the second
/// `cancel_stream` returns `StreamError::NotFound`, surfaced via `?`.
#[derive(Default)]
struct StreamCrasher;

impl Actor for StreamCrasher {
    type Message = StreamMsg;
    type Response = ();

    async fn handle(&mut self, msg: StreamMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            StreamMsg::Go => {
                let id = ctx.add_stream(tokio_stream::iter(std::iter::empty::<u32>()));
                ctx.cancel_stream(id)?;
                ctx.cancel_stream(id)?; // already cancelled -> NotFound
                Ok(())
            }
            StreamMsg::Data | StreamMsg::Done => Ok(()),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_error_arrives_structured() {
    let direct: ActorError = StreamError::NotFound.into();
    assert!(
        matches!(direct, ActorError::Stream(StreamError::NotFound)),
        "expected ActorError::Stream(StreamError::NotFound), got: {direct}"
    );

    let handle = StreamCrasher.spawn().await.unwrap();
    let err = handle
        .send(StreamMsg::Go)
        .await
        .expect_err("double-cancel must crash");
    match err {
        AskError::Actor(ActorError::Stream(StreamError::NotFound)) => {}
        other => panic!("expected AskError::Actor(ActorError::Stream(NotFound)), got: {other:?}"),
    }
}

/// A supervisor whose `handle` restarts a child id that was never spawned:
/// `restart_child` returns `SupervisionError::ChildNotFound`, surfaced via `?`.
#[derive(Debug, Default)]
struct SupervisionCrasher;

impl Actor for SupervisionCrasher {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.restart_child("never-spawned")?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn supervision_error_arrives_structured() {
    let direct: ActorError = SupervisionError::ChildNotFound("x".into()).into();
    assert!(
        matches!(
            direct,
            ActorError::Supervision(SupervisionError::ChildNotFound(_))
        ),
        "expected ActorError::Supervision(ChildNotFound), got: {direct}"
    );

    // Must be a supervisor (else `restart_child` fails NotASupervisor first),
    // but no child is ever spawned - `restart_child` must see ChildNotFound.
    let handle = SupervisionCrasher.spawn().supervisor().await.unwrap();
    let err = handle
        .send(())
        .await
        .expect_err("must crash: no such child");
    match err {
        AskError::Actor(ActorError::Supervision(SupervisionError::ChildNotFound(_))) => {}
        other => panic!(
            "expected AskError::Actor(ActorError::Supervision(ChildNotFound)), got: {other:?}"
        ),
    }
}

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

    tokio::time::timeout(Duration::from_secs(10), sup.wait_stopped())
        .await
        .expect("supervisor must exit once its restart budget is exhausted");

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

/// Trivial supervised-child target for tests 20 and 21.
#[derive(Debug, Default)]
struct Leaf;

impl Actor for Leaf {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SpawnOutcome {
    Ok,
    Duplicate,
    Other(String),
}

#[derive(Clone)]
enum MgmtCmd {
    SpawnNamed(String),
    Terminate(String),
    Delete(String),
}

#[derive(Default)]
struct DupSupervisor;

impl Actor for DupSupervisor {
    type Message = MgmtCmd;
    type Response = SpawnOutcome;

    async fn handle(
        &mut self,
        msg: MgmtCmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<SpawnOutcome> {
        let outcome = match msg {
            MgmtCmd::SpawnNamed(name) => match ctx.spawn_child(Leaf::default).named(name).await {
                Ok(_) => SpawnOutcome::Ok,
                Err(SpawnError::DuplicateChild(_)) => SpawnOutcome::Duplicate,
                Err(other) => SpawnOutcome::Other(other.to_string()),
            },
            MgmtCmd::Terminate(id) => match ctx.terminate_child(id).await {
                Ok(()) => SpawnOutcome::Ok,
                Err(e) => SpawnOutcome::Other(e.to_string()),
            },
            MgmtCmd::Delete(id) => match ctx.delete_child(id) {
                Ok(()) => SpawnOutcome::Ok,
                Err(e) => SpawnOutcome::Other(e.to_string()),
            },
        };
        Ok(outcome)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_child_kept_spec_then_delete_allows_respawn() {
    let sup = DupSupervisor.spawn().supervisor().await.unwrap();
    let name = format!("dup-child-{}", uuid::Uuid::new_v4());

    assert_eq!(
        sup.send(MgmtCmd::SpawnNamed(name.clone())).await.unwrap(),
        SpawnOutcome::Ok,
        "first spawn under a fresh name must succeed"
    );

    assert_eq!(
        sup.send(MgmtCmd::Terminate(name.clone())).await.unwrap(),
        SpawnOutcome::Ok,
        "terminate_child must succeed and keep the spec"
    );

    assert_eq!(
        sup.send(MgmtCmd::SpawnNamed(name.clone())).await.unwrap(),
        SpawnOutcome::Duplicate,
        "spawn_child against a kept spec must return DuplicateChild (OTP already_present)"
    );

    // Registry bookkeeping may lag the terminate by one death-plane
    // observation (the terminated child's own watcher completion still has
    // to be picked up and adopted), so poll the delete until the child is
    // seen dead instead of asserting immediately.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match sup.send(MgmtCmd::Delete(name.clone())).await.unwrap() {
            SpawnOutcome::Ok => break,
            SpawnOutcome::Other(ref e)
                if e.contains("is running") && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            other => panic!("delete_child must clear the kept spec, got: {other:?}"),
        }
    }

    assert_eq!(
        sup.send(MgmtCmd::SpawnNamed(name.clone())).await.unwrap(),
        SpawnOutcome::Ok,
        "spawn_child must succeed again once the old spec is deleted"
    );
}

// ---------------------------------------------------------------------------
// Test 21: spawn_child on a non-supervisor -> SpawnError::NotASupervisor
// ---------------------------------------------------------------------------

/// Not configured as a supervisor (no `.supervisor()` / `.with_supervision`),
/// so `ctx.spawn_child` must reject with `SpawnError::NotASupervisor`.
///
/// The `Result<ActorHandle<Leaf>, SpawnError>` annotation below is the
/// compile-level half of the assertion: `ChildSpawnBuilder`'s
/// `IntoFuture::Output` really is `SpawnError`, not `ActorError` - if it were
/// `ActorError`, this annotation would fail to type-check and the file would
/// not build.
#[derive(Default)]
struct NotSupervisor;

impl Actor for NotSupervisor {
    type Message = ();
    type Response = bool;

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<bool> {
        let result: Result<ActorHandle<Leaf>, SpawnError> = ctx.spawn_child(Leaf::default).await;
        Ok(matches!(result, Err(SpawnError::NotASupervisor)))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn spawn_child_on_non_supervisor_is_not_a_supervisor() {
    let handle = NotSupervisor.spawn().await.unwrap();
    let matched = handle.send(()).await.unwrap();
    assert!(
        matched,
        "expected SpawnError::NotASupervisor from a non-supervisor spawn_child"
    );
}
