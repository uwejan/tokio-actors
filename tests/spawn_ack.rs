//! Behavioral suite for the spawn ack.
//!
//! - Default `spawn().await` does not return until `pre_start` AND
//!   `on_started` have both run (OTP `start_link` parity); an `Err` from
//!   either surfaces as `SpawnError::Init`.
//! - The failure ack fires only after the registry guard has dropped, so an
//!   immediate same-name respawn can never observe `NameTaken` (OTP 26
//!   corpse-consumed parity) - the load-bearing invariant under test.
//! - `.detached()` reproduces the exact v0.7.1 fire-and-forget behavior.
//! - `.start_timeout(dur)` bounds the wait; on expiry the task is aborted
//!   with no lifecycle hook running afterward.
//! - When a SUPERVISOR's `on_started` spawns children via `ctx.spawn_child`
//!   and then fails, the shared teardown tail's Phase 5 (`stop_all_children`)
//!   still runs: pre-spawned children are stopped, not orphaned, even though
//!   the supervisor's own `on_stopped` is skipped (self-cleaning contract).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, SpawnError, StopReason,
};

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

type Log = Arc<Mutex<Vec<&'static str>>>;

/// Records `pre_start` and `on_started` into a shared log, yielding inside
/// each hook so a premature ack would have a real chance to race ahead.
#[derive(Default)]
struct OrderActor {
    log: Log,
}

impl Actor for OrderActor {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        tokio::task::yield_now().await;
        self.log.lock().unwrap().push("pre_start");
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        tokio::task::yield_now().await;
        self.log.lock().unwrap().push("on_started");
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// An actor whose `pre_start` always fails: init never completes.
#[derive(Default)]
struct FailInit;

impl Actor for FailInit {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("nope"))
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// An actor whose `pre_start` sleeps far longer than any test's start_timeout,
/// and whose `on_stopped` records whether it ever ran.
#[derive(Default)]
struct SlowInit {
    stopped: Arc<AtomicBool>,
}

impl Actor for SlowInit {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// An actor whose `pre_start` succeeds but whose `on_started` always panics,
/// and whose `on_stopped` records whether it ever ran.
#[derive(Default)]
struct PanicInit {
    stopped: Arc<AtomicBool>,
}

impl Actor for PanicInit {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("on_started always panics");
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// A child that flips `stopped` in `on_stopped`; used to prove a
/// supervisor's pre-spawned children are torn down (not orphaned) when the
/// supervisor's own init fails after spawning them.
#[derive(Default)]
struct TeardownChild {
    stopped: Arc<AtomicBool>,
}

impl Actor for TeardownChild {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Handles of the children spawned by [`SpawnsChildrenThenFailsInit`], handed
/// out so the test can assert on them after `spawn().await` returns.
type ChildHandles = Arc<Mutex<Vec<ActorHandle<TeardownChild>>>>;

/// A SUPERVISOR whose `on_started` spawns two children via `ctx.spawn_child`
/// and then always fails. Proves that Phase 5 (`stop_all_children`) runs off
/// the shared teardown tail even when the failing init belongs to the
/// supervisor itself, not to one of its children.
#[derive(Default)]
struct SpawnsChildrenThenFailsInit {
    child1_stopped: Arc<AtomicBool>,
    child2_stopped: Arc<AtomicBool>,
    handles: ChildHandles,
}

impl Actor for SpawnsChildrenThenFailsInit {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let stopped1 = self.child1_stopped.clone();
        let child1 = ctx
            .spawn_child(move || TeardownChild {
                stopped: stopped1.clone(),
            })
            .await
            .expect("child 1 must spawn");

        let stopped2 = self.child2_stopped.clone();
        let child2 = ctx
            .spawn_child(move || TeardownChild {
                stopped: stopped2.clone(),
            })
            .await
            .expect("child 2 must spawn");

        self.handles.lock().unwrap().extend([child1, child2]);

        Err(ActorError::user(
            "supervisor init fails after spawning children",
        ))
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Polls a synchronous predicate until it holds or the deadline passes
/// (mirrors the `wait_until` helper used across the supervision suites).
async fn wait_until<F>(timeout_ms: u64, mut pred: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_spawn_waits_for_on_started() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let handle = OrderActor { log: log.clone() }.spawn().await.unwrap();

    // No sleep here: the ack itself is the proof both hooks already ran, in
    // order, before `.await` returned.
    assert_eq!(
        *log.lock().unwrap(),
        vec!["pre_start", "on_started"],
        "spawn().await must not return before pre_start AND on_started have both run"
    );

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_start_err_surfaces_as_spawn_error_init() {
    let result = FailInit.spawn().await;

    match result {
        Err(SpawnError::Init(err)) => {
            assert!(
                matches!(*err, ActorError::User(_)),
                "expected the original ActorError::User to survive, got: {err}"
            );
            assert!(
                err.to_string().contains("nope"),
                "the original error message must survive into the ack: {err}"
            );
        }
        Ok(_) => panic!("spawn must fail when pre_start returns Err"),
        Err(other) => panic!("expected SpawnError::Init, got: {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_respawn_after_failed_init_never_sees_name_taken() {
    let name = "spawn-ack-respawn-loop";

    for round in 1..=100 {
        let result = FailInit.spawn().named(name).await;
        match result {
            Err(SpawnError::Init(_)) => {}
            Ok(_) => panic!("round {round}: spawn must fail when pre_start returns Err"),
            Err(SpawnError::NameTaken { .. }) => {
                panic!("round {round}: NameTaken - the failure ack raced the registry guard's drop")
            }
            Err(other) => panic!("round {round}: expected SpawnError::Init, got: {other}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_spawn_does_not_wait_and_stays_silent_on_init_failure() {
    // .detached() reproduces the exact v0.7.1 behavior: the ack is skipped
    // entirely, so spawn() itself reports success even though pre_start is
    // guaranteed to fail moments later in the background.
    let handle = FailInit.spawn().detached().await.unwrap();

    // The actor still dies from the failed pre_start - just silently, with
    // nothing telling THIS caller why. wait_stopped still resolves.
    tokio::time::timeout(Duration::from_secs(2), handle.wait_stopped())
        .await
        .expect("the detached actor must still die from the failed pre_start");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_timeout_aborts_slow_init_and_skips_on_stopped() {
    let stopped = Arc::new(AtomicBool::new(false));
    let result = SlowInit {
        stopped: stopped.clone(),
    }
    .spawn()
    .start_timeout(Duration::from_millis(50))
    .await;

    match result {
        Err(SpawnError::StartTimeout) => {}
        Ok(_) => panic!("spawn must time out: pre_start sleeps far past the deadline"),
        Err(other) => panic!("expected SpawnError::StartTimeout, got: {other}"),
    }

    // Give the abort plenty of time to actually land, then confirm no
    // lifecycle hook ever ran afterward (there is nothing left to run one on:
    // the task was dropped mid pre_start, kill-tier parity with OTP).
    sleep(Duration::from_millis(200)).await;
    assert!(
        !stopped.load(Ordering::SeqCst),
        "on_stopped must NOT run after a start_timeout abort"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_respawn_after_start_timeout_never_sees_name_taken() {
    let name = "spawn-ack-start-timeout-respawn-loop";

    for round in 1..=20 {
        let stopped = Arc::new(AtomicBool::new(false));
        let result = SlowInit { stopped }
            .spawn()
            .named(name)
            .start_timeout(Duration::from_millis(30))
            .await;
        match result {
            Err(SpawnError::StartTimeout) => {}
            Ok(_) => {
                panic!("round {round}: spawn must time out: pre_start sleeps past the deadline")
            }
            Err(SpawnError::NameTaken { .. }) => panic!(
                "round {round}: NameTaken - the timeout error raced the \
                 aborted task's own teardown"
            ),
            Err(other) => panic!("round {round}: expected SpawnError::StartTimeout, got: {other}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_started_panic_surfaces_as_spawn_error_init_and_skips_on_stopped() {
    let stopped = Arc::new(AtomicBool::new(false));
    let result = PanicInit {
        stopped: stopped.clone(),
    }
    .spawn()
    .await;

    match result {
        Err(SpawnError::Init(err)) => {
            assert!(
                matches!(*err, ActorError::Panic(_)),
                "expected the panic to surface as ActorError::Panic, got: {err}"
            );
        }
        Ok(_) => panic!("spawn must fail when on_started panics"),
        Err(other) => panic!("expected SpawnError::Init, got: {other}"),
    }

    // No sleep needed here (unlike the start_timeout test above): the
    // default, no-deadline spawn ack only fires after run_actor's entire
    // teardown tail has already completed, and that tail unconditionally
    // skips on_stopped when init failed - there is nothing left pending to
    // race, so the terminal state of `stopped` is already decided.
    assert!(
        !stopped.load(Ordering::SeqCst),
        "on_stopped must NOT run after an on_started panic (self-cleaning contract)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_failure_stops_children_spawned_in_on_started() {
    let child1_stopped = Arc::new(AtomicBool::new(false));
    let child2_stopped = Arc::new(AtomicBool::new(false));
    let handles: ChildHandles = Arc::new(Mutex::new(Vec::new()));

    let supervisor = SpawnsChildrenThenFailsInit {
        child1_stopped: child1_stopped.clone(),
        child2_stopped: child2_stopped.clone(),
        handles: handles.clone(),
    };

    let result = supervisor.spawn().supervisor().await;

    // (1) The supervisor's own init failure - not a child's - must still
    // surface to the caller as SpawnError::Init.
    match result {
        Err(SpawnError::Init(err)) => {
            assert!(
                matches!(*err, ActorError::User(_)),
                "expected the original ActorError::User to survive, got: {err}"
            );
        }
        Ok(_) => panic!("spawn must fail: on_started returns Err after spawning children"),
        Err(other) => panic!("expected SpawnError::Init, got: {other}"),
    }

    // (2) Both pre-spawned children must have run on_stopped: the shared
    // teardown tail's Phase 5 (stop_all_children) stops them, it does not
    // orphan them. spawn().await does not itself return until that teardown
    // (including Phase 5) has already completed - see the module docs - so
    // this holds by construction; poll with a bounded deadline anyway rather
    // than assert synchronously, in case that ordering ever changes.
    assert!(
        wait_until(2_000, || {
            child1_stopped.load(Ordering::SeqCst) && child2_stopped.load(Ordering::SeqCst)
        })
        .await,
        "both pre-spawned children must run on_stopped when the supervisor's own init fails"
    );

    // (3) Both children's handles independently report a terminal state.
    let spawned = handles.lock().unwrap().clone();
    assert_eq!(
        spawned.len(),
        2,
        "on_started must have recorded both child handles before failing"
    );
    for child in spawned {
        tokio::time::timeout(Duration::from_secs(2), child.wait_stopped())
            .await
            .expect("each pre-spawned child must reach a terminal state");
    }
}
