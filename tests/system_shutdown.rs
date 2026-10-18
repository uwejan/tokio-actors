//! Behavioral suite for the system shutdown cascade:
//! roots-only sequential cascade, reverse registration order, the per-root
//! escalation ladder, the global deadline, and shutdown-time registration
//! rejection.
//!
//! - `ActorSystem::shutdown`/`shutdown_with` signal ONLY root actors (spawned
//!   via the top-level `SpawnBuilder`); a supervised child is taken down by
//!   its own supervisor's cascade, never restarted, since the parent's
//!   message loop (the only thing that would evaluate a restart) has already
//!   exited by the time its children are stopped.
//! - Roots are stopped SEQUENTIALLY, in reverse registration order, each
//!   escalating `ParentRequest` -> `Kill` -> `abort()` as needed.
//! - A breached global deadline hands every remaining root to a concurrent
//!   force-stop sweep instead of continuing one root at a time.
//! - Every system fixture in this file is created fresh via `ActorSystem::create`
//!   (a UUID-suffixed name): the registry is process-global, and shutdown's new
//!   `shutting_down` flag is never reset, so reusing `ActorSystem::default()`
//!   here would permanently affect other tests in this file.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;

use tokio::sync::{oneshot, Notify};
use tokio::time::Instant;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorId, ActorResult, ActorStatus, ActorSystem,
    ChildEvent, RestartType, Shutdown, ShutdownPolicy, ShutdownReport, SpawnError, StopOutcome,
    StopReason, StopReport, SupervisionAction, SystemConfig,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The root outcomes of a report, without the reasons.
fn outcomes_of(report: &ShutdownReport) -> Vec<(ActorId, StopOutcome)> {
    report
        .outcomes
        .iter()
        .map(|(id, stop)| (id.clone(), stop.outcome))
        .collect()
}

fn uname(base: &str) -> String {
    format!("shutdown-{base}-{}", uuid::Uuid::new_v4())
}

/// The kill grace every actor in this file has (the crate's default).
const ASSUMED_KILL_GRACE: Duration = Duration::from_millis(120);

/// The time an aborted task is given to end (the crate's settle time).
const ASSUMED_ABORT_SETTLE: Duration = Duration::from_millis(100);

/// How far past its exact timing an outcome may land on a real-time runtime
/// before the test fails: a hang bound, loose enough for a host that stalls
/// its threads. The exact timing is pinned on the paused clock, where an
/// outcome lands on its instant.
const HANG_MARGIN: Duration = Duration::from_secs(10);

/// How far past its exact timing an outcome may land on a real-time runtime
/// in a test whose failing outcome waits for a turn held until the test
/// releases it: loose enough for a host that stalls its threads, and far
/// short of the held turn.
const STALL_SLACK: Duration = Duration::from_secs(2);

/// The upper bound of an outcome that lands within `precise` on the paused
/// clock.
fn upper(paused: bool, precise: Duration) -> Duration {
    if paused {
        precise
    } else {
        precise + HANG_MARGIN
    }
}

/// Polls a synchronous predicate until it holds or the deadline passes.
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
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

/// A trivial, always-responsive actor.
#[derive(Default)]
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A supervised child that bumps a shared counter every time it starts
/// (initial start AND any restart) - the "zero restarts" probe for test 9.
struct CountingWorker {
    starts: Arc<AtomicUsize>,
}

impl Actor for CountingWorker {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A root supervisor that spawns two anonymous Permanent `CountingWorker`
/// children on `on_started`.
struct RootSupervisor {
    child_a_starts: Arc<AtomicUsize>,
    child_b_starts: Arc<AtomicUsize>,
}

impl Actor for RootSupervisor {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let a = self.child_a_starts.clone();
        ctx.spawn_child(move || CountingWorker { starts: a.clone() })
            .restart_type(RestartType::Permanent)
            .await?;
        let b = self.child_b_starts.clone();
        ctx.spawn_child(move || CountingWorker { starts: b.clone() })
            .restart_type(RestartType::Permanent)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Records its own tag into a shared log when `on_stopped` runs - the
/// reverse-registration-order probe for test 10.
struct OrderActor {
    tag: &'static str,
    log: Arc<Mutex<Vec<&'static str>>>,
}

impl Actor for OrderActor {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.log.lock().unwrap().push(self.tag);
        Ok(())
    }
}

/// Always vetoes a graceful/parent-requested stop - the vetoing-root probe
/// for test 11.
#[derive(Default)]
struct VetoActor;

impl Actor for VetoActor {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }
}

/// Vetoes every stoppable stop forever, exactly like `VetoActor`, but also
/// supervises one plain child - the vetoing-root-with-a-child probe for the
/// escalated-Kill cascade test.
#[derive(Default)]
struct VetoingSupervisorRoot;

impl Actor for VetoingSupervisorRoot {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(Idle::default)
            .named("veto-root-child")
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }
}

/// Signals `entered` and then hangs at an `.await` forever once it starts
/// processing `Hang` - deaf to both `ParentRequest` and `Kill` (both travel
/// through the very run loop this actor never returns control to), so only a
/// task abort can end it. The hung-root probe for tests 12 and 13.
#[derive(Default)]
struct HungActor {
    entered: Arc<AtomicBool>,
}

enum HungMsg {
    Hang,
}

impl Actor for HungActor {
    type Message = HungMsg;
    type Response = ();

    async fn handle(&mut self, msg: HungMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            HungMsg::Hang => {
                self.entered.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                unreachable!("pending() never resolves")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test 9: roots-only, zero child restarts
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_is_roots_only_with_zero_child_restarts() {
    let sys = ActorSystem::create(uname("roots-only")).unwrap();

    let child_a_starts = Arc::new(AtomicUsize::new(0));
    let child_b_starts = Arc::new(AtomicUsize::new(0));

    let sup = RootSupervisor {
        child_a_starts: child_a_starts.clone(),
        child_b_starts: child_b_starts.clone(),
    }
    .spawn()
    .named("root-sup")
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();
    let root_id = sup.id().clone();

    // spawn_child only confirms the SPAWN, not the child's own on_started
    // (detached by design) - wait for both children to actually report their
    // first start so "zero restarts" has an unambiguous starting point.
    assert!(
        wait_until(2_000, || {
            child_a_starts.load(Ordering::SeqCst) == 1 && child_b_starts.load(Ordering::SeqCst) == 1
        })
        .await,
        "both children must start before shutdown begins"
    );

    let report = tokio::time::timeout(Duration::from_secs(5), sys.shutdown())
        .await
        .expect("shutdown must not hang");

    assert_eq!(
        outcomes_of(&report),
        vec![(root_id, StopOutcome::Graceful)],
        "the report must contain only the root's outcome, no children"
    );
    assert!(
        report.swept.is_empty(),
        "a sweep-free shutdown must report an empty swept list"
    );
    assert_eq!(
        child_a_starts.load(Ordering::SeqCst),
        1,
        "child A must NOT have been restarted during shutdown"
    );
    assert_eq!(
        child_b_starts.load(Ordering::SeqCst),
        1,
        "child B must NOT have been restarted during shutdown"
    );
}

// ---------------------------------------------------------------------------
// Test 10: reverse registration order
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_stops_roots_in_reverse_registration_order() {
    let sys = ActorSystem::create(uname("reverse-order")).unwrap();
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    OrderActor {
        tag: "A",
        log: log.clone(),
    }
    .spawn()
    .named("order-a")
    .on_system(&sys)
    .await
    .unwrap();
    OrderActor {
        tag: "B",
        log: log.clone(),
    }
    .spawn()
    .named("order-b")
    .on_system(&sys)
    .await
    .unwrap();

    let report = tokio::time::timeout(Duration::from_secs(5), sys.shutdown())
        .await
        .expect("shutdown must not hang");

    assert_eq!(
        *log.lock().unwrap(),
        vec!["B", "A"],
        "the most-recently-registered root (B) must stop first"
    );
    assert!(
        !report.outcomes.is_empty(),
        "a normal multi-root shutdown must report non-empty outcomes"
    );
    assert!(
        report.swept.is_empty(),
        "a normal shutdown with no leftovers must report an empty sweep"
    );
}

// ---------------------------------------------------------------------------
// Test 11: vetoing root escalates to Killed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn vetoing_root_escalates_to_killed() {
    let sys = ActorSystem::create(uname("veto")).unwrap();
    let handle = VetoActor
        .spawn()
        .named("veto-root")
        .on_system(&sys)
        .await
        .unwrap();
    let root_id = handle.id().clone();

    let per_actor_timeout = Duration::from_millis(150);
    let started = Instant::now();
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_secs(5),
            per_actor_timeout,
        }),
    )
    .await
    .expect("shutdown must not hang");
    let elapsed = started.elapsed();

    assert_eq!(
        outcomes_of(&report),
        vec![(root_id, StopOutcome::Killed)],
        "a root that always vetoes ParentRequest must be escalated to Kill"
    );
    assert!(
        report.swept.is_empty(),
        "a sweep-free shutdown must report an empty swept list"
    );
    assert!(
        elapsed < per_actor_timeout + Duration::from_secs(3),
        "escalation to Kill must complete within per_actor_timeout + grace \
         (plus CI scheduling slack), took {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn vetoing_root_with_a_child_cascades_the_escalated_kill() {
    let sys = ActorSystem::create(uname("veto-cascade")).unwrap();
    let handle = VetoingSupervisorRoot
        .spawn()
        .named("veto-cascade-root")
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
    let root_id = handle.id().clone();

    assert!(
        wait_until(2_000, || sys.get::<Idle>("veto-root-child").is_some()).await,
        "the child must be registered before shutdown begins"
    );

    let report = tokio::time::timeout(
        Duration::from_secs(5),
        sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_secs(5),
            per_actor_timeout: Duration::from_millis(150),
        }),
    )
    .await
    .expect("shutdown must not hang");

    assert_eq!(
        outcomes_of(&report),
        vec![(root_id, StopOutcome::Killed)],
        "a root that always vetoes ParentRequest must be escalated to Kill"
    );

    // The Kill that finally stops the root is an ESCALATION from the
    // system's own shutdown ladder, not a directly configured one, but it
    // must cascade to the supervised child exactly the same way: the root
    // drops its own supervision state instead of awaiting the child.
    assert!(
        wait_until(2_000, || sys.get::<Idle>("veto-root-child").is_none()).await,
        "the supervised child must be cascaded and torn down after the root's escalated Kill"
    );
}

// ---------------------------------------------------------------------------
// Test 12: root hung at an await is Aborted
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn root_hung_at_await_is_aborted() {
    let sys = ActorSystem::create(uname("hang")).unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let handle = HungActor {
        entered: entered.clone(),
    }
    .spawn()
    .named("hung-root")
    .on_system(&sys)
    .await
    .unwrap();
    let root_id = handle.id().clone();

    handle.notify(HungMsg::Hang).await.unwrap();
    assert!(
        wait_until(2_000, || entered.load(Ordering::SeqCst)).await,
        "actor must have entered the hang before shutdown starts"
    );

    let report = tokio::time::timeout(
        Duration::from_secs(5),
        sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_secs(5),
            per_actor_timeout: Duration::from_millis(150),
        }),
    )
    .await
    .expect("shutdown must not hang");

    // Neither ParentRequest nor Kill is ever observed (both travel through
    // the run loop the actor never returns control to): only the abort()
    // backstop can end it.
    assert_eq!(outcomes_of(&report), vec![(root_id, StopOutcome::Aborted)]);
    assert!(
        report.swept.is_empty(),
        "a sweep-free shutdown must report an empty swept list"
    );
}

// ---------------------------------------------------------------------------
// Test 13: global deadline bounds total shutdown time
// ---------------------------------------------------------------------------

async fn global_deadline_bounds_total_shutdown_time(paused: bool) {
    let sys = ActorSystem::create(uname("deadline")).unwrap();

    const N: usize = 3;
    let mut ids = Vec::with_capacity(N);
    for i in 0..N {
        let entered = Arc::new(AtomicBool::new(false));
        let handle = HungActor {
            entered: entered.clone(),
        }
        .spawn()
        .named(format!("hung-root-{i}"))
        .on_system(&sys)
        .await
        .unwrap();
        handle.notify(HungMsg::Hang).await.unwrap();
        assert!(wait_until(2_000, || entered.load(Ordering::SeqCst)).await);
        ids.push(handle.id().clone());
    }

    // per_actor_timeout is deliberately far larger than the global timeout:
    // even the FIRST root cannot finish a full sequential ladder before the
    // deadline, so every root ends up in the concurrent force sweep.
    let policy = ShutdownPolicy {
        timeout: Duration::from_millis(80),
        per_actor_timeout: Duration::from_millis(500),
    };

    let global = policy.timeout;
    let started = Instant::now();
    let report = tokio::time::timeout(Duration::from_secs(5), sys.shutdown_with(policy))
        .await
        .expect("shutdown must not hang");
    let elapsed = started.elapsed();

    // A deadline-blind sequential ladder takes N * (per_actor_timeout +
    // kill grace) here, about 1.86 s: each hung root ends at its abort. The
    // deadline-aware implementation returns at the global timeout plus one
    // concurrent Kill, kill grace and abort, about 200 ms. The paused clock
    // holds the bound of that path, the timeout, the kill grace and the
    // settle time with a margin, which the sequential ladder exceeds by
    // more than a second; the real-time variant keeps a hang bound.
    let precise = global + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_millis(300);
    assert!(
        elapsed < upper(paused, precise),
        "shutdown must return close to the global deadline, took {elapsed:?}"
    );
    assert_eq!(
        report.outcomes.len(),
        N,
        "every root must be reported exactly once"
    );
    for id in &ids {
        let outcome = report
            .outcomes
            .iter()
            .find(|(rid, _)| rid == id)
            .map(|(_, report)| report.outcome);
        assert!(
            matches!(
                outcome,
                Some(StopOutcome::Killed)
                    | Some(StopOutcome::Aborted)
                    | Some(StopOutcome::Unresponsive)
            ),
            "straggler {id:?} must be reported Killed/Aborted/Unresponsive, got {outcome:?}"
        );
    }
    assert!(
        report.swept.is_empty(),
        "deadline stragglers are roots and belong in outcomes, not swept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn global_deadline_bounds_total_shutdown_time_multi_thread() {
    global_deadline_bounds_total_shutdown_time(false).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn global_deadline_bounds_total_shutdown_time_current_thread() {
    global_deadline_bounds_total_shutdown_time(true).await;
}

// ---------------------------------------------------------------------------
// Test: wait_stopped-then-respawn under the same name always succeeds
// ---------------------------------------------------------------------------

// The registry guard must free the name strictly BEFORE the terminal
// `Stopped` status write that `wait_stopped()` observes: once `wait_stopped`
// resolves, an immediate same-name spawn must never race the dying task and
// see `NameTaken`. Looped, since a single pass could pass by luck even with
// the ordering reversed.
#[tokio::test(flavor = "multi_thread")]
async fn wait_stopped_then_respawn_same_name_succeeds_in_a_loop() {
    let sys = ActorSystem::create(uname("respawn-loop")).unwrap();

    for round in 1..=50 {
        let handle = Idle
            .spawn()
            .named("respawn-slot")
            .on_system(&sys)
            .await
            .unwrap_or_else(|e| panic!("round {round}: spawn failed: {e}"));
        handle
            .stop(StopReason::Graceful)
            .await
            .unwrap_or_else(|e| panic!("round {round}: stop failed: {e}"));
        handle.wait_stopped().await;
    }
}

// ---------------------------------------------------------------------------
// Test 14: registration after shutdown begins is rejected
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn registration_after_shutdown_begins_is_rejected() {
    let sys = ActorSystem::create(uname("reject")).unwrap();

    let report = tokio::time::timeout(Duration::from_secs(5), sys.shutdown())
        .await
        .expect("shutdown of an empty system must not hang");
    assert!(report.outcomes.is_empty());
    assert!(report.swept.is_empty());

    let result = Idle.spawn().named("too-late").on_system(&sys).await;
    match result {
        Err(SpawnError::SystemShuttingDown(_)) => {}
        Ok(_) => panic!("registration must fail once shutdown has begun"),
        Err(other) => panic!("expected SpawnError::SystemShuttingDown, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Test: a root registered concurrently with shutdown is still reported in
// `outcomes`, never `swept`
// ---------------------------------------------------------------------------

/// Six tasks spawn detached, anonymous roots in a tight loop, from before
/// `shutdown()` begins until it has returned, on the same system. Every root
/// a spawn registered is reported in `outcomes`, never force-killed by the
/// leftover sweep and reported as a non-root `swept` entry; a spawn the
/// shutdown overtakes is refused with `SystemShuttingDown`, and some spawn is
/// refused while the shutdown runs. A registration and the shutdown's phase
/// change are atomic with respect to each other, so no root registers after
/// the root snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn roots_registered_during_shutdown_are_reported_in_outcomes() {
    let sys = ActorSystem::create(uname("late-roots")).unwrap();

    const SPAWNERS: usize = 6;

    let spawned_ids: Arc<Mutex<Vec<ActorId>>> = Arc::new(Mutex::new(Vec::new()));
    let shutdown_returned = Arc::new(AtomicBool::new(false));
    let refused_while_running = Arc::new(AtomicUsize::new(0));

    let mut spawn_tasks = Vec::with_capacity(SPAWNERS);
    for _ in 0..SPAWNERS {
        let sys = sys.clone();
        let spawned_ids = spawned_ids.clone();
        let (returned, refused) = (shutdown_returned.clone(), refused_while_running.clone());
        spawn_tasks.push(tokio::spawn(async move {
            while !returned.load(Ordering::SeqCst) {
                match Idle.spawn().on_system(&sys).detached().await {
                    Ok(handle) => spawned_ids.lock().unwrap().push(handle.id().clone()),
                    Err(SpawnError::SystemShuttingDown(_)) => {
                        if !returned.load(Ordering::SeqCst) {
                            refused.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    Err(other) => panic!("unexpected spawn error: {other}"),
                }
                tokio::task::yield_now().await;
            }
        }));
    }

    // Some spawns land before the shutdown begins; the rest race it.
    let landed = spawned_ids.clone();
    assert!(wait_until(5_000, move || landed.lock().unwrap().len() >= SPAWNERS).await);

    let report = tokio::time::timeout(Duration::from_secs(30), sys.shutdown())
        .await
        .expect("shutdown must not hang");
    shutdown_returned.store(true, Ordering::SeqCst);

    for task in spawn_tasks {
        task.await.expect("a spawner task must not panic");
    }

    assert!(
        refused_while_running.load(Ordering::SeqCst) > 0,
        "a spawn raced the running shutdown and was refused"
    );
    let spawned_ids = spawned_ids.lock().unwrap();
    let spawned_set: HashSet<ActorId> = spawned_ids.iter().cloned().collect();

    for (id, outcome) in &report.swept {
        assert!(
            !spawned_set.contains(id),
            "a root spawned during shutdown must never be reported in `swept`: \
             {id:?} ({outcome:?})"
        );
    }

    let outcome_ids: HashSet<ActorId> = report.outcomes.iter().map(|(id, _)| id.clone()).collect();
    for id in spawned_set.iter() {
        assert!(
            outcome_ids.contains(id),
            "every successfully spawned root must be reported in `outcomes`: {id:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// A root that left the registry before it published `Stopped`
// ---------------------------------------------------------------------------
//
// An actor frees its registry entry before it publishes `Stopped`, and drop
// glue can run in between (here, a queued message whose `Drop` holds its
// thread). Such a root lives on a runtime of its own, so holding its thread
// holds nothing the shutdown needs.

/// Set when dropped.
struct DropSignal(Arc<AtomicBool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Sets `entered` when dropped, then holds the dropping thread until `open`
/// is set (for at most ten seconds).
struct HeldDrop {
    entered: Arc<AtomicBool>,
    open: Arc<AtomicBool>,
}

impl Drop for HeldDrop {
    fn drop(&mut self) {
        self.entered.store(true, Ordering::SeqCst);
        let bound = std::time::Instant::now() + Duration::from_secs(10);
        while !self.open.load(Ordering::SeqCst) && std::time::Instant::now() < bound {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Queued behind a root's stop and never handled: dropped with the mailbox.
enum Ballast {
    Held(#[allow(dead_code)] HeldDrop),
}

/// A root whose `pre_stop` waits for the test before it accepts, so the test
/// can queue a message behind the stop.
struct Lingering {
    in_pre_stop: Arc<AtomicBool>,
    accept: Arc<AtomicBool>,
    stopped_with: Arc<Mutex<Option<StopReason>>>,
}

impl Actor for Lingering {
    type Message = Ballast;
    type Response = ();

    async fn handle(&mut self, _msg: Ballast, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.in_pre_stop.store(true, Ordering::SeqCst);
        while !self.accept.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        true
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.stopped_with.lock().unwrap() = Some(reason.clone());
        Ok(())
    }
}

enum ParkMsg {
    Park(Arc<AtomicBool>),
    Signal(#[allow(dead_code)] DropSignal),
}

/// Parks in `handle` on `Park`; a `Signal` queued behind it is dropped only
/// when the task is aborted.
struct Parked;

impl Actor for Parked {
    type Message = ParkMsg;
    type Response = ();

    async fn handle(&mut self, msg: ParkMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let ParkMsg::Park(parked) = msg {
            parked.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

/// A `Lingering` root on its own runtime, and the flags that drive it.
struct WindowRoot {
    handle: tokio_actors::ActorHandle<Lingering>,
    in_pre_stop: Arc<AtomicBool>,
    accept: Arc<AtomicBool>,
    entered: Arc<AtomicBool>,
    open: Arc<AtomicBool>,
    stopped_with: Arc<Mutex<Option<StopReason>>>,
}

impl WindowRoot {
    async fn spawn(
        sys: &Arc<ActorSystem>,
        on: &tokio::runtime::Handle,
        grace: Duration,
        open: Arc<AtomicBool>,
    ) -> WindowRoot {
        let (in_pre_stop, accept, stopped_with) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
        );
        let actor = Lingering {
            in_pre_stop: in_pre_stop.clone(),
            accept: accept.clone(),
            stopped_with: stopped_with.clone(),
        };
        let sys = sys.clone();
        let handle = on
            .spawn(async move {
                actor
                    .spawn()
                    .named(uname("window"))
                    .on_system(&sys)
                    .kill_grace(grace)
                    .await
                    .unwrap()
            })
            .await
            .unwrap();
        WindowRoot {
            handle,
            in_pre_stop,
            accept,
            entered: Arc::new(AtomicBool::new(false)),
            open,
            stopped_with,
        }
    }

    /// Once the shutdown's stop request reached the root: queues a message
    /// whose drop holds the root's teardown after its registry entry is
    /// freed, lets `pre_stop` accept, and waits until the drop holds it.
    /// Panics unless that happened before `deadline`, the instant the
    /// shutdown's global timeout cuts its ladder.
    async fn hold_in_the_window(&self, deadline: Instant) {
        assert!(
            wait_until(10_000, || self.in_pre_stop.load(Ordering::SeqCst)).await,
            "the shutdown's stop request reaches the root"
        );
        self.handle
            .notify(Ballast::Held(HeldDrop {
                entered: self.entered.clone(),
                open: self.open.clone(),
            }))
            .await
            .unwrap();
        self.accept.store(true, Ordering::SeqCst);
        assert!(
            wait_until(10_000, || self.entered.load(Ordering::SeqCst)).await,
            "the root's teardown reaches the queued message's drop"
        );
        assert!(
            Instant::now() < deadline,
            "the root left the registry before the shutdown's ladder was cut"
        );
        assert!(!self.handle.is_alive());
        assert_eq!(self.handle.status(), tokio_actors::ActorStatus::Stopping);
    }
}

/// The global timeout cuts the ladder while the root is between freeing its
/// registry entry and publishing `Stopped`, and it publishes `Stopped` itself
/// only after the sweep has looked (the sweep's `Kill` of the next root
/// aborts that root, whose dropped mailbox releases the first). The sweep
/// waits for it: `Graceful`, the outcome the root reached by itself.
#[test]
fn a_root_gone_before_it_published_stopped_is_reported_as_it_ends() {
    let actors = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let on = actors.handle().clone();
    common::run(2, async move {
        let sys = ActorSystem::create(uname("gone-graceful")).unwrap();
        let open = Arc::new(AtomicBool::new(false));
        // Registered first, so the ladder reaches it last and the sweep looks
        // at it after the window root.
        let parked = Arc::new(AtomicBool::new(false));
        let releaser = Parked
            .spawn()
            .named(uname("releaser"))
            .on_system(&sys)
            .kill_grace(Duration::ZERO)
            .await
            .unwrap();
        releaser
            .notify(ParkMsg::Park(parked.clone()))
            .await
            .unwrap();
        releaser
            .notify(ParkMsg::Signal(DropSignal(open.clone())))
            .await
            .unwrap();
        assert!(wait_until(10_000, || parked.load(Ordering::SeqCst)).await);
        let root = WindowRoot::spawn(&sys, &on, Duration::from_secs(5), open.clone()).await;

        let timeout = Duration::from_millis(1000);
        let started = Instant::now();
        let shutdown = {
            let sys = sys.clone();
            tokio::spawn(async move {
                sys.shutdown_with(ShutdownPolicy {
                    timeout,
                    per_actor_timeout: Duration::from_secs(30),
                })
                .await
            })
        };
        root.hold_in_the_window(started + timeout).await;
        let report = tokio::time::timeout(Duration::from_secs(10), shutdown)
            .await
            .expect("the shutdown returns")
            .unwrap();
        assert!(
            open.load(Ordering::SeqCst),
            "the next root's abort released the first"
        );
        let outcome = |id: &ActorId| {
            report
                .outcomes
                .iter()
                .find(|(reported, _)| reported == id)
                .map(|(_, report)| report.outcome)
        };
        assert_eq!(
            outcome(root.handle.id()),
            Some(StopOutcome::Graceful),
            "the root ran on_stopped and published Stopped itself: {report:?}"
        );
        assert_eq!(outcome(releaser.id()), Some(StopOutcome::Aborted));
        assert!(matches!(
            *root.stopped_with.lock().unwrap(),
            Some(StopReason::ParentRequest)
        ));
        assert_eq!(root.handle.status(), tokio_actors::ActorStatus::Stopped);
    });
    actors.shutdown_background();
}

/// One condition (a root that left the registry and does not publish
/// `Stopped` within its kill grace and the wait after an abort) gets one
/// verdict whether the global timeout sends it to the sweep or the sequential
/// ladder runs out its steps.
#[test]
fn a_root_gone_before_it_published_stopped_gets_one_verdict_on_both_paths() {
    let actors = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let on = actors.handle().clone();
    common::run(2, async move {
        let grace = Duration::from_millis(50);
        let mut verdicts = Vec::new();
        for (timeout, per_actor) in [
            // The global timeout cuts the ladder; the sweep finds it gone.
            (Duration::from_millis(1000), Duration::from_secs(30)),
            // No cut: step 1, then the Kill step and the abort step find it
            // gone.
            (Duration::from_secs(30), Duration::from_millis(1000)),
        ] {
            let sys = ActorSystem::create(uname("gone-one-verdict")).unwrap();
            let open = Arc::new(AtomicBool::new(false));
            let root = WindowRoot::spawn(&sys, &on, grace, open.clone()).await;
            let started = Instant::now();
            let shutdown = {
                let sys = sys.clone();
                tokio::spawn(async move {
                    sys.shutdown_with(ShutdownPolicy {
                        timeout,
                        per_actor_timeout: per_actor,
                    })
                    .await
                })
            };
            root.hold_in_the_window(started + timeout.min(per_actor))
                .await;
            let report = tokio::time::timeout(Duration::from_secs(10), shutdown)
                .await
                .expect("the shutdown returns")
                .unwrap();
            // Released only now: the root publishes `Stopped` itself.
            open.store(true, Ordering::SeqCst);
            tokio::time::timeout(Duration::from_secs(10), root.handle.wait_stopped())
                .await
                .expect("the root publishes Stopped");
            assert_eq!(root.handle.status(), tokio_actors::ActorStatus::Stopped);
            verdicts.push(report.outcomes);
        }
        let only = |outcomes: &[(ActorId, StopReport)]| {
            assert_eq!(outcomes.len(), 1, "{outcomes:?}");
            outcomes[0].1.outcome
        };
        let (cut, sequential) = (only(&verdicts[0]), only(&verdicts[1]));
        assert_eq!(cut, sequential, "one condition, one verdict on both paths");
        assert_eq!(cut, StopOutcome::Unresponsive);
    });
    actors.shutdown_background();
}

/// A root in its own kill grace, out of the registry and not yet `Stopped`,
/// read by `stop_within` and by the shutdown's sequential ladder. Both wait
/// the root's own grace (read while its entry still held it) and the wait
/// after an abort, and both give the one verdict for it: `Unresponsive`, the
/// drop being held until each call has returned.
#[test]
fn stop_within_and_the_shutdown_wait_the_root_s_own_grace_after_its_entry_is_gone() {
    let actors = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let on = actors.handle().clone();
    common::run(2, async move {
        // Longer than the default grace, so the wait read is the root's own.
        let grace = Duration::from_millis(600);
        let settle = Duration::from_millis(100);
        let window = Duration::from_millis(1000);
        let mut verdicts = Vec::new();
        for through_shutdown in [false, true] {
            let sys = ActorSystem::create(uname("own-grace")).unwrap();
            let open = Arc::new(AtomicBool::new(false));
            let root = WindowRoot::spawn(&sys, &on, grace, open.clone()).await;
            let started = Instant::now();
            let call = {
                let (sys, handle) = (sys.clone(), root.handle.clone());
                tokio::spawn(async move {
                    if through_shutdown {
                        let report = sys
                            .shutdown_with(ShutdownPolicy {
                                timeout: Duration::from_secs(30),
                                per_actor_timeout: window,
                            })
                            .await;
                        assert_eq!(report.outcomes.len(), 1, "{report:?}");
                        report.outcomes[0].1.outcome
                    } else {
                        handle
                            .stop_within(StopReason::Graceful, window)
                            .await
                            .expect("the stop is accepted")
                    }
                })
            };
            root.hold_in_the_window(started + window).await;
            let verdict = tokio::time::timeout(Duration::from_secs(10), call)
                .await
                .expect("the call returns")
                .unwrap();
            let took = started.elapsed();
            open.store(true, Ordering::SeqCst);
            tokio::time::timeout(Duration::from_secs(10), root.handle.wait_stopped())
                .await
                .expect("the root publishes Stopped");
            assert!(
                took >= window + grace + settle,
                "through_shutdown={through_shutdown}: waited the root's own grace, took {took:?}"
            );
            verdicts.push(verdict);
        }
        assert_eq!(verdicts, vec![StopOutcome::Unresponsive; 2]);
    });
    actors.shutdown_background();
}

// ---------------------------------------------------------------------------
// The leftover sweep
// ---------------------------------------------------------------------------

/// A supervisor child whose `on_stopped` takes `stop_for`, or never ends
/// (`None`), and which notes when it returned.
struct SlowToStop {
    stop_for: Option<Duration>,
    finished: Arc<AtomicBool>,
}

impl Actor for SlowToStop {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        match self.stop_for {
            Some(time) => tokio::time::sleep(time).await,
            None => std::future::pending::<()>().await,
        }
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// A root supervisor with one supervisor child, [`SlowToStop`]. With `veto`
/// it refuses every stop it may refuse, so a system shutdown kills it.
struct HoldsASlowSupervisor {
    stop_for: Option<Duration>,
    veto: bool,
    kill_grace: Option<Duration>,
    finished: Arc<AtomicBool>,
}

impl Actor for HoldsASlowSupervisor {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        !self.veto
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (stop_for, finished) = (self.stop_for, self.finished.clone());
        let mut child = ctx
            .spawn_child(move || SlowToStop {
                stop_for,
                finished: finished.clone(),
            })
            .named("slow-to-stop")
            .shutdown(Shutdown::Timeout(Duration::from_secs(10)))
            .with_config(ActorConfig::default().supervisor());
        if let Some(grace) = self.kill_grace {
            child = child.kill_grace(grace);
        }
        child.await?;
        Ok(())
    }
}

/// A fresh system holding [`HoldsASlowSupervisor`] and its child, once the
/// child is registered.
struct SlowTree {
    sys: Arc<ActorSystem>,
    finished: Arc<AtomicBool>,
    root: ActorHandle<HoldsASlowSupervisor>,
    child: ActorHandle<SlowToStop>,
}

async fn slow_tree(stop_for: Option<Duration>, veto: bool) -> SlowTree {
    slow_tree_with_grace(stop_for, veto, None).await
}

async fn slow_tree_with_grace(
    stop_for: Option<Duration>,
    veto: bool,
    kill_grace: Option<Duration>,
) -> SlowTree {
    let sys = ActorSystem::create(uname("leftover")).unwrap();
    slow_tree_on(sys, stop_for, veto, kill_grace).await
}

/// [`slow_tree_with_grace`] on a given system.
async fn slow_tree_on(
    sys: Arc<ActorSystem>,
    stop_for: Option<Duration>,
    veto: bool,
    kill_grace: Option<Duration>,
) -> SlowTree {
    let finished = Arc::new(AtomicBool::new(false));
    let root = HoldsASlowSupervisor {
        stop_for,
        veto,
        kill_grace,
        finished: finished.clone(),
    }
    .spawn()
    .named(uname("leftover-root"))
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();
    let mut found = None;
    assert!(
        wait_until(2_000, || {
            found = sys.get::<SlowToStop>("slow-to-stop");
            found.is_some()
        })
        .await
    );
    SlowTree {
        sys,
        finished,
        root,
        child: found.unwrap(),
    }
}

/// Kills the root of a tree whose child supervisor takes `stop_for` to stop
/// in order (forever with `None`), and returns the tree with the child still
/// registered.
async fn orphaned_subtree(stop_for: Option<Duration>) -> SlowTree {
    orphaned_subtree_with_grace(stop_for, None).await
}

async fn orphaned_subtree_with_grace(
    stop_for: Option<Duration>,
    kill_grace: Option<Duration>,
) -> SlowTree {
    let tree = slow_tree_with_grace(stop_for, false, kill_grace).await;
    tree.root.stop(StopReason::Kill).await.unwrap();
    tree.root.wait_stopped().await;
    // The child is an orphan now: it is stopping in order, and registered.
    assert!(tree.child.is_alive());
    tree
}

#[test]
fn leftover_sweep_waits_for_a_still_stopping_subtree() {
    common::run(0, leftover_sweep_waits_for_a_still_stopping_subtree_body());
}

async fn leftover_sweep_waits_for_a_still_stopping_subtree_body() {
    let tree = orphaned_subtree(Some(Duration::from_secs(1))).await;
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        tree.sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_secs(8),
            per_actor_timeout: Duration::from_secs(1),
        }),
    )
    .await
    .expect("shutdown must not hang");
    assert!(
        tree.finished.load(Ordering::SeqCst),
        "the sweep let the orphaned supervisor finish its orderly stop"
    );
    assert!(report.outcomes.is_empty(), "{:?}", outcomes_of(&report));
    assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
    let (id, stop) = &report.swept[0];
    assert_eq!(id, tree.child.id());
    assert_eq!(stop.outcome, StopOutcome::Graceful, "{stop:?}");
    assert_eq!(
        stop.reason,
        Some(StopReason::ParentKilled),
        "and it is reported with the reason it ended with"
    );
}

/// Blocks its thread for `dur` when dropped, recording when the drop began
/// and when it ended.
struct BlocksWhenDropped {
    dur: Duration,
    entered: Arc<Mutex<Option<Instant>>>,
    left: Arc<Mutex<Option<Instant>>>,
}

impl Drop for BlocksWhenDropped {
    fn drop(&mut self) {
        *self.entered.lock().unwrap() = Some(Instant::now());
        std::thread::sleep(self.dur);
        *self.left.lock().unwrap() = Some(Instant::now());
    }
}

/// A supervisor child that stops in order in 200 ms and whose value blocks
/// its thread when dropped, after its end is final.
struct EndsThenBlocksInDrop {
    _blocks: BlocksWhenDropped,
}

impl Actor for EndsThenBlocksInDrop {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }
}

/// A root supervisor with one [`EndsThenBlocksInDrop`] child, whose kill
/// grace (10 s) outlasts its blocking drop.
struct HoldsADropBlocker {
    drop_for: Duration,
    entered: Arc<Mutex<Option<Instant>>>,
    left: Arc<Mutex<Option<Instant>>>,
}

impl Actor for HoldsADropBlocker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (dur, entered, left) = (self.drop_for, self.entered.clone(), self.left.clone());
        ctx.spawn_child(move || EndsThenBlocksInDrop {
            _blocks: BlocksWhenDropped {
                dur,
                entered: entered.clone(),
                left: left.clone(),
            },
        })
        .named("blocks-in-drop")
        .shutdown(Shutdown::Timeout(Duration::from_secs(10)))
        .kill_grace(Duration::from_secs(10))
        .with_config(ActorConfig::default().supervisor())
        .await?;
        Ok(())
    }
}

/// A leftover whose end is final before the global deadline (its orderly
/// stop finishes 200 ms after its parent was killed, the deadline is 500 ms
/// after the shutdown began) but whose value then blocks in its drop for
/// 700 ms, so it publishes `Stopped` only after the deadline: it is reported
/// `Graceful` with the reason it ended with, by the instant its end became
/// final. The drop records when it began and ended, and a round counts only
/// when it began (after the end became final) before the deadline and ended
/// after it; a round the host's load pushed out of that order is run again,
/// up to five times.
#[test]
fn a_leftover_final_before_the_deadline_whose_drop_blocks_past_it_reads_graceful() {
    common::run(
        0,
        a_leftover_final_before_the_deadline_whose_drop_blocks_past_it_reads_graceful_body(),
    );
}

async fn a_leftover_final_before_the_deadline_whose_drop_blocks_past_it_reads_graceful_body() {
    const TIMEOUT: Duration = Duration::from_millis(500);
    for round in 1..=5 {
        let sys = ActorSystem::create(uname("drop-blocker")).unwrap();
        let (entered, left) = (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)));
        let root = HoldsADropBlocker {
            drop_for: Duration::from_millis(700),
            entered: entered.clone(),
            left: left.clone(),
        }
        .spawn()
        .named(uname("drop-blocker-root"))
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        let mut found = None;
        assert!(
            wait_until(2_000, || {
                found = sys.get::<EndsThenBlocksInDrop>("blocks-in-drop");
                found.is_some()
            })
            .await
        );
        let child = found.unwrap();
        root.stop(StopReason::Kill).await.unwrap();
        root.wait_stopped().await;
        assert!(child.is_alive(), "the child is stopping in order");
        let started = Instant::now();
        let report = tokio::time::timeout(
            Duration::from_secs(20),
            sys.shutdown_with(ShutdownPolicy {
                timeout: TIMEOUT,
                per_actor_timeout: Duration::from_millis(100),
            }),
        )
        .await
        .expect("shutdown must not hang");
        let entered = entered.lock().unwrap().expect("the drop ran");
        let left = left.lock().unwrap().expect("the drop ended");
        if entered >= started + TIMEOUT || left < started + TIMEOUT + Duration::from_millis(20) {
            // The end did not become final before the deadline, or `Stopped`
            // came before it: not the case under test.
            assert!(round < 5, "five rounds never met the case under test");
            continue;
        }
        assert!(report.outcomes.is_empty(), "{:?}", outcomes_of(&report));
        assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
        let (id, stop) = &report.swept[0];
        assert_eq!(id, child.id());
        assert_eq!(stop.outcome, StopOutcome::Graceful, "{stop:?}");
        assert_eq!(stop.reason, Some(StopReason::ParentKilled), "{stop:?}");
        return;
    }
}

/// A leftover that never ends by itself is held until the global deadline
/// and forced then: its end is timed against the policy's own timeout, from
/// an instant taken before the shutdown computed its deadline.
#[test]
fn a_leftover_that_never_ends_is_forced_at_the_global_deadline_and_not_before() {
    common::run(
        0,
        a_leftover_that_never_ends_is_forced_at_the_global_deadline_and_not_before_body(false),
    );
}

#[test]
fn a_leftover_that_never_ends_is_forced_at_the_global_deadline_and_not_before_paused() {
    common::run_paused(
        a_leftover_that_never_ends_is_forced_at_the_global_deadline_and_not_before_body(true),
    );
}

async fn a_leftover_that_never_ends_is_forced_at_the_global_deadline_and_not_before_body(
    paused: bool,
) {
    const TIMEOUT: Duration = Duration::from_millis(700);
    let tree = orphaned_subtree(None).await;
    let child = tree.child.clone();
    let started = Instant::now();
    let ended = tokio::spawn(async move {
        child.wait_stopped().await;
        started.elapsed()
    });
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        tree.sys.shutdown_with(ShutdownPolicy {
            timeout: TIMEOUT,
            per_actor_timeout: Duration::from_millis(100),
        }),
    )
    .await
    .expect("shutdown must not hang");
    let ended = tokio::time::timeout(Duration::from_secs(5), ended)
        .await
        .expect("the leftover is forced once the global deadline passes")
        .unwrap();
    assert!(
        ended >= TIMEOUT + ASSUMED_KILL_GRACE,
        "a leftover that ignores the Kill ends no earlier than the global deadline plus its \
         kill grace: it ended at {ended:?}"
    );
    assert!(
        ended < upper(paused, TIMEOUT + Duration::from_secs(2)),
        "the leftover is forced at the global deadline: it ended at {ended:?}"
    );
    assert!(!tree.finished.load(Ordering::SeqCst));
    assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
    assert_eq!(&report.swept[0].0, tree.child.id());
    assert!(
        matches!(
            report.swept[0].1.outcome,
            StopOutcome::Killed | StopOutcome::Aborted
        ),
        "{:?}",
        report.swept[0].1
    );
}

// The shutdown's ladder kills a root that refuses its request. Its
// supervisor child is raised its orderly stop only when the runtime drops
// the child's runner task, which the root aborted as it ended, so the final
// sweep can find that child still running with nothing raised on its lane.
// The sweep must still let it stop in order. Where the shutdown runs decides
// which of the two runs first, so each test below builds the runtime shape
// it names rather than taking the flavor from the environment. A shutdown
// inside a spawned task, on one worker or on four, reaches the sweep before
// the raise (on one worker in every round); in `block_on` and on a
// current_thread runtime the raise mostly or always comes first, so those
// two tests check the orderly end without claiming that interleaving.

/// One round: the child must end through its orderly stop (`on_stopped`
/// ran, `ParentKilled`) and be reported `Graceful` in `swept`. Returns a
/// description of a wrong outcome.
async fn killed_root_s_child_round() -> Option<String> {
    let tree = slow_tree(Some(Duration::from_millis(200)), true).await;
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        tree.sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_secs(5),
            per_actor_timeout: Duration::from_millis(50),
        }),
    )
    .await
    .expect("shutdown must not hang");
    let finished = tree.finished.load(Ordering::SeqCst);
    let ok = finished
        && report.swept.len() == 1
        && &report.swept[0].0 == tree.child.id()
        && report.swept[0].1.outcome == StopOutcome::Graceful
        && report.swept[0].1.reason == Some(StopReason::ParentKilled);
    (!ok).then(|| {
        format!(
            "finished={finished} swept={:?} outcomes={:?}",
            report.swept, report.outcomes
        )
    })
}

/// Runs 20 rounds on `runtime`, the shutdown inside a spawned task or
/// directly in `block_on`, and fails on any round whose child was not let
/// stop in order.
fn killed_root_s_child_rounds(runtime: tokio::runtime::Runtime, in_a_task: bool) {
    const ROUNDS: usize = 20;
    let mut wrong = Vec::new();
    for _ in 0..ROUNDS {
        let round = if in_a_task {
            runtime.block_on(async { tokio::spawn(killed_root_s_child_round()).await.unwrap() })
        } else {
            runtime.block_on(killed_root_s_child_round())
        };
        wrong.extend(round);
    }
    assert!(
        wrong.is_empty(),
        "{} of {ROUNDS} rounds cut the orderly stop short; first: {}",
        wrong.len(),
        wrong[0]
    );
}

fn multi_thread_runtime(workers: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn leftover_sweep_lets_a_killed_root_s_child_stop_in_order_shutdown_in_a_task_on_one_worker() {
    killed_root_s_child_rounds(multi_thread_runtime(1), true);
}

#[test]
fn leftover_sweep_lets_a_killed_root_s_child_stop_in_order_shutdown_in_a_task_on_four_workers() {
    killed_root_s_child_rounds(multi_thread_runtime(4), true);
}

#[test]
fn a_killed_root_s_supervisor_child_ends_in_order_when_the_shutdown_runs_in_block_on() {
    killed_root_s_child_rounds(multi_thread_runtime(4), false);
}

#[test]
fn a_killed_root_s_supervisor_child_ends_in_order_when_the_shutdown_runs_on_current_thread() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    killed_root_s_child_rounds(runtime, true);
}

/// A root supervisor with an idle worker child, which can block its thread:
/// a turn holds it for the given time or until `turn.release` is set, raises
/// `turn.entered` as it begins and `turn.left` as it ends.
struct BlockableRoot {
    turn: Arc<HeldTurn>,
}

/// What a `BlockableRoot`'s blocked turn shows and obeys.
#[derive(Default)]
struct HeldTurn {
    entered: AtomicBool,
    release: AtomicBool,
    left: AtomicBool,
}

/// Releases a `BlockableRoot`'s turn when dropped, so the turn ends however
/// the test ends.
struct ReleasesTheTurn(Arc<HeldTurn>);

impl Drop for ReleasesTheTurn {
    fn drop(&mut self) {
        self.0.release.store(true, Ordering::SeqCst);
    }
}

/// How long a `BlockableRoot`'s turn holds its thread when the test never
/// releases it: far past the global deadline, the kill grace, the settle
/// time and the slack.
const HELD_FOR: Duration = Duration::from_secs(15);

impl Actor for BlockableRoot {
    type Message = Duration;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Idle)
            .named("unasked-worker")
            .shutdown(Shutdown::Timeout(Duration::from_secs(5)))
            .await?;
        Ok(())
    }

    async fn handle(&mut self, time: Duration, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.turn.entered.store(true, Ordering::SeqCst);
        let until = std::time::Instant::now() + time;
        while !self.turn.release.load(Ordering::SeqCst) && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.turn.left.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// A shutdown whose root is unresponsive leaves its worker registered and
/// running with nothing asked to stop it: the sweep holds it until the
/// global deadline and then kills it while the root's turn still holds its
/// thread; the idle worker obeys the `Kill` at once. The turn ends only when the test releases it, after the checks,
/// so a sweep that left the worker to the root's own end would fail the
/// bound by seconds. The root blocks its thread, so the test needs real
/// parallelism.
#[test]
fn leftover_sweep_holds_a_leftover_nothing_stops_until_the_global_deadline_then_kills_it() {
    const TIMEOUT: Duration = Duration::from_millis(1_500);
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("unasked")).unwrap();
        let turn = Arc::new(HeldTurn::default());
        let releases = ReleasesTheTurn(turn.clone());
        let root = BlockableRoot { turn: turn.clone() }
            .spawn()
            .named(uname("unasked-root"))
            .on_system(&sys)
            .supervisor()
            .await
            .unwrap();
        assert!(wait_until(2_000, || sys.get::<Idle>("unasked-worker").is_some()).await);
        let worker = sys.get::<Idle>("unasked-worker").unwrap();
        root.notify(HELD_FOR).await.unwrap();
        // The root is inside its blocked turn: it cannot take the request.
        assert!(wait_until(2_000, || turn.entered.load(Ordering::SeqCst)).await);
        let started = Instant::now();
        let watched = worker.clone();
        let seen = turn.clone();
        let ended = tokio::spawn(async move {
            watched.wait_stopped().await;
            (started.elapsed(), seen.left.load(Ordering::SeqCst))
        });
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            sys.shutdown_with(ShutdownPolicy {
                timeout: TIMEOUT,
                per_actor_timeout: Duration::from_millis(200),
            }),
        )
        .await
        .expect("shutdown must not hang");
        let (ended, root_left) = tokio::time::timeout(Duration::from_secs(10), ended)
            .await
            .expect("the worker ends")
            .unwrap();
        assert!(
            ended >= TIMEOUT,
            "the sweep held the leftover until the global deadline: it ended at {ended:?}"
        );
        assert!(
            !root_left,
            "the leftover ended while the root's turn held its thread"
        );
        assert!(
            ended < TIMEOUT + STALL_SLACK,
            "the sweep killed the leftover at the global deadline: it ended at {ended:?}"
        );
        assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
        assert_eq!(&report.swept[0].0, worker.id());
        assert!(
            matches!(
                report.swept[0].1.outcome,
                StopOutcome::Killed | StopOutcome::Aborted
            ),
            "{:?}",
            report.swept[0].1
        );
        drop(releases);
    });
}

/// The global deadline's `Kill` and abort of a leftover do not depend on the
/// shutdown future: a caller that drops it before the deadline still sees
/// the leftover killed at the deadline. The future is polled once, which,
/// with no root left, takes the sweep's snapshot, and then dropped. The
/// leftover's `on_stopped` never ends, so only the deadline's abort, one
/// kill grace after its `Kill`, ends it.
#[test]
fn a_dropped_shutdown_still_kills_a_leftover_at_the_global_deadline() {
    common::run(
        0,
        a_dropped_shutdown_still_kills_a_leftover_at_the_global_deadline_body(false),
    );
}

#[test]
fn a_dropped_shutdown_still_kills_a_leftover_at_the_global_deadline_paused() {
    common::run_paused(a_dropped_shutdown_still_kills_a_leftover_at_the_global_deadline_body(true));
}

async fn a_dropped_shutdown_still_kills_a_leftover_at_the_global_deadline_body(paused: bool) {
    const TIMEOUT: Duration = Duration::from_millis(700);
    let tree = orphaned_subtree(None).await;
    let child = tree.child.clone();
    let started = Instant::now();
    let ended = tokio::spawn(async move {
        child.wait_stopped().await;
        started.elapsed()
    });
    let mut shutdown = Box::pin(tree.sys.shutdown_with(ShutdownPolicy {
        timeout: TIMEOUT,
        per_actor_timeout: Duration::from_millis(100),
    }));
    let first =
        std::future::poll_fn(|cx| std::task::Poll::Ready(shutdown.as_mut().poll(cx).is_pending()))
            .await;
    assert!(first, "the sweep waits on the leftover");
    drop(shutdown);
    let ended = tokio::time::timeout(Duration::from_secs(5), ended)
        .await
        .expect("the leftover is killed although the shutdown future was dropped")
        .unwrap();
    assert!(
        ended >= TIMEOUT + ASSUMED_KILL_GRACE,
        "the leftover, which ignores the Kill, is aborted no earlier than one kill grace \
         after the deadline: it ended at {ended:?}"
    );
    assert!(
        ended
            < upper(
                paused,
                TIMEOUT + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_secs(1)
            ),
        "the leftover ends one kill grace after the deadline: it ended at {ended:?}"
    );
    assert!(!tree.finished.load(Ordering::SeqCst));
}

/// The same for a leftover nothing asked to stop, the worker of a root that
/// is unresponsive: the caller gives up after 1 s, against a global deadline
/// of 2 s, and the worker is still killed at the deadline, which it obeys at
/// once, while the root's turn holds its thread. The turn ends only when the test releases it,
/// after the checks, so a worker left to the root's own end would fail the
/// bound by seconds. The caller's bound gives the root's ladder (a 100 ms
/// request step, the kill grace and the settle time) several times its
/// length to reach the sweep. The root blocks its thread, so the test needs
/// real parallelism.
#[test]
fn a_dropped_shutdown_still_kills_a_leftover_nothing_stops_at_the_global_deadline() {
    const TIMEOUT: Duration = Duration::from_secs(2);
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("dropped")).unwrap();
        let turn = Arc::new(HeldTurn::default());
        let releases = ReleasesTheTurn(turn.clone());
        let root = BlockableRoot { turn: turn.clone() }
            .spawn()
            .named(uname("dropped-root"))
            .on_system(&sys)
            .supervisor()
            .await
            .unwrap();
        assert!(wait_until(2_000, || sys.get::<Idle>("unasked-worker").is_some()).await);
        let worker = sys.get::<Idle>("unasked-worker").unwrap();
        root.notify(HELD_FOR).await.unwrap();
        assert!(wait_until(2_000, || turn.entered.load(Ordering::SeqCst)).await);
        let started = Instant::now();
        let watched = worker.clone();
        let seen = turn.clone();
        let ended = tokio::spawn(async move {
            watched.wait_stopped().await;
            (started.elapsed(), seen.left.load(Ordering::SeqCst))
        });
        let dropped = tokio::time::timeout(
            Duration::from_secs(1),
            sys.shutdown_with(ShutdownPolicy {
                timeout: TIMEOUT,
                per_actor_timeout: Duration::from_millis(100),
            }),
        )
        .await;
        assert!(dropped.is_err(), "the caller gave up before the deadline");
        let (ended, root_left) = tokio::time::timeout(Duration::from_secs(10), ended)
            .await
            .expect("the worker ends")
            .unwrap();
        assert!(
            ended >= TIMEOUT,
            "nothing cuts the leftover before the global deadline: it ended at {ended:?}"
        );
        assert!(
            !root_left,
            "the leftover ended while the root's turn held its thread"
        );
        assert!(
            ended < TIMEOUT + STALL_SLACK,
            "the leftover is killed at the global deadline: it ended at {ended:?}"
        );
        drop(releases);
    });
}

/// The abort the deadline schedules comes one kill grace after its `Kill`: a
/// leftover still stopping in order at the deadline whose `on_stopped`
/// finishes inside its kill grace (1 s here) completes it and ends `Killed`,
/// never aborted at the deadline.
#[test]
fn a_leftover_finishing_its_stop_inside_its_kill_grace_after_the_deadline_ends_killed() {
    common::run(
        0,
        a_leftover_finishing_its_stop_inside_its_kill_grace_after_the_deadline_ends_killed_body(),
    );
}

async fn a_leftover_finishing_its_stop_inside_its_kill_grace_after_the_deadline_ends_killed_body() {
    // `on_stopped` began when the root was killed and takes 1 s; the
    // deadline comes 600 ms after the shutdown begins, so the stop ends about
    // 400 ms into the leftover's 1 s kill grace.
    let tree =
        orphaned_subtree_with_grace(Some(Duration::from_secs(1)), Some(Duration::from_secs(1)))
            .await;
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        tree.sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_millis(600),
            per_actor_timeout: Duration::from_millis(100),
        }),
    )
    .await
    .expect("shutdown must not hang");
    assert!(
        tree.finished.load(Ordering::SeqCst),
        "the leftover's `on_stopped` ran to its end inside its kill grace: {:?}",
        report.swept
    );
    assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
    assert_eq!(&report.swept[0].0, tree.child.id());
    assert_eq!(
        report.swept[0].1.outcome,
        StopOutcome::Killed,
        "{:?}",
        report.swept[0].1
    );
}

/// A worker that refuses every stop it may refuse, so its supervisor's
/// teardown escalates it to `Kill` at the end of its window.
#[derive(Default)]
struct RefusesToStop;

impl Actor for RefusesToStop {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }
}

/// A worker whose `on_stopped` never ends.
#[derive(Default)]
struct NeverFinishesStopping;

impl Actor for NeverFinishesStopping {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// A supervisor that starts a worker whose stop never finishes, then one
/// that refuses its stop under a 200 ms window: its orderly teardown stops
/// the second first, killing it at the window, and then waits on the first
/// for ever.
struct StopsTwoWorkers;

impl Actor for StopsTwoWorkers {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(NeverFinishesStopping::default)
            .named("never-finishes")
            .shutdown(Shutdown::Infinity)
            .await?;
        ctx.spawn_child(RefusesToStop::default)
            .named("refuses")
            .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
            .await?;
        Ok(())
    }
}

/// A root whose `Infinity` supervisor child is [`StopsTwoWorkers`].
struct HoldsTwoWorkersBelow;

impl Actor for HoldsTwoWorkersBelow {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| StopsTwoWorkers)
            .named("middle")
            .shutdown(Shutdown::Infinity)
            .with_config(ActorConfig::default().supervisor())
            .await?;
        Ok(())
    }
}

/// A leftover its own supervisor killed (at the end of its 200 ms window)
/// well before the 800 ms global deadline reads `Graceful` with that `Kill`,
/// whatever order the registry lists the leftovers in: other leftovers held
/// to the deadline (its supervisor, and a worker whose stop never finishes)
/// do not change when its end was seen. Several rounds, each a fresh
/// system, so the registry's order varies.
#[test]
fn a_leftover_killed_by_its_own_supervisor_before_the_deadline_reads_graceful_in_every_order() {
    common::run(0, a_leftover_killed_by_its_own_supervisor_before_the_deadline_reads_graceful_in_every_order_body(false));
}

#[test]
fn a_leftover_killed_by_its_own_supervisor_before_the_deadline_reads_graceful_in_every_order_paused(
) {
    common::run_paused(a_leftover_killed_by_its_own_supervisor_before_the_deadline_reads_graceful_in_every_order_body(true));
}

async fn a_leftover_killed_by_its_own_supervisor_before_the_deadline_reads_graceful_in_every_order_body(
    paused: bool,
) {
    const ROUNDS: usize = 6;
    const TIMEOUT: Duration = Duration::from_millis(800);
    for round in 0..ROUNDS {
        let sys = ActorSystem::create(uname("scan-order")).unwrap();
        let root = HoldsTwoWorkersBelow
            .spawn()
            .named(uname("scan-order-root"))
            .on_system(&sys)
            .supervisor()
            .await
            .unwrap();
        assert!(wait_until(2_000, || sys.get::<RefusesToStop>("refuses").is_some()).await);
        let refuses = sys.get::<RefusesToStop>("refuses").unwrap();
        root.stop(StopReason::Kill).await.unwrap();
        root.wait_stopped().await;
        let started = Instant::now();
        let watched = refuses.clone();
        let ended = tokio::spawn(async move {
            watched.wait_stopped().await;
            started.elapsed()
        });
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            sys.shutdown_with(ShutdownPolicy {
                timeout: TIMEOUT,
                per_actor_timeout: Duration::from_millis(100),
            }),
        )
        .await
        .expect("shutdown must not hang");
        let ended = ended.await.unwrap();
        assert!(
            ended < upper(paused, TIMEOUT - Duration::from_millis(200)),
            "round {round}: the worker was killed by its supervisor well before the \
             deadline: it ended at {ended:?}"
        );
        let (_, stop) = report
            .swept
            .iter()
            .find(|(id, _)| id == refuses.id())
            .expect("the worker is swept");
        assert_eq!(
            (stop.outcome, stop.reason.clone()),
            (StopOutcome::Graceful, Some(StopReason::Kill)),
            "round {round}: {:?}",
            report.swept
        );
    }
}

/// Once a sweep has started, its deadline `Kill` and abort stand for a later
/// shutdown with a later deadline: a stop only ever strengthens. The first
/// call (500 ms) is polled once, which with no root left takes the sweep's
/// snapshot, and dropped; under the second call (5 s) the leftover, which
/// would stop in order for 1.5 s, ends no earlier than the first deadline
/// plus its kill grace and before its stop could finish, and is reported
/// `Aborted`.
#[test]
fn a_later_shutdown_keeps_the_deadline_of_a_sweep_that_started() {
    common::run(
        0,
        a_later_shutdown_keeps_the_deadline_of_a_sweep_that_started_body(false),
    );
}

#[test]
fn a_later_shutdown_keeps_the_deadline_of_a_sweep_that_started_paused() {
    common::run_paused(a_later_shutdown_keeps_the_deadline_of_a_sweep_that_started_body(true));
}

async fn a_later_shutdown_keeps_the_deadline_of_a_sweep_that_started_body(paused: bool) {
    const FIRST: Duration = Duration::from_millis(500);
    let tree = orphaned_subtree(Some(Duration::from_millis(1_500))).await;
    let child = tree.child.clone();
    let started = Instant::now();
    let ended = tokio::spawn(async move {
        child.wait_stopped().await;
        started.elapsed()
    });
    let mut first = Box::pin(tree.sys.shutdown_with(ShutdownPolicy {
        timeout: FIRST,
        per_actor_timeout: Duration::from_millis(100),
    }));
    let pending =
        std::future::poll_fn(|cx| std::task::Poll::Ready(first.as_mut().poll(cx).is_pending()))
            .await;
    assert!(pending, "the first sweep waits on the leftover");
    drop(first);
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        tree.sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_secs(5),
            per_actor_timeout: Duration::from_millis(100),
        }),
    )
    .await
    .expect("shutdown must not hang");
    let ended = ended.await.unwrap();
    assert!(
        !tree.finished.load(Ordering::SeqCst),
        "the first sweep's deadline cut the orderly stop: {:?}",
        report.swept
    );
    assert!(
        ended >= FIRST + ASSUMED_KILL_GRACE && ended < upper(paused, Duration::from_millis(1_500)),
        "the leftover ended one kill grace after the first deadline: it ended at {ended:?}"
    );
    assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
    assert_eq!(&report.swept[0].0, tree.child.id());
    assert_eq!(
        report.swept[0].1.outcome,
        StopOutcome::Aborted,
        "{:?}",
        report.swept[0].1
    );
}

/// A later shutdown reports by the deadline of a sweep that started before
/// it: the leftover, an orphan finishing a 1 s orderly stop inside its 1 s
/// kill grace, is still stopping at the first call's 600 ms deadline, so the
/// later 5 s call forces it there and reports it `Killed`, as a single
/// 600 ms shutdown would, never `Graceful` by its own later deadline.
#[test]
fn a_later_shutdown_reports_by_the_deadline_of_a_sweep_that_started() {
    common::run(0, async {
        let tree =
            orphaned_subtree_with_grace(Some(Duration::from_secs(1)), Some(Duration::from_secs(1)))
                .await;
        let mut first = Box::pin(tree.sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_millis(600),
            per_actor_timeout: Duration::from_millis(100),
        }));
        let pending =
            std::future::poll_fn(|cx| std::task::Poll::Ready(first.as_mut().poll(cx).is_pending()))
                .await;
        assert!(pending, "the first sweep waits on the leftover");
        drop(first);
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            tree.sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(5),
                per_actor_timeout: Duration::from_millis(100),
            }),
        )
        .await
        .expect("shutdown must not hang");
        assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
        assert_eq!(&report.swept[0].0, tree.child.id());
        assert_eq!(
            report.swept[0].1.outcome,
            StopOutcome::Killed,
            "{:?}",
            report.swept[0].1
        );
    });
}

/// Every leftover is reported once even when the sweep's own tasks fail: the
/// caller's runtime has no timer, so each wait and each force panics, while
/// the leftover runs on a runtime that has one. The leftover, whose wait
/// failed, goes to the force and is reported, and the abort the force armed
/// on the leftover's own runtime ends it. The test builds both runtimes
/// itself.
#[test]
fn a_leftover_is_reported_once_when_the_sweep_s_tasks_fail() {
    let actors = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let tree = actors.block_on(orphaned_subtree(None));
    let caller = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let report = caller.block_on(tree.sys.shutdown_with(ShutdownPolicy {
        timeout: Duration::from_millis(300),
        per_actor_timeout: Duration::from_millis(100),
    }));
    assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
    assert_eq!(&report.swept[0].0, tree.child.id());
    // The failed force reports from the plane, as a forced end: the
    // leftover still running then reads `Unresponsive`, and one that the
    // force's `Kill` ended first (it had not yet begun its `on_stopped`)
    // reads `Killed`; never `Graceful`.
    let stop = &report.swept[0].1;
    assert!(
        matches!(
            (stop.outcome, &stop.reason),
            (StopOutcome::Unresponsive, None) | (StopOutcome::Killed, Some(StopReason::Kill))
        ),
        "{stop:?}"
    );
    let ended = actors.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), tree.child.wait_stopped()).await
    });
    assert!(
        ended.is_ok(),
        "the abort armed on its own runtime ends the leftover"
    );
}

/// Polls `fut` once: a shutdown call with no root left reaches its final
/// sweep's wait in that poll (the sweep has folded its deadline into the
/// system's and scheduled its deadline entries), and returns `Pending`.
async fn poll_once<F: Future + Unpin>(fut: &mut F) -> bool {
    std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::pin::Pin::new(&mut *fut).poll(cx).is_pending())
    })
    .await
}

/// Two shutdown calls of different timeouts (300 ms and 5 s), started in
/// either order, report one real orphan the same way while the end of its
/// orderly stop moves around their shared deadline: the orphan's
/// `on_stopped` ends 240 ms to 360 ms after its parent was killed, so its
/// seal meets the deadline's `Kill` from the reaper and the force. Every
/// round both calls agree, and the rounds reach both sides of the deadline.
/// Multi-thread only: the two calls, the orphan and the reaper run in
/// parallel.
#[test]
fn two_shutdown_calls_report_one_real_orphan_the_same_way_around_the_deadline() {
    common::run_multi_thread_only(4, async {
        let short = ShutdownPolicy {
            timeout: Duration::from_millis(300),
            per_actor_timeout: Duration::from_millis(100),
        };
        let long = ShutdownPolicy {
            timeout: Duration::from_secs(5),
            per_actor_timeout: Duration::from_millis(100),
        };
        let mut outcomes = Vec::new();
        for long_first in [false, true] {
            for stop_ms in (240..=360).step_by(6) {
                let tree = orphaned_subtree_with_grace(
                    Some(Duration::from_millis(stop_ms)),
                    Some(ASSUMED_KILL_GRACE),
                )
                .await;
                let mut a = Box::pin(tree.sys.shutdown_with(short.clone()));
                let mut b = Box::pin(tree.sys.shutdown_with(long.clone()));
                if long_first {
                    assert!(poll_once(&mut b).await, "the long call waits in its sweep");
                } else {
                    assert!(poll_once(&mut a).await, "the short call waits in its sweep");
                }
                let (a, b) =
                    tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(a, b) })
                        .await
                        .expect("shutdown must not hang");
                assert_eq!(
                    a.swept, b.swept,
                    "long_first={long_first} stop_ms={stop_ms}: one end, one report"
                );
                assert_eq!(a.swept.len(), 1, "{:?}", a.swept);
                outcomes.push(a.swept[0].1.outcome);
            }
        }
        assert!(
            outcomes.contains(&StopOutcome::Graceful) && outcomes.contains(&StopOutcome::Killed),
            "the rounds reach both sides of the deadline: {outcomes:?}"
        );
    });
}

/// Two shutdown calls running at once report one outcome for one end: the
/// first (5 s) reaches its sweep first, the second (600 ms) lowers the
/// system's deadline while the first still waits. The orphan, finishing a
/// 1 s orderly stop inside its 1 s kill grace, is still stopping at 600 ms,
/// so both calls read it `Killed`.
#[test]
fn concurrent_shutdown_calls_report_one_outcome_for_one_end() {
    common::run(0, async {
        let tree =
            orphaned_subtree_with_grace(Some(Duration::from_secs(1)), Some(Duration::from_secs(1)))
                .await;
        let long = tree.sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_secs(5),
            per_actor_timeout: Duration::from_millis(100),
        });
        let short = tree.sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_millis(600),
            per_actor_timeout: Duration::from_millis(100),
        });
        let (long, short) =
            tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(long, short) })
                .await
                .expect("shutdown must not hang");
        for report in [&long, &short] {
            assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
            assert_eq!(&report.swept[0].0, tree.child.id());
            assert_eq!(
                report.swept[0].1.outcome,
                StopOutcome::Killed,
                "long {:?}, short {:?}",
                long.swept,
                short.swept
            );
        }
    });
}

/// A completed shutdown leaves no deadline behind: after a shutdown with a
/// leftover completes and the system is reactivated, a new orphan stopping
/// in order under a long second shutdown finishes its `on_stopped` and is
/// reported `Graceful`, never forced by the first shutdown's past deadline.
#[test]
fn a_completed_shutdown_leaves_no_deadline_after_a_reactivation() {
    common::run(0, async {
        let first = orphaned_subtree(Some(Duration::from_millis(100))).await;
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            first.sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_millis(300),
                per_actor_timeout: Duration::from_millis(100),
            }),
        )
        .await
        .expect("shutdown must not hang");
        assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
        assert!(
            first.sys.reactivate(),
            "the completed shutdown left Defunct"
        );
        let second = slow_tree_on(
            first.sys.clone(),
            Some(Duration::from_millis(500)),
            false,
            None,
        )
        .await;
        second.root.stop(StopReason::Kill).await.unwrap();
        second.root.wait_stopped().await;
        assert!(second.child.is_alive());
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            second.sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(5),
                per_actor_timeout: Duration::from_millis(100),
            }),
        )
        .await
        .expect("shutdown must not hang");
        assert!(
            second.finished.load(Ordering::SeqCst),
            "the orphan finished its orderly stop: {:?}",
            report.swept
        );
        assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
        assert_eq!(
            report.swept[0].1.outcome,
            StopOutcome::Graceful,
            "{:?}",
            report.swept[0].1
        );
    });
}

/// A root that awaits for 10 s in a turn and raises `entered` as it begins
/// it; its supervisor child stops in order in 100 ms and has a 400 ms kill
/// grace.
struct BusyRootWithAQuickChild {
    entered: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

impl Actor for BusyRootWithAQuickChild {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(10)).await;
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let finished = self.finished.clone();
        ctx.spawn_child(move || SlowToStop {
            stop_for: Some(Duration::from_millis(100)),
            finished: finished.clone(),
        })
        .named("quick-child")
        .shutdown(Shutdown::Timeout(Duration::from_secs(10)))
        .with_config(ActorConfig::default().supervisor())
        .kill_grace(Duration::from_millis(400))
        .await?;
        Ok(())
    }
}

/// A sweep that starts long after its deadline still gives a leftover its
/// kill grace after the deadline `Kill`: the busy root is forced first (its
/// 1 s kill grace runs out well past the 200 ms deadline), and only then does
/// its supervisor child begin its orderly stop, so both stages of the child's
/// deadline entry are past when it is queued. The abort follows the `Kill`
/// by the child's 400 ms kill grace from when the `Kill` fires, so the child
/// ends `Killed`, never aborted at once.
#[test]
fn a_late_sweep_still_gives_a_leftover_its_kill_grace_after_the_deadline_kill() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("late-sweep")).unwrap();
        let (entered, finished) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let root = BusyRootWithAQuickChild {
            entered: entered.clone(),
            finished: finished.clone(),
        }
        .spawn()
        .named(uname("late-sweep-root"))
        .on_system(&sys)
        .supervisor()
        .kill_grace(Duration::from_secs(1))
        .await
        .unwrap();
        assert!(wait_until(2_000, || sys.get::<SlowToStop>("quick-child").is_some()).await);
        let child = sys.get::<SlowToStop>("quick-child").unwrap();
        root.notify(()).await.unwrap();
        assert!(wait_until(2_000, || entered.load(Ordering::SeqCst)).await);
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_millis(200),
                per_actor_timeout: Duration::from_millis(100),
            }),
        )
        .await
        .expect("shutdown must not hang");
        let (_, stop) = report
            .swept
            .iter()
            .find(|(id, _)| id == child.id())
            .expect("the child is swept");
        assert_eq!(
            stop.outcome,
            StopOutcome::Killed,
            "{stop:?}; its on_stopped finished: {}",
            finished.load(Ordering::SeqCst)
        );
    });
}

/// A supervisor child whose `on_stopped` blocks its thread for 2.5 s.
struct BlocksItsThreadWhileStopping;

impl Actor for BlocksItsThreadWhileStopping {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        std::thread::sleep(Duration::from_millis(2_500));
        Ok(())
    }
}

/// A root whose supervisor child is [`BlocksItsThreadWhileStopping`].
struct HoldsAThreadBlocker;

impl Actor for HoldsAThreadBlocker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| BlocksItsThreadWhileStopping)
            .named("blocks")
            .shutdown(Shutdown::Timeout(Duration::from_secs(10)))
            .with_config(ActorConfig::default().supervisor())
            .await?;
        Ok(())
    }
}

/// A wait already running follows the system's deadline when a later call
/// lowers it: the long call (5 s) takes its snapshot first, the short one
/// (600 ms) then lowers the deadline, and the long call's wait gives up at
/// 600 ms too. The orphan blocks its thread inside `on_stopped`, so both
/// calls force it and read it `Unresponsive`, and the long call returns well
/// before its own deadline. The blocked thread needs real parallelism.
#[test]
fn a_lowered_system_deadline_shortens_a_wait_already_running() {
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("lowered")).unwrap();
        let root = HoldsAThreadBlocker
            .spawn()
            .named(uname("lowered-root"))
            .on_system(&sys)
            .supervisor()
            .await
            .unwrap();
        assert!(
            wait_until(2_000, || sys
                .get::<BlocksItsThreadWhileStopping>("blocks")
                .is_some())
            .await
        );
        root.stop(StopReason::Kill).await.unwrap();
        root.wait_stopped().await;
        let started = Instant::now();
        let long = sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_secs(5),
            per_actor_timeout: Duration::from_millis(100),
        });
        let short = sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_millis(600),
            per_actor_timeout: Duration::from_millis(100),
        });
        let (long, short) =
            tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(long, short) })
                .await
                .expect("shutdown must not hang");
        let took = started.elapsed();
        for report in [&long, &short] {
            assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
            assert_eq!(
                report.swept[0].1.outcome,
                StopOutcome::Unresponsive,
                "long {:?}, short {:?}",
                long.swept,
                short.swept
            );
        }
        assert!(
            took < Duration::from_millis(600) + HANG_MARGIN,
            "the long call gave up at the lowered deadline: both returned after {took:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// A dropped `shutdown_with` is finished by a later `shutdown`
// ---------------------------------------------------------------------------

/// Crashes on its first message.
struct Crasher;

impl Actor for Crasher {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("crashed on command"))
    }
}

/// A supervisor that spawns its one `Permanent` child on its first message
/// and records the action of every child event it receives.
struct ActionLogSupervisor {
    child: String,
    actions: Arc<Mutex<Vec<SupervisionAction>>>,
}

impl Actor for ActionLogSupervisor {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Crasher)
            .named(self.child.clone())
            .await?;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.actions.lock().unwrap().push(event.action);
        Ok(())
    }
}

/// Dropping the future of `shutdown_with` leaves the system refusing
/// registrations and supervised restarts and `reactivate` returning `false`,
/// leaves the roots the call had not reached untouched, and a later
/// `shutdown` call stops them and returns the system to service.
#[test]
fn dropped_shutdown_with_is_finished_by_a_later_shutdown() {
    common::run(
        0,
        dropped_shutdown_with_is_finished_by_a_later_shutdown_body(false),
    );
}

#[test]
fn dropped_shutdown_with_is_finished_by_a_later_shutdown_paused() {
    common::run_paused(dropped_shutdown_with_is_finished_by_a_later_shutdown_body(
        true,
    ));
}

async fn dropped_shutdown_with_is_finished_by_a_later_shutdown_body(paused: bool) {
    // The window the later `shutdown()` gives each root, read from the
    // system's own policy.
    const PER_ROOT: Duration = Duration::from_millis(400);
    let sys = ActorSystem::create_with(
        uname("dropped"),
        SystemConfig {
            shutdown_policy: ShutdownPolicy {
                timeout: Duration::from_secs(30),
                per_actor_timeout: PER_ROOT,
            },
            ..Default::default()
        },
    )
    .unwrap();

    // Registered first, so the shutdown reaches it last: a root the
    // dropped call never gets to.
    let unreached_asked = Arc::new(AtomicUsize::new(0));
    let unreached = CountingVeto {
        asked: unreached_asked.clone(),
        entered: Arc::new(Notify::new()),
    }
    .spawn()
    .named(uname("unreached"))
    .on_system(&sys)
    .await
    .unwrap();
    let unreached_id = unreached.id().clone();

    let actions = Arc::new(Mutex::new(Vec::new()));
    let child = uname("crasher");
    let supervisor = ActionLogSupervisor {
        child: child.clone(),
        actions: actions.clone(),
    }
    .spawn()
    .named(uname("supervisor"))
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), supervisor.send(()))
        .await
        .expect("the supervisor must answer")
        .unwrap();

    // Registered last, so the shutdown reaches it first. It vetoes every
    // stoppable stop and signals when it is first asked.
    let vetoing_entered = Arc::new(Notify::new());
    let vetoing = CountingVeto {
        asked: Arc::new(AtomicUsize::new(0)),
        entered: vetoing_entered.clone(),
    }
    .spawn()
    .named(uname("vetoing"))
    .on_system(&sys)
    .await
    .unwrap();
    let vetoing_id = vetoing.id().clone();

    // The first shutdown has a window far longer than the test, so
    // nothing but the drop below ends it. It is dropped as soon as the
    // vetoing root has been asked to stop.
    let mut first = Box::pin(sys.shutdown_with(ShutdownPolicy {
        timeout: Duration::from_secs(60),
        per_actor_timeout: Duration::from_secs(30),
    }));
    tokio::select! {
        biased;
        _ = vetoing_entered.notified() => {}
        _ = &mut first => panic!("the first shutdown must still be waiting on the vetoing root"),
    }
    drop(first);

    let refused = tokio::time::timeout(Duration::from_secs(10), async {
        Idle.spawn().named(uname("late")).on_system(&sys).await
    })
    .await
    .expect("a refused spawn must answer");
    assert!(
        matches!(refused, Err(SpawnError::SystemShuttingDown(_))),
        "a spawn after a dropped shutdown must be refused"
    );
    assert!(
        !sys.reactivate(),
        "a system whose shutdown never completed must not reactivate"
    );

    // A crashed child cannot be restarted: its supervisor exhausts its
    // budget and ends.
    let crasher = sys
        .get::<Crasher>(&child)
        .expect("the child must be registered");
    tokio::time::timeout(Duration::from_secs(10), crasher.notify(()))
        .await
        .expect("the crash message must be accepted")
        .unwrap();
    assert!(
        wait_until(2_000, || actions
            .lock()
            .unwrap()
            .contains(&SupervisionAction::BudgetExhausted))
        .await,
        "the refused restarts must exhaust the supervisor's budget: {:?}",
        actions.lock().unwrap()
    );
    tokio::time::timeout(Duration::from_secs(2), supervisor.wait_stopped())
        .await
        .expect("the supervisor must end once its budget is exhausted");
    assert!(
        wait_until(2_000, || !sys.actor_ids().contains(supervisor.id())).await,
        "the ended supervisor must leave the registry before the later shutdown"
    );

    // The root the dropped call never reached was not asked to stop.
    assert_eq!(
        unreached_asked.load(Ordering::SeqCst),
        0,
        "the dropped call must not go on to the roots it had not reached"
    );
    assert_eq!(unreached.status(), ActorStatus::Running);

    // A later shutdown finishes the job: it escalates both vetoing roots,
    // the one the first call was waiting on and the one it never reached,
    // and the system returns to service.
    let bound = upper(
        paused,
        (PER_ROOT + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE) * 2 + Duration::from_secs(1),
    );
    let report = tokio::time::timeout(bound, sys.shutdown())
        .await
        .expect("the later shutdown must finish within its bound");
    let reported: Vec<ActorId> = report.outcomes.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(
        reported,
        vec![vetoing_id, unreached_id],
        "the later shutdown must stop both vetoing roots, newest first"
    );
    assert!(report.swept.is_empty(), "nothing is left to sweep");
    assert!(!vetoing.is_alive(), "the vetoing root must be stopped");
    assert!(!unreached.is_alive(), "the unreached root must be stopped");
    assert!(
        unreached_asked.load(Ordering::SeqCst) >= 1,
        "the later shutdown asks the unreached root to stop"
    );
    assert!(
        sys.reactivate(),
        "a completed shutdown must leave the system reactivatable"
    );
}

// ---------------------------------------------------------------------------
// A parent window shorter than its children's stop budgets
// ---------------------------------------------------------------------------

/// A worker whose `pre_stop` sleeps for `delay` and then accepts. It records
/// when `pre_stop` begins (into `order`) and whether `on_stopped` ran.
struct SlowStopper {
    tag: &'static str,
    delay: Duration,
    order: Arc<Mutex<Vec<&'static str>>>,
    stopped: Arc<AtomicBool>,
}

impl Actor for SlowStopper {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.order.lock().unwrap().push(self.tag);
        tokio::time::sleep(self.delay).await;
        true
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

/// A root supervisor with every default whose two children keep their default
/// `Shutdown` window. Their stop budgets add up to more than the 5 seconds a
/// system shutdown gives the root, so the root is escalated to `Kill` and
/// reported as killed, and the child still stopping when it happens is taken
/// by the `Kill` its link guard raises.
struct DefaultWindowRoot {
    first: (Duration, Arc<AtomicBool>),
    second: (Duration, Arc<AtomicBool>),
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl Actor for DefaultWindowRoot {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (delay, stopped) = self.first.clone();
        let order = self.order.clone();
        ctx.spawn_child(move || SlowStopper {
            tag: "c1",
            delay,
            order: order.clone(),
            stopped: stopped.clone(),
        })
        .named("c1")
        .await?;
        let (delay, stopped) = self.second.clone();
        let order = self.order.clone();
        ctx.spawn_child(move || SlowStopper {
            tag: "c2",
            delay,
            order: order.clone(),
            stopped: stopped.clone(),
        })
        .named("c2")
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn root_supervisor_with_a_default_window_child_is_escalated_at_system_shutdown() {
    common::run(
        0,
        root_supervisor_with_a_default_window_child_is_escalated_at_system_shutdown_body(false),
    );
}

#[test]
fn root_supervisor_with_a_default_window_child_is_escalated_at_system_shutdown_paused() {
    common::run_paused(
        root_supervisor_with_a_default_window_child_is_escalated_at_system_shutdown_body(true),
    );
}

async fn root_supervisor_with_a_default_window_child_is_escalated_at_system_shutdown_body(
    paused: bool,
) {
    const SYSTEM_PER_ROOT: Duration = Duration::from_secs(5);
    let sys = ActorSystem::create(uname("default-window")).unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let c1_stopped = Arc::new(AtomicBool::new(false));
    let c2_stopped = Arc::new(AtomicBool::new(false));
    let root = DefaultWindowRoot {
        first: (Duration::from_millis(4_500), c1_stopped.clone()),
        second: (Duration::from_secs(1), c2_stopped.clone()),
        order: order.clone(),
    }
    .spawn()
    .named(uname("root"))
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();
    let root_id = root.id().clone();

    let bound = upper(
        paused,
        SYSTEM_PER_ROOT + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_secs(1),
    );
    let report = tokio::time::timeout(bound, sys.shutdown())
        .await
        .expect("the shutdown must finish within the root's window plus its kill ladder");

    assert_eq!(
        outcomes_of(&report),
        vec![(root_id, StopOutcome::Killed)],
        "the root's window is shorter than its children's budgets, so it is escalated to Kill"
    );
    assert_eq!(
        *order.lock().unwrap(),
        vec!["c2", "c1"],
        "the teardown stops the newest child first"
    );
    assert!(c2_stopped.load(Ordering::SeqCst), "c2 ran its on_stopped");
    assert!(
        !c1_stopped.load(Ordering::SeqCst),
        "c1 was still in pre_stop when the root was killed, so it never reaches on_stopped"
    );
    assert!(
        sys.actor_ids().is_empty(),
        "nothing of the tree stays registered: {:?}",
        sys.registered()
    );
}

// ---------------------------------------------------------------------------
// A dropped `stop_within` takes no further escalation step
// ---------------------------------------------------------------------------

/// Vetoes every stoppable stop, counts how many times it was asked and
/// signals `entered` each time.
struct CountingVeto {
    asked: Arc<AtomicUsize>,
    entered: Arc<Notify>,
}

impl Actor for CountingVeto {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        false
    }
}

/// A dropped `stop_within` takes no escalation step: the window it was given
/// passes with the actor still running, and the stop request it raised was
/// asked of the actor.
#[test]
fn dropped_stop_within_does_not_escalate_further() {
    common::run(0, async {
        const WINDOW: Duration = Duration::from_millis(200);
        let sys = ActorSystem::create(uname("dropped-stop-within")).unwrap();
        let asked = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let actor = CountingVeto {
            asked: asked.clone(),
            entered: entered.clone(),
        }
        .spawn()
        .named(uname("vetoing"))
        .on_system(&sys)
        .await
        .unwrap();

        // The call is dropped as soon as the actor has been asked to stop,
        // long before its window would escalate.
        let mut call = Box::pin(actor.stop_within(StopReason::Graceful, WINDOW));
        tokio::select! {
            biased;
            _ = entered.notified() => {}
            _ = &mut call => panic!("the call must still be waiting on the vetoing actor"),
        }
        drop(call);

        // Well past the window the dropped call would have escalated at.
        tokio::time::sleep(WINDOW * 2).await;
        assert_eq!(
            actor.status(),
            ActorStatus::Running,
            "the dropped call must not have raised a Kill"
        );
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "the actor was asked to stop once"
        );

        let report = tokio::time::timeout(
            Duration::from_secs(5),
            sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(5),
                per_actor_timeout: Duration::from_millis(150),
            }),
        )
        .await
        .expect("cleanup must finish");
        assert_eq!(report.outcomes.len(), 1);
    });
}

/// Parks in a handler on `Block`, and records whether it was stopped with
/// `StopReason::Graceful`.
struct GatedActor {
    graceful_stop: Arc<AtomicBool>,
}

enum GatedMsg {
    Block(oneshot::Sender<()>, oneshot::Receiver<()>),
}

impl Actor for GatedActor {
    type Message = GatedMsg;
    type Response = ();

    async fn handle(&mut self, msg: GatedMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let GatedMsg::Block(entered, release) = msg;
        let _ = entered.send(());
        let _ = release.await;
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.graceful_stop
            .store(matches!(reason, StopReason::Graceful), Ordering::SeqCst);
        Ok(())
    }
}

/// An actor busy in a handler is still stopped, with the reason that was
/// requested, by a `stop_within` request whose call was dropped before the
/// actor got to it.
#[test]
fn dropped_stop_within_leaves_a_pending_request_raised() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("dropped-pending")).unwrap();
        let graceful_stop = Arc::new(AtomicBool::new(false));
        let actor = GatedActor {
            graceful_stop: graceful_stop.clone(),
        }
        .spawn()
        .named(uname("gated"))
        .on_system(&sys)
        .await
        .unwrap();

        // Park the actor inside its handler, so the stop request is still
        // pending when the call is dropped.
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        actor
            .notify(GatedMsg::Block(entered_tx, release_rx))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), entered_rx)
            .await
            .expect("the handler must be entered")
            .unwrap();

        // A window far longer than the test: only the caller's timeout ends
        // the call.
        let call = tokio::time::timeout(
            Duration::from_millis(50),
            actor.stop_within(StopReason::Graceful, Duration::from_secs(60)),
        )
        .await;
        assert!(call.is_err(), "the call must be dropped by the caller");
        assert_eq!(actor.status(), ActorStatus::Running);

        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), actor.wait_stopped())
            .await
            .expect("the request raised before the drop must still stop the actor");
        assert!(
            graceful_stop.load(Ordering::SeqCst),
            "the actor stopped with the reason that was requested"
        );
    });
}
