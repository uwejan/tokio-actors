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

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, AskError, SpawnError, StreamError, StreamEvent,
};

// ---------------------------------------------------------------------------
// Existing coverage (unchanged)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 18: ?-conversion structures Timer/Stream/Supervision errors
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Test 19: budget exhaustion records the structured Supervision error
// ---------------------------------------------------------------------------

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
