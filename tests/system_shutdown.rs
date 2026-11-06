#![allow(dead_code)]
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

use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorHandle, ActorId, ActorResult, ActorSystem, RestartType, Shutdown,
    ShutdownPolicy, ShutdownReport, SpawnError, StopOutcome, StopReason,
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

/// The bound on an await that must complete, such as a shutdown call, a stop
/// or a held drop. It only limits how long a real hang takes to report.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// How far past its exact timing an outcome may land on a real-time runtime
/// in a test whose failing outcome waits for a turn held until the test
/// releases it: loose enough for a host that stalls its threads, and far
/// short of the held turn.
const STALL_SLACK: Duration = Duration::from_secs(2);

/// How far past its exact instant an outcome may land on the paused clock:
/// one poll step of a waiting loop and the timers' rounding, far short of any
/// window a test configures.
const PAUSED_SLACK: Duration = Duration::from_millis(30);

/// The upper bound of an outcome that lands at `exact`: that instant plus
/// [`PAUSED_SLACK`] on the paused clock, and a hang bound past it on a
/// real-time runtime.
fn upper(paused: bool, exact: Duration) -> Duration {
    if paused {
        exact + PAUSED_SLACK
    } else {
        exact + HANG_MARGIN
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

/// Watches a runtime for a stall. A task wakes every 5 ms and notes the
/// longest time between two wake-ups that ends at or after `from`; on a
/// runtime with one worker thread every task stops when the thread does, so a
/// long gap is the host holding that thread. A test whose outcome needs a task
/// to be polled within a kill grace counts a run only when the gap stayed
/// under `STALL_GAP`. A test sets `from` to the point where its outcome starts
/// to depend on such a poll: a gap that ends before it only delays every step
/// after it, and a gap that reaches past it counts in full.
struct Canary {
    from: Instant,
    last: Arc<Mutex<Instant>>,
    longest: Arc<Mutex<Duration>>,
    stop: Arc<AtomicBool>,
}

/// Half the crate's default kill grace: a run whose canary saw a longer gap
/// is not the case under test.
const STALL_GAP: Duration = Duration::from_millis(60);

/// The gap that spoils a run of a test whose order survives stalls of
/// several hundred milliseconds.
const WIDE_STALL_GAP: Duration = Duration::from_millis(150);

impl Canary {
    /// Counts every gap from now on.
    fn start() -> Canary {
        Canary::watching_from(Instant::now())
    }

    /// Counts only the gaps that end at or after `from`.
    fn watching_from(from: Instant) -> Canary {
        let canary = Canary {
            from,
            last: Arc::new(Mutex::new(Instant::now())),
            longest: Arc::new(Mutex::new(Duration::ZERO)),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let (last, longest, stop) = (
            canary.last.clone(),
            canary.longest.clone(),
            canary.stop.clone(),
        );
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
                let now = Instant::now();
                let mut last = last.lock().unwrap();
                if now >= from {
                    let mut longest = longest.lock().unwrap();
                    *longest = (*longest).max(now - *last);
                }
                *last = now;
            }
        });
        canary
    }

    /// Stops the canary and returns the longest gap it counted, the time
    /// since its last wake-up included when that reaches `from`.
    fn finish(&self) -> Duration {
        self.stop.store(true, Ordering::SeqCst);
        let now = Instant::now();
        let last = *self.last.lock().unwrap();
        let pending = if now >= self.from {
            now - last
        } else {
            Duration::ZERO
        };
        (*self.longest.lock().unwrap()).max(pending)
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

// ---------------------------------------------------------------------------
// Test 11: vetoing root escalates to Killed
// ---------------------------------------------------------------------------

/// The Kill step gives the root one kill grace to observe the `Kill` before
/// the abort, so the outcome needs the root's task to be polled within that
/// grace. The runtime has one worker thread and a canary watches it from
/// `STALL_GAP` before the escalation's `Kill` at the per-actor timeout: a run
/// whose canary saw a stall of half the grace or more is not the case under
/// test and is run again, up to five times.
#[test]
fn vetoing_root_escalates_to_killed() {
    common::run(1, async {
        for round in 1..=5 {
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
            let canary = Canary::watching_from(started + per_actor_timeout - STALL_GAP);
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
            let gap = canary.finish();
            if gap >= STALL_GAP {
                assert!(round < 5, "five runs never had a steady runtime: {gap:?}");
                continue;
            }

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
            return;
        }
    });
}

// ---------------------------------------------------------------------------
// Test 12: root hung at an await is Aborted
// ---------------------------------------------------------------------------

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
    // settle time, which the sequential ladder exceeds by more than a
    // second; the real-time variant keeps a hang bound.
    let precise = global + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE;
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

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn global_deadline_bounds_total_shutdown_time_current_thread() {
    global_deadline_bounds_total_shutdown_time(true).await;
}

// ---------------------------------------------------------------------------
// Test: the two forced steps of a shutdown add up
// ---------------------------------------------------------------------------

/// An actor that can block its thread for a while: a turn holds the thread
/// for the given time or until the turn is released, raising `entered` as it
/// begins. A blocked turn takes no request and, holding the thread, survives
/// the abort, so the actor ends only when the turn does.
struct HoldsItsThread {
    turn: Arc<HeldTurn>,
}

impl Actor for HoldsItsThread {
    type Message = Duration;
    type Response = ();

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

/// A root supervisor of one [`HoldsItsThread`] worker with its own kill
/// grace, and itself able to block its thread the same way.
struct RootOverAWorker {
    turn: Arc<HeldTurn>,
    worker_turn: Arc<HeldTurn>,
    worker_grace: Duration,
}

impl Actor for RootOverAWorker {
    type Message = Duration;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let worker_turn = self.worker_turn.clone();
        ctx.spawn_child(move || HoldsItsThread {
            turn: worker_turn.clone(),
        })
        .named("held-worker")
        .shutdown(Shutdown::Timeout(Duration::from_secs(5)))
        .kill_grace(self.worker_grace)
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

/// A root and a leftover that both ignore every request, the `Kill` and the
/// abort (each holds its thread in a turn) are forced in two steps: the root,
/// whose kill grace and settle time run from the global deadline, and then
/// the leftover, whose own distinct kill grace and settle time run from the
/// end of the first step. The shutdown therefore ends no earlier than the
/// first step plus the leftover's grace, and within the sum of both steps
/// after the deadline.
#[test]
fn a_root_and_a_leftover_that_ignore_every_stop_end_a_shutdown_after_both_forced_steps() {
    const TIMEOUT: Duration = Duration::from_millis(400);
    const ROOT_GRACE: Duration = Duration::from_millis(250);
    const LEFTOVER_GRACE: Duration = Duration::from_millis(700);
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("two-steps")).unwrap();
        let root_turn = Arc::new(HeldTurn::default());
        let worker_turn = Arc::new(HeldTurn::default());
        let _releases = (
            ReleasesTheTurn(root_turn.clone()),
            ReleasesTheTurn(worker_turn.clone()),
        );
        let root = RootOverAWorker {
            turn: root_turn.clone(),
            worker_turn: worker_turn.clone(),
            worker_grace: LEFTOVER_GRACE,
        }
        .spawn()
        .named(uname("two-steps-root"))
        .on_system(&sys)
        .kill_grace(ROOT_GRACE)
        .supervisor()
        .await
        .unwrap();
        assert!(wait_until(2_000, || sys.get::<HoldsItsThread>("held-worker").is_some()).await);
        let worker = sys.get::<HoldsItsThread>("held-worker").unwrap();
        root.notify(HELD_FOR).await.unwrap();
        worker.notify(HELD_FOR).await.unwrap();
        assert!(
            wait_until(2_000, || {
                root_turn.entered.load(Ordering::SeqCst)
                    && worker_turn.entered.load(Ordering::SeqCst)
            })
            .await
        );

        // The request step is far longer than the global deadline, so the
        // deadline is what cuts the root's ladder.
        let started = Instant::now();
        let report = tokio::time::timeout(
            HANG_BOUND,
            sys.shutdown_with(ShutdownPolicy {
                timeout: TIMEOUT,
                per_actor_timeout: Duration::from_secs(5),
            }),
        )
        .await
        .expect("shutdown must not hang");
        let took = started.elapsed();

        let first_step = TIMEOUT + ROOT_GRACE + ASSUMED_ABORT_SETTLE;
        assert!(
            took >= first_step + LEFTOVER_GRACE,
            "the leftover is given its own kill grace after the first step ends: shutdown took \
             {took:?}"
        );
        assert!(
            took < first_step + LEFTOVER_GRACE + ASSUMED_ABORT_SETTLE + STALL_SLACK,
            "the shutdown ends within the deadline plus both steps' grace and settle time: it \
             took {took:?}"
        );
        assert!(
            !root_turn.left.load(Ordering::SeqCst) && !worker_turn.left.load(Ordering::SeqCst),
            "both turns held their threads through the shutdown"
        );
        assert_eq!(
            outcomes_of(&report),
            vec![(root.id().clone(), StopOutcome::Unresponsive)]
        );
        assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
        assert_eq!(&report.swept[0].0, worker.id());
        assert_eq!(report.swept[0].1.outcome, StopOutcome::Unresponsive);
    });
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
/// the root snapshot. The shutdown is held in the stop of the first root, a
/// root whose `pre_stop` waits for the test, until a refused spawn has been
/// seen, so the refusal happens while the shutdown provably runs. The
/// spawners stop adding roots at a bound until the test has called the
/// shutdown, and keep trying after it, so a spawn is always attempted after
/// the shutdown sealed registration, however long the test took to call it.
#[tokio::test(flavor = "multi_thread")]
async fn roots_registered_during_shutdown_are_reported_in_outcomes() {
    let sys = ActorSystem::create(uname("late-roots")).unwrap();

    const SPAWNERS: usize = 6;
    const MAX_ROOTS: usize = 20_000;

    let (in_pre_stop, accept, stopped_with) = (
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Mutex::new(None)),
    );
    // Registered first, so the shutdown stops it last, after the roots the
    // spawners registered.
    let gate = Lingering {
        in_pre_stop,
        accept: accept.clone(),
        stopped_with,
    }
    .spawn()
    .named(uname("late-roots-gate"))
    .on_system(&sys)
    .await
    .unwrap();

    let spawned_ids: Arc<Mutex<Vec<ActorId>>> = Arc::new(Mutex::new(Vec::new()));
    let shutdown_returned = Arc::new(AtomicBool::new(false));
    let refused_while_running = Arc::new(AtomicUsize::new(0));
    let first_refusal = Arc::new(Notify::new());
    let shutdown_called = Arc::new(AtomicBool::new(false));

    let mut spawn_tasks = Vec::with_capacity(SPAWNERS);
    for _ in 0..SPAWNERS {
        let sys = sys.clone();
        let spawned_ids = spawned_ids.clone();
        let (returned, refused) = (shutdown_returned.clone(), refused_while_running.clone());
        let first_refusal = first_refusal.clone();
        let shutdown_called = shutdown_called.clone();
        spawn_tasks.push(tokio::spawn(async move {
            // The bound keeps a system that stops refusing from growing
            // without limit while the test waits for a refusal. A spawner at
            // the bound waits, it does not end: before the shutdown is called
            // it waits for the call, and after it the bound doubles.
            while !returned.load(Ordering::SeqCst) {
                let bound = if shutdown_called.load(Ordering::SeqCst) {
                    2 * MAX_ROOTS
                } else {
                    MAX_ROOTS
                };
                if spawned_ids.lock().unwrap().len() >= bound {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue;
                }
                match Idle.spawn().on_system(&sys).detached().await {
                    Ok(handle) => spawned_ids.lock().unwrap().push(handle.id().clone()),
                    Err(SpawnError::SystemShuttingDown(_)) => {
                        if !returned.load(Ordering::SeqCst) {
                            refused.fetch_add(1, Ordering::SeqCst);
                            first_refusal.notify_one();
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

    shutdown_called.store(true, Ordering::SeqCst);
    let shutdown = {
        let sys = sys.clone();
        tokio::spawn(async move {
            sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(30),
                per_actor_timeout: Duration::from_secs(30),
            })
            .await
        })
    };
    assert!(
        tokio::time::timeout(HANG_BOUND, first_refusal.notified())
            .await
            .is_ok(),
        "a spawn raced the running shutdown and was refused"
    );
    accept.notify_one();
    let report = tokio::time::timeout(Duration::from_secs(30), shutdown)
        .await
        .expect("shutdown must not hang")
        .unwrap();
    shutdown_returned.store(true, Ordering::SeqCst);

    for task in spawn_tasks {
        task.await.expect("a spawner task must not panic");
    }

    assert!(
        refused_while_running.load(Ordering::SeqCst) > 0,
        "a spawn raced the running shutdown and was refused"
    );
    assert!(
        report.outcomes.iter().any(|(id, _)| id == gate.id()),
        "the gate root is reported in `outcomes`: {:?}",
        outcomes_of(&report)
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

/// Notes the instant it was dropped and signals `entered`, then holds the
/// dropping thread until `open` is set (for at most `HANG_BOUND`).
struct HeldDrop {
    entered: Arc<tokio::sync::Notify>,
    entered_at: Arc<Mutex<Option<std::time::Instant>>>,
    open: Arc<AtomicBool>,
}

impl Drop for HeldDrop {
    fn drop(&mut self) {
        *self.entered_at.lock().unwrap() = Some(std::time::Instant::now());
        self.entered.notify_one();
        let bound = std::time::Instant::now() + HANG_BOUND;
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
    /// Signalled when `pre_stop` begins.
    in_pre_stop: Arc<tokio::sync::Notify>,
    /// Signalled by the test to let `pre_stop` accept.
    accept: Arc<tokio::sync::Notify>,
    stopped_with: Arc<Mutex<Option<StopReason>>>,
}

impl Actor for Lingering {
    type Message = Ballast;
    type Response = ();

    async fn handle(&mut self, _msg: Ballast, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.in_pre_stop.notify_one();
        self.accept.notified().await;
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

/// A `Lingering` root on its own runtime, and the flags that drive it.
struct WindowRoot {
    handle: tokio_actors::ActorHandle<Lingering>,
    in_pre_stop: Arc<tokio::sync::Notify>,
    accept: Arc<tokio::sync::Notify>,
    entered: Arc<tokio::sync::Notify>,
    entered_at: Arc<Mutex<Option<std::time::Instant>>>,
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
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
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
            entered: Arc::new(tokio::sync::Notify::new()),
            entered_at: Arc::new(Mutex::new(None)),
            open,
            stopped_with,
        }
    }

    /// As [`hold_in_the_window`](Self::hold_in_the_window), and returns
    /// whether the drop took the root's teardown before `deadline`, read from
    /// the instant the drop itself noted, not from when this task looked. A
    /// host that stalls the root past `deadline` before its teardown reaches
    /// the drop returns `false`: the root is then still held, and the caller
    /// releases it and runs the scenario again.
    async fn hold_if_in_time(&self, deadline: Instant) -> bool {
        assert!(
            tokio::time::timeout(HANG_BOUND, self.in_pre_stop.notified())
                .await
                .is_ok(),
            "the shutdown's stop request reaches the root"
        );
        self.handle
            .notify(Ballast::Held(HeldDrop {
                entered: self.entered.clone(),
                entered_at: self.entered_at.clone(),
                open: self.open.clone(),
            }))
            .await
            .unwrap();
        self.accept.notify_one();
        assert!(
            tokio::time::timeout(HANG_BOUND, self.entered.notified())
                .await
                .is_ok(),
            "the root's teardown reaches the queued message's drop"
        );
        let at = self
            .entered_at
            .lock()
            .unwrap()
            .expect("the drop noted its instant");
        if at >= deadline.into_std() {
            return false;
        }
        assert!(!self.handle.is_alive());
        assert_eq!(self.handle.status(), tokio_actors::ActorStatus::Stopping);
        true
    }
}

/// A root in its own kill grace, out of the registry and not yet `Stopped`,
/// read by `stop_within` and by the shutdown's sequential ladder. Both wait
/// the root's own grace (read while its entry still held it) and the wait
/// after an abort, and both give the one verdict for it: `Unresponsive`, the
/// drop being held until each call has returned. A run counts only when the
/// drop took the root's teardown inside the call's window (the instant the
/// drop noted); a run the host's load pushed out of that order is run again,
/// up to five times.
#[test]
fn stop_within_and_the_shutdown_wait_the_root_s_own_grace_after_its_entry_is_gone() {
    let actors = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let on = actors.handle().clone();
    common::run(2, async move {
        const ATTEMPTS: usize = 5;
        // Longer than the default grace, so the wait read is the root's own.
        let grace = Duration::from_millis(600);
        let settle = Duration::from_millis(100);
        let window = Duration::from_millis(2500);
        let mut verdicts = Vec::new();
        for through_shutdown in [false, true] {
            let mut counted = None;
            for attempt in 1..=ATTEMPTS {
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
                let in_time = root.hold_if_in_time(started + window).await;
                let verdict = tokio::time::timeout(HANG_BOUND, call)
                    .await
                    .expect("the call returns")
                    .unwrap();
                let took = started.elapsed();
                open.store(true, Ordering::SeqCst);
                tokio::time::timeout(HANG_BOUND, root.handle.wait_stopped())
                    .await
                    .expect("the root publishes Stopped");
                if in_time {
                    assert!(
                        took >= window + grace + settle,
                        "through_shutdown={through_shutdown}: waited the root's own grace, took {took:?}"
                    );
                    counted = Some(verdict);
                    break;
                }
                assert!(
                    attempt < ATTEMPTS,
                    "{ATTEMPTS} runs never had the root held inside the window"
                );
            }
            verdicts.push(counted.expect("a run counted"));
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
    let tree = killed_root_tree(stop_for, kill_grace).await;
    // The child is an orphan now: it is stopping in order, and registered.
    assert!(tree.child.is_alive());
    tree
}

/// [`orphaned_subtree_with_grace`] without the check that the child is still
/// registered when the root's end is seen: for a caller whose child stops in
/// a time a stalled host can outlast.
async fn killed_root_tree(stop_for: Option<Duration>, kill_grace: Option<Duration>) -> SlowTree {
    let tree = slow_tree_with_grace(stop_for, false, kill_grace).await;
    tree.root.stop(StopReason::Kill).await.unwrap();
    tree.root.wait_stopped().await;
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
/// when the sweep found the child, the drop began (after the end became
/// final) before the deadline and ended after it; a round the host's load
/// pushed out of that order is run again, up to five times.
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
        let (entered_at, left_at) = (*entered.lock().unwrap(), *left.lock().unwrap());
        let in_case = report.swept.len() == 1
            && matches!(
                (entered_at, left_at),
                (Some(entered), Some(left))
                    if entered < started + TIMEOUT
                        && left >= started + TIMEOUT + Duration::from_millis(20)
            );
        if !in_case {
            // The child had left the registry before the sweep looked (the
            // host held the walk back past its whole stop, so nothing was
            // swept), its end did not become final before the deadline, or
            // `Stopped` came before it: not the case under test. The drop
            // ends before the next round starts.
            assert!(
                wait_until(HANG_BOUND.as_millis() as u64, || left
                    .lock()
                    .unwrap()
                    .is_some())
                .await,
                "the drop ended"
            );
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
        ended < upper(paused, TIMEOUT + ASSUMED_KILL_GRACE),
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
        let (ended, root_left) = tokio::time::timeout(HANG_BOUND, ended)
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
        ended < upper(paused, TIMEOUT + ASSUMED_KILL_GRACE),
        "the leftover ends one kill grace after the deadline: it ended at {ended:?}"
    );
    assert!(!tree.finished.load(Ordering::SeqCst));
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
        ended >= FIRST + ASSUMED_KILL_GRACE && ended < upper(paused, FIRST + ASSUMED_KILL_GRACE),
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
/// 600 ms shutdown would, never `Graceful` by its own later deadline. The
/// order survives a stall of the runtime of up to 400 ms before the calls and
/// 600 ms after them, so no gap under 150 ms can break it; the runtime has
/// one worker thread and a canary watches it
/// from before the root is killed, and a run whose canary saw a gap of 150 ms
/// or more is not the case under test and is run again, up to five times.
#[test]
fn a_later_shutdown_reports_by_the_deadline_of_a_sweep_that_started() {
    common::run(1, async {
        for round in 1..=5 {
            let canary = Canary::start();
            let tree =
                killed_root_tree(Some(Duration::from_secs(1)), Some(Duration::from_secs(1))).await;
            let mut first = Box::pin(tree.sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_millis(600),
                per_actor_timeout: Duration::from_millis(100),
            }));
            let pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(first.as_mut().poll(cx).is_pending())
            })
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
            let gap = canary.finish();
            if gap >= WIDE_STALL_GAP {
                assert!(round < 5, "five runs never had a steady runtime: {gap:?}");
                continue;
            }
            assert_eq!(report.swept.len(), 1, "{:?}", report.swept);
            assert_eq!(&report.swept[0].0, tree.child.id());
            assert_eq!(
                report.swept[0].1.outcome,
                StopOutcome::Killed,
                "{:?}",
                report.swept[0].1
            );
            return;
        }
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

/// A completed shutdown leaves no deadline behind: after a shutdown that
/// swept a leftover completes and the system is reactivated, a new orphan
/// stopping in order under a long second shutdown finishes its `on_stopped`
/// and is reported `Graceful`, never forced by the first shutdown's past
/// deadline. The first shutdown stores its deadline whether or not an orphan
/// is still registered when its sweep looks; a second round whose orphan had
/// finished before the sweep looked (the host held the walk back past its
/// whole stop) leaves nothing to classify and is run again, up to five times.
#[test]
fn a_completed_shutdown_leaves_no_deadline_after_a_reactivation() {
    common::run(0, async {
        let first = killed_root_tree(Some(Duration::from_millis(100)), None).await;
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            first.sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_millis(300),
                per_actor_timeout: Duration::from_millis(100),
            }),
        )
        .await
        .expect("shutdown must not hang");
        assert!(
            report.swept.len() <= 1,
            "at most the one orphan: {:?}",
            report.swept
        );
        if let Some((id, _)) = report.swept.first() {
            assert_eq!(id, first.child.id());
        }
        let sys = first.sys.clone();
        for round in 1..=5 {
            assert!(sys.reactivate(), "the completed shutdown left Defunct");
            let second =
                slow_tree_on(sys.clone(), Some(Duration::from_millis(500)), false, None).await;
            second.root.stop(StopReason::Kill).await.unwrap();
            second.root.wait_stopped().await;
            let report = tokio::time::timeout(
                Duration::from_secs(10),
                second.sys.shutdown_with(ShutdownPolicy {
                    timeout: Duration::from_secs(5),
                    per_actor_timeout: Duration::from_millis(100),
                }),
            )
            .await
            .expect("shutdown must not hang");
            if report.swept.is_empty() && second.finished.load(Ordering::SeqCst) {
                // Nothing was registered when the sweep looked.
                assert!(round < 5, "five rounds never had the orphan swept");
                continue;
            }
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
            return;
        }
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
/// ends `Killed`, never aborted at once. The child is swept only if it is
/// still registered when the sweep looks, 100 ms after its stop began, and it
/// is aborted only if its stop outlasts the grace; the runtime has one worker
/// thread and a canary watches it from 1 s after the shutdown begins (the
/// root's `Kill` comes at its per-actor timeout or later and its abort 1 s
/// after that, and the child's stop, the sweep and the child's grace follow
/// the abort), and a run whose canary saw a stall of 60 ms or more is not the
/// case under test and is run again, up to five times.
#[test]
fn a_late_sweep_still_gives_a_leftover_its_kill_grace_after_the_deadline_kill() {
    common::run(1, async {
        for round in 1..=5 {
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
            let canary = Canary::watching_from(Instant::now() + Duration::from_secs(1));
            let report = tokio::time::timeout(
                Duration::from_secs(10),
                sys.shutdown_with(ShutdownPolicy {
                    timeout: Duration::from_millis(200),
                    per_actor_timeout: Duration::from_millis(100),
                }),
            )
            .await
            .expect("shutdown must not hang");
            let gap = canary.finish();
            if gap >= STALL_GAP {
                assert!(round < 5, "five runs never had a steady runtime: {gap:?}");
                continue;
            }
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
            return;
        }
    });
}

// ---------------------------------------------------------------------------
// A dropped `shutdown_with` goes on in the system; a later `shutdown` completes it
// ---------------------------------------------------------------------------

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

/// The `Kill` of the escalation needs the root's task to be polled within its
/// kill grace. On a real clock the runtime has one worker thread and a canary
/// watches it from `STALL_GAP` before the root's window ends, where the `Kill`
/// is raised (before then a gap only delays the children's stops, which
/// already outlast the window): a run whose canary saw a stall of half the
/// grace or more is not the case under test and is run again, up to five
/// times. The paused clock has no stalls, and runs once.
#[test]
fn root_supervisor_with_a_default_window_child_is_escalated_at_system_shutdown() {
    common::run(
        1,
        root_supervisor_with_a_default_window_child_is_escalated_at_system_shutdown_body(false),
    );
}

async fn root_supervisor_with_a_default_window_child_is_escalated_at_system_shutdown_body(
    paused: bool,
) {
    const SYSTEM_PER_ROOT: Duration = Duration::from_secs(5);
    for round in 1..=5 {
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
            SYSTEM_PER_ROOT + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE,
        );
        let canary =
            (!paused).then(|| Canary::watching_from(Instant::now() + SYSTEM_PER_ROOT - STALL_GAP));
        let report = tokio::time::timeout(bound, sys.shutdown())
            .await
            .expect("the shutdown must finish within the root's window plus its kill ladder");
        if let Some(canary) = canary {
            let gap = canary.finish();
            if gap >= STALL_GAP {
                assert!(round < 5, "five runs never had a steady runtime: {gap:?}");
                continue;
            }
        }

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
        return;
    }
}

// ---------------------------------------------------------------------------
// A dropped `stop_within` is still escalated by the system
// ---------------------------------------------------------------------------
