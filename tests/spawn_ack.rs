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

use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorId, ActorResult, ActorStatus, ActorSystem, AskError, ChildEvent,
    RestartType, ShutdownPolicy, SpawnError, StopReason,
};

mod common;

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

/// An actor whose `pre_start` waits at a gate no test opens, so it outlasts
/// every test's start_timeout, and whose `on_stopped` records whether it
/// ever ran.
#[derive(Default)]
struct SlowInit {
    gate: Arc<HookGate>,
    stopped: Arc<AtomicBool>,
}

impl Actor for SlowInit {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.gate.pass().await;
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
        ..Default::default()
    }
    .spawn()
    .start_timeout(Duration::from_millis(50))
    .await;

    match result {
        Err(SpawnError::StartTimeout) => {}
        Ok(_) => panic!("spawn must time out: pre_start waits far past the deadline"),
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
        let result = SlowInit {
            stopped,
            ..Default::default()
        }
        .spawn()
        .named(name)
        .start_timeout(Duration::from_millis(30))
        .await;
        match result {
            Err(SpawnError::StartTimeout) => {}
            Ok(_) => {
                panic!("round {round}: spawn must time out: pre_start waits past the deadline")
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

// ---------------------------------------------------------------------------
// An awaited spawn whose actor was killed before it finished initializing
// ---------------------------------------------------------------------------

static UNIQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "spawn-ack-{base}-{}",
        UNIQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// The bound on every outcome a test awaits that is not derived from a
/// configured window.
const BOUND: Duration = Duration::from_secs(10);

/// The longest a callback waits at a [`HookGate`]: past every bound a test
/// awaits, so a gate the test never opens shows as that test's own failure.
const GATE_CAP: Duration = Duration::from_secs(15);

/// A gate a callback waits at until the test opens it. The callback says it
/// has entered, so a request the test raises after [`HookGate::entered`]
/// lands while the callback is in flight, whatever the scheduling, and the
/// callback ends only once the test opens the gate.
#[derive(Debug, Default)]
struct HookGate {
    entered: AtomicBool,
    open: AtomicBool,
    wake: tokio::sync::Notify,
}

impl HookGate {
    /// Says the callback has entered, then waits until the gate is open, for
    /// at most [`GATE_CAP`]. Returns whether the gate was opened.
    async fn pass(&self) -> bool {
        self.entered.store(true, Ordering::SeqCst);
        timeout(GATE_CAP, async {
            loop {
                let mut woken = std::pin::pin!(self.wake.notified());
                woken.as_mut().enable();
                if self.open.load(Ordering::SeqCst) {
                    return;
                }
                woken.await;
            }
        })
        .await
        .is_ok()
    }

    /// Returns once the callback has entered the gate.
    async fn entered(&self) {
        let entered = || self.entered.load(Ordering::SeqCst);
        assert!(
            wait_until(BOUND.as_millis() as u64, entered).await,
            "the callback reaches its gate"
        );
    }

    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }
}

/// Opens its gate when dropped, also while a failed assertion unwinds, so a
/// callback never waits at the gate past the test that holds it.
struct OpensOnDrop(Arc<HookGate>);

impl Drop for OpensOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// An actor whose `on_started` waits at a gate the test opens only after the
/// spawn has returned.
#[derive(Debug)]
struct SlowStart {
    gate: Arc<HookGate>,
}

impl Actor for SlowStart {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.gate.pass().await;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn awaited_spawn_killed_during_init_reports_killed() {
    common::run(2, async {
        let name = uname("killed-init");
        let id = ActorId::from(name.clone());
        let gate = Arc::new(HookGate::default());
        let _opens = OpensOnDrop(gate.clone());
        // The Kill lands once `on_started` is observed to wait at its gate,
        // which stays shut until the spawn has returned: the initialization
        // is still running when the Kill's backstop ends it.
        let killer = tokio::spawn({
            let gate = gate.clone();
            async move {
                gate.entered().await;
                ActorSystem::default().kill_by_id(&id).await
            }
        });
        let result = timeout(BOUND, SlowStart { gate }.spawn().named(name))
            .await
            .expect("the spawn returns");
        assert!(matches!(result, Err(SpawnError::Killed)), "{result:?}");
        assert!(killer.await.unwrap().is_some(), "the id was registered");
    });
}

/// An actor whose `pre_start` waits at a gate the test opens, records that
/// it returned, and whose `on_started` records that it ran.
#[derive(Debug)]
struct SlowPreStart {
    gate: Arc<HookGate>,
    returned: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
}

impl Actor for SlowPreStart {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if !self.gate.pass().await {
            return Err(ActorError::user("the test never opened the gate"));
        }
        self.returned.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A `Kill` accepted while `pre_start` runs is read on the lane once
/// `pre_start` returns, inside the 5 s kill grace: the spawn reports
/// `Killed`, `pre_start` ran to its end (the abort did not cut it), and
/// `on_started` never runs.
#[test]
fn awaited_spawn_killed_during_pre_start_reports_killed() {
    common::run(2, async {
        let name = uname("killed-pre-start");
        let gate = Arc::new(HookGate::default());
        let _opens = OpensOnDrop(gate.clone());
        let returned = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let spawning = SlowPreStart {
            gate: gate.clone(),
            returned: returned.clone(),
            started: started.clone(),
        }
        .spawn()
        .named(name.clone())
        .kill_grace(Duration::from_secs(5));
        let spawn = tokio::spawn(async move { spawning.await });
        // The Kill is accepted while `pre_start` waits at its gate; the gate
        // opens right after, well inside the grace.
        gate.entered().await;
        ActorSystem::default()
            .kill(&name)
            .await
            .expect("the kill is accepted");
        gate.open();
        let result = timeout(BOUND, spawn)
            .await
            .expect("the spawn returns")
            .expect("the spawning task ends");
        assert!(matches!(result, Err(SpawnError::Killed)), "{result:?}");
        assert!(
            returned.load(Ordering::SeqCst),
            "pre_start ran to its end: the lane read, not the abort, ended the initialization"
        );
        assert!(
            !started.load(Ordering::SeqCst),
            "on_started never ran after the kill"
        );
    });
}

#[test]
fn awaited_spawn_during_system_shutdown_reports_killed() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("shutdown")).unwrap();
        let gate = Arc::new(HookGate::default());
        let _opens = OpensOnDrop(gate.clone());
        // The shutdown begins once `on_started` is observed to wait at its
        // gate, which stays shut until the spawn has returned.
        let shutdown = tokio::spawn({
            let (sys, gate) = (sys.clone(), gate.clone());
            async move {
                gate.entered().await;
                sys.shutdown_with(ShutdownPolicy {
                    timeout: Duration::from_secs(3),
                    per_actor_timeout: Duration::from_millis(50),
                })
                .await
            }
        });
        let result = timeout(
            BOUND,
            SlowStart { gate }
                .spawn()
                .named(uname("root"))
                .on_system(&sys),
        )
        .await
        .expect("the spawn returns");
        assert!(matches!(result, Err(SpawnError::Killed)), "{result:?}");
        shutdown.await.unwrap();
    });
}

/// A message whose `Drop` panics while it is armed.
struct Bomb {
    armed: Arc<AtomicBool>,
}

impl Drop for Bomb {
    fn drop(&mut self) {
        if self.armed.load(Ordering::SeqCst) && !std::thread::panicking() {
            panic!("a queued message was dropped");
        }
    }
}

/// An actor whose `pre_start` queues a message that panics when it is
/// dropped, then fails.
#[derive(Debug)]
struct QueuesABomb;

impl Actor for QueuesABomb {
    type Message = Bomb;
    type Response = ();

    async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let armed = Arc::new(AtomicBool::new(true));
        ctx.self_handle()
            .try_notify(Bomb { armed })
            .expect("the actor's own mailbox takes the message");
        Err(ActorError::user("the initialization fails"))
    }

    async fn handle(&mut self, _msg: Bomb, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn awaited_spawn_reports_its_init_error_when_a_queued_message_panics_on_drop() {
    common::run(2, async {
        // The failure ack is sent only after the queued mail is dropped, and
        // that drop panics: the task ends without an ack, and the error the
        // initialization failed with is the spawn error. The cleanup panic
        // never replaces it, and the spawn is never `Killed`.
        let result = timeout(BOUND, QueuesABomb.spawn().named(uname("bomb")))
            .await
            .expect("the spawn returns");
        match result {
            Err(SpawnError::Init(err)) => {
                assert_eq!(*err, ActorError::user("the initialization fails"));
            }
            other => panic!("expected SpawnError::Init(the init error), got {other:?}"),
        }
        // The same through the bounded wait.
        let result = timeout(
            BOUND,
            QueuesABomb
                .spawn()
                .named(uname("bomb-timeout"))
                .start_timeout(Duration::from_secs(5)),
        )
        .await
        .expect("the spawn returns");
        match result {
            Err(SpawnError::Init(err)) => {
                assert_eq!(*err, ActorError::user("the initialization fails"));
            }
            other => panic!("expected SpawnError::Init(the init error), got {other:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// A restart attempt killed during its initialization
// ---------------------------------------------------------------------------

/// A child whose restarted instances wait in `on_started` while the probe's
/// flag is set.
struct HeldChild {
    held: Arc<AtomicBool>,
}

impl Actor for HeldChild {
    type Message = bool;
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        while self.held.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: bool, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("the child crashes on command");
    }
}

struct KillSup {
    held: Arc<AtomicBool>,
    built: Arc<std::sync::atomic::AtomicUsize>,
    child: String,
    events: Arc<Mutex<Vec<ChildEvent>>>,
}

impl Actor for KillSup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let held = self.held.clone();
        let built = self.built.clone();
        ctx.spawn_child(move || {
            if built.fetch_add(1, Ordering::SeqCst) == 0 {
                held.store(false, Ordering::SeqCst);
            } else {
                held.store(true, Ordering::SeqCst);
            }
            HeldChild { held: held.clone() }
        })
        .named(self.child.clone())
        .restart_type(RestartType::Permanent)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

#[test]
fn restart_attempt_killed_during_init_is_reported_as_kill() {
    common::run(2, async {
        let held = Arc::new(AtomicBool::new(false));
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let events = Arc::new(Mutex::new(Vec::new()));
        let child = uname("held-child");
        let sup = KillSup {
            held: held.clone(),
            built: built.clone(),
            child: child.clone(),
            events: events.clone(),
        }
        .spawn()
        .named(uname("kill-sup"))
        .supervisor()
        .await
        .expect("supervisor spawn");
        let handle = ActorSystem::default()
            .get::<HeldChild>(&child)
            .expect("the child is registered");
        let _ = handle.send(true).await;
        // The attempt's incarnation is registered under the id once the old
        // one has ended and the registry holds a live one again.
        assert!(
            wait_until(5_000, || {
                built.load(Ordering::SeqCst) == 2
                    && handle.status() == ActorStatus::Stopped
                    && ActorSystem::default()
                        .get::<HeldChild>(&child)
                        .is_some_and(|attempt| attempt.status() != ActorStatus::Stopped)
            })
            .await,
            "the restart attempt's incarnation is registered"
        );
        let report = ActorSystem::default()
            .kill_by_id(&ActorId::from(child.clone()))
            .await;
        assert!(report.is_some(), "the attempt's incarnation is registered");
        assert!(
            wait_until(5_000, || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event.reason, StopReason::Kill))
            })
            .await,
            "the attempt's end is reported as a kill: {:?}",
            events.lock().unwrap()
        );
        for event in events.lock().unwrap().iter() {
            assert!(
                !matches!(event.reason, StopReason::Failure(ActorError::User(_))),
                "never a fabricated user error: {event:?}"
            );
        }
        held.store(false, Ordering::SeqCst);
        sup.stop(StopReason::Graceful).await.unwrap();
    });
}

/// An actor whose `pre_start` waits for the test, and whose value's drop
/// holds its worker thread until the test releases it.
struct HeldDrop {
    entered: Arc<AtomicBool>,
    go: Arc<tokio::sync::Notify>,
    drop_entered: Arc<AtomicBool>,
    drop_held: Arc<AtomicBool>,
}

impl Actor for HeldDrop {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.store(true, Ordering::SeqCst);
        self.go.notified().await;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

impl Drop for HeldDrop {
    fn drop(&mut self) {
        self.drop_entered.store(true, Ordering::SeqCst);
        let held = self.drop_held.clone();
        tokio::task::block_in_place(|| {
            while held.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
    }
}

/// An awaited spawn whose initialization a `Kill` cut is answered
/// `SpawnError::Killed` once the actor's reason is final at its lane seal,
/// while the value's drop still holds: the spawn never waits through the
/// drops after the seal. Multi-thread only: the held drop blocks its worker
/// thread, which on one thread would hold the spawner too.
#[test]
fn an_awaited_spawn_a_kill_cut_returns_while_the_value_drop_still_runs() {
    common::run_multi_thread_only(4, async {
        let name = uname("held-drop");
        let entered = Arc::new(AtomicBool::new(false));
        let go = Arc::new(tokio::sync::Notify::new());
        let drop_entered = Arc::new(AtomicBool::new(false));
        let drop_held = Arc::new(AtomicBool::new(true));
        // However this test ends, the held drop is released.
        struct Release(Arc<AtomicBool>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _release = Release(drop_held.clone());
        let actor = HeldDrop {
            entered: entered.clone(),
            go: go.clone(),
            drop_entered: drop_entered.clone(),
            drop_held: drop_held.clone(),
        };
        let spawn_name = name.clone();
        let spawner = tokio::spawn(async move {
            actor
                .spawn()
                .named(spawn_name)
                .kill_grace(Duration::from_secs(30))
                .await
        });
        assert!(
            wait_until(BOUND.as_millis() as u64, || entered.load(Ordering::SeqCst)).await,
            "pre_start begins"
        );
        let handle = ActorSystem::default()
            .get::<HeldDrop>(&name)
            .expect("registered");
        handle.stop(StopReason::Kill).await.expect("accepted");
        go.notify_one();
        let result = timeout(BOUND, spawner)
            .await
            .expect("the spawn returns while the value's drop holds")
            .expect("the spawner task ends");
        assert!(
            matches!(result, Err(SpawnError::Killed)),
            "{:?}",
            result.err()
        );
        assert!(
            drop_held.load(Ordering::SeqCst),
            "the value's drop was still held when the spawn returned"
        );
        drop_held.store(false, Ordering::SeqCst);
        assert_eq!(
            timeout(BOUND, handle.wait_stopped()).await.expect("ends"),
            StopReason::Kill
        );
        assert!(drop_entered.load(Ordering::SeqCst));
    });
}

/// A queued message whose drop holds its worker thread while the test keeps
/// its flag set.
struct HeldMessage {
    entered: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
}

impl Drop for HeldMessage {
    fn drop(&mut self) {
        self.entered.store(true, Ordering::SeqCst);
        let held = self.held.clone();
        tokio::task::block_in_place(|| {
            while held.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
    }
}

/// Releases a held flag however the test ends.
struct ReleaseFlag(Arc<AtomicBool>);

impl Drop for ReleaseFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// What an [`AbortTarget`] receives.
enum AbortMsg {
    /// A turn parked for ever: the actor is aborted after its kill grace.
    Park,
    /// A request answered with `()`.
    Ask,
    Held(HeldMessage),
}

/// An actor whose `pre_start` parks until `go` while `hang_init` is set.
struct AbortTarget {
    hang_init: bool,
    entered: Arc<AtomicBool>,
    parked: Arc<AtomicBool>,
}

impl Actor for AbortTarget {
    type Message = AbortMsg;
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.store(true, Ordering::SeqCst);
        if self.hang_init {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn handle(&mut self, msg: AbortMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            AbortMsg::Park => {
                self.parked.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok(())
            }
            AbortMsg::Ask => Ok(()),
            AbortMsg::Held(message) => {
                drop(message);
                Ok(())
            }
        }
    }
}

/// A `Kill` reaches an initialization that never yields, with a message
/// queued whose drop holds: the task is aborted after its kill grace, and
/// the awaited spawn (with a start timeout, and without) returns `Killed`
/// while that drop still holds, since the reason is written before the
/// queued messages drop. Multi-thread only: the held drop blocks its thread.
#[test]
fn an_awaited_spawn_aborted_in_its_initialization_returns_before_the_queued_drops() {
    common::run_multi_thread_only(4, async {
        for start_timeout in [None, Some(Duration::from_secs(5))] {
            let name = uname("abort-init");
            let entered = Arc::new(AtomicBool::new(false));
            let held = Arc::new(AtomicBool::new(true));
            let _release = ReleaseFlag(held.clone());
            let drop_entered = Arc::new(AtomicBool::new(false));
            let actor = AbortTarget {
                hang_init: true,
                entered: entered.clone(),
                parked: Arc::new(AtomicBool::new(false)),
            };
            let spawn_name = name.clone();
            let spawner = tokio::spawn(async move {
                let mut builder = actor
                    .spawn()
                    .named(spawn_name)
                    .kill_grace(Duration::from_millis(50));
                if let Some(bound) = start_timeout {
                    builder = builder.start_timeout(bound);
                }
                builder.await
            });
            assert!(
                wait_until(BOUND.as_millis() as u64, || entered.load(Ordering::SeqCst)).await,
                "pre_start begins"
            );
            let handle = ActorSystem::default()
                .get::<AbortTarget>(&name)
                .expect("registered");
            handle
                .notify(AbortMsg::Held(HeldMessage {
                    entered: drop_entered.clone(),
                    held: held.clone(),
                }))
                .await
                .expect("queued");
            handle.stop(StopReason::Kill).await.expect("accepted");
            let result = timeout(BOUND, spawner)
                .await
                .expect("the spawn returns while the queued drop holds")
                .expect("the spawner ends");
            assert!(
                matches!(result, Err(SpawnError::Killed)),
                "{:?}",
                result.err()
            );
            assert!(held.load(Ordering::SeqCst), "the queued drop still held");
            held.store(false, Ordering::SeqCst);
            assert_eq!(
                timeout(BOUND, handle.wait_stopped()).await.expect("ends"),
                StopReason::Kill
            );
            assert!(drop_entered.load(Ordering::SeqCst));
        }
    });
}

/// A running actor parked in a turn is killed and aborted after its grace,
/// with a request queued ahead of a message whose drop holds: the request is
/// answered `ResponseDropped { reason: Kill }` while the second drop still
/// holds. Multi-thread only: the held drop blocks its thread.
#[test]
fn a_request_queued_on_the_abort_path_is_answered_before_the_next_queued_drop() {
    common::run_multi_thread_only(4, async {
        let entered = Arc::new(AtomicBool::new(false));
        let held = Arc::new(AtomicBool::new(true));
        let _release = ReleaseFlag(held.clone());
        let drop_entered = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicBool::new(false));
        let handle = AbortTarget {
            hang_init: false,
            entered,
            parked: parked.clone(),
        }
        .spawn()
        .named(uname("abort-run"))
        .kill_grace(Duration::from_millis(50))
        .await
        .expect("spawns");
        handle.notify(AbortMsg::Park).await.expect("queued");
        assert!(
            wait_until(BOUND.as_millis() as u64, || parked.load(Ordering::SeqCst)).await,
            "the turn parks"
        );
        let asker = handle.clone();
        let request = tokio::spawn(async move { asker.send(AbortMsg::Ask).await });
        let deadline = tokio::time::Instant::now() + BOUND;
        while handle.mailbox_len() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the request is queued"
            );
            sleep(Duration::from_millis(1)).await;
        }
        handle
            .notify(AbortMsg::Held(HeldMessage {
                entered: drop_entered.clone(),
                held: held.clone(),
            }))
            .await
            .expect("queued");
        handle.stop(StopReason::Kill).await.expect("accepted");
        let answer = timeout(BOUND, request)
            .await
            .expect("the request is answered while the second drop holds")
            .expect("the requester ends");
        assert!(
            matches!(
                answer,
                Err(AskError::ResponseDropped {
                    reason: StopReason::Kill
                })
            ),
            "{answer:?}"
        );
        assert!(
            held.load(Ordering::SeqCst),
            "the second queued drop still held"
        );
        held.store(false, Ordering::SeqCst);
        assert_eq!(
            timeout(BOUND, handle.wait_stopped()).await.expect("ends"),
            StopReason::Kill
        );
        assert!(drop_entered.load(Ordering::SeqCst));
    });
}
