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
    StopReason, SupervisionAction, SystemConfig,
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

    /// Once the shutdown's stop request reached the root: queues a message
    /// whose drop holds the root's teardown after its registry entry is
    /// freed, lets `pre_stop` accept, and waits until the drop holds it.
    /// Panics unless that happened before `deadline`, the instant the
    /// shutdown's global timeout cuts its ladder.
    async fn hold_in_the_window(&self, deadline: Instant) {
        assert!(
            self.hold_if_in_time(deadline).await,
            "the root left the registry before the shutdown's ladder was cut"
        );
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

async fn a_leftover_killed_by_its_own_supervisor_before_the_deadline_reads_graceful_in_every_order_body(
    paused: bool,
) {
    const ROUNDS: usize = 6;
    const TIMEOUT: Duration = Duration::from_millis(800);
    let mut swept_rounds = 0;
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
        if ended >= TIMEOUT {
            // The host delayed the worker's kill past the point where it is
            // sure to precede the deadline (the deadline is no earlier than
            // `started + TIMEOUT`, and the end was seen no earlier than it
            // became final): not the case under test.
            assert!(!paused, "round {round}: the worker ended at {ended:?}");
            continue;
        }
        let Some((_, stop)) = report.swept.iter().find(|(id, _)| id == refuses.id()) else {
            assert!(
                !paused,
                "round {round}: the worker is swept: {:?}",
                report.swept
            );
            // Gone from the registry before the sweep looked.
            assert!(!refuses.is_alive(), "round {round}: {:?}", report.swept);
            continue;
        };
        swept_rounds += 1;
        assert_eq!(
            (stop.outcome, stop.reason.clone()),
            (StopOutcome::Graceful, Some(StopReason::Kill)),
            "round {round}: {:?}",
            report.swept
        );
    }
    assert!(
        swept_rounds >= ROUNDS / 2,
        "the worker was swept in {swept_rounds} of {ROUNDS} rounds"
    );
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
/// the given order, report one real orphan the same way while the end of its
/// orderly stop moves around their shared deadline: the orphan's
/// `on_stopped` ends 240 ms to 360 ms after its parent was killed, so its
/// seal meets the deadline's `Kill` from the reaper and the force. The stops
/// of one lattice step the end by 12 ms from `first_stop_ms`; at every stop
/// both calls agree. Which side of the deadline a stop lands on is pinned on
/// the paused clock by the test after the four below. A stop whose calls swept
/// nothing (the host let the orphan finish before they began) is not compared,
/// and at least half the stops must be. Multi-thread only: the
/// two calls, the orphan and the reaper run in parallel.
fn two_shutdown_calls_agree_around_the_deadline(long_first: bool, first_stop_ms: u64) {
    common::run_multi_thread_only(4, async move {
        let mut outcomes = Vec::new();
        let mut stops = 0;
        for stop_ms in (first_stop_ms..=360).step_by(12) {
            stops += 1;
            let (a, b, finished) = two_calls_on_an_orphan(long_first, stop_ms).await;
            assert_eq!(
                a.swept, b.swept,
                "long_first={long_first} stop_ms={stop_ms}: one end, one report"
            );
            // A host that stalled the calls past the orphan's whole stop
            // leaves nothing for either sweep to report; the other stops
            // carry the race. An orphan still stopping is always reported.
            if a.swept.is_empty() {
                assert!(
                    finished,
                    "long_first={long_first} stop_ms={stop_ms}: a still-stopping orphan went unreported"
                );
                continue;
            }
            assert_eq!(a.swept.len(), 1, "{:?}", a.swept);
            outcomes.push(a.swept[0].1.outcome);
        }
        // `Aborted`: the host held the orphan's stop past the abort that
        // follows the deadline's `Kill` by its kill grace.
        assert!(
            outcomes.iter().all(|outcome| matches!(
                outcome,
                StopOutcome::Graceful | StopOutcome::Killed | StopOutcome::Aborted
            )),
            "a real orphan is reported by its own end, the deadline's Kill or the abort after it: {outcomes:?}"
        );
        assert!(
            outcomes.len() * 2 >= stops,
            "the two calls were compared on a swept orphan at {} of {stops} stops",
            outcomes.len()
        );
    });
}

/// Runs a short (300 ms) and a long (5 s) shutdown call over one orphan
/// whose orderly stop takes `stop_ms`, starting the call named by
/// `long_first` first, and returns the short call's report, the long call's
/// and whether the orphan's stop had finished when both calls returned.
/// A host that stalled past the orphan's whole stop before the calls began
/// leaves them an empty sweep, with the stop finished.
async fn two_calls_on_an_orphan(
    long_first: bool,
    stop_ms: u64,
) -> (ShutdownReport, ShutdownReport, bool) {
    let short = ShutdownPolicy {
        timeout: Duration::from_millis(300),
        per_actor_timeout: Duration::from_millis(100),
    };
    let long = ShutdownPolicy {
        timeout: Duration::from_secs(5),
        per_actor_timeout: Duration::from_millis(100),
    };
    let tree = killed_root_tree(
        Some(Duration::from_millis(stop_ms)),
        Some(ASSUMED_KILL_GRACE),
    )
    .await;
    let mut a = Box::pin(tree.sys.shutdown_with(short));
    let mut b = Box::pin(tree.sys.shutdown_with(long));
    if long_first {
        assert!(poll_once(&mut b).await, "the long call waits in its sweep");
    } else {
        assert!(poll_once(&mut a).await, "the short call waits in its sweep");
    }
    let (a, b) = tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(a, b) })
        .await
        .expect("shutdown must not hang");
    (a, b, tree.finished.load(Ordering::SeqCst))
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

/// A supervisor child whose `on_stopped` blocks its thread for 4 s and
/// raises `entered` as it begins.
struct BlocksItsThreadWhileStopping {
    entered: Arc<AtomicBool>,
}

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
        self.entered.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(4_000));
        Ok(())
    }
}

/// A root whose supervisor child is [`BlocksItsThreadWhileStopping`].
struct HoldsAThreadBlocker {
    entered: Arc<AtomicBool>,
}

impl Actor for HoldsAThreadBlocker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let entered = self.entered.clone();
        ctx.spawn_child(move || BlocksItsThreadWhileStopping {
            entered: entered.clone(),
        })
        .named("blocks")
        .shutdown(Shutdown::Timeout(Duration::from_secs(10)))
        .with_config(ActorConfig::default().supervisor())
        .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// A dropped `shutdown_with` goes on in the system; a later `shutdown` completes it
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

    // Registered first, so the shutdown reaches it last: a root the walk
    // gets to only after the vetoing root's window.
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
    tokio::time::timeout(HANG_MARGIN, supervisor.send(()))
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

    // The first shutdown has a window far longer than the test. It is
    // dropped as soon as the vetoing root has been asked to stop; its walk
    // goes on without it.
    let began = Instant::now();
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
        wait_until(HANG_MARGIN.as_millis() as u64, || actions
            .lock()
            .unwrap()
            .contains(&SupervisionAction::BudgetExhausted))
        .await,
        "the refused restarts must exhaust the supervisor's budget: {:?}",
        actions.lock().unwrap()
    );
    tokio::time::timeout(HANG_MARGIN, supervisor.wait_stopped())
        .await
        .expect("the supervisor must end once its budget is exhausted");
    assert!(
        wait_until(HANG_MARGIN.as_millis() as u64, || !sys
            .actor_ids()
            .contains(supervisor.id()))
        .await,
        "the ended supervisor must leave the registry before the later shutdown"
    );

    // The walk waits out the vetoing root's window (the first call's 30 s
    // `per_actor_timeout`, counted from the stop request, which is no earlier
    // than `began`) before it asks the next root. So the other root has not
    // been asked while fewer than 30 s have passed since `began`: the
    // observation below is made first and counts only if the clock, read
    // after it, is still inside that bound.
    let asked_before_the_window_ended = unreached_asked.load(Ordering::SeqCst);
    let status_before_the_window_ended = unreached.status();
    if began.elapsed() < Duration::from_secs(30) {
        assert_eq!(
            asked_before_the_window_ended, 0,
            "the walk waits out the vetoing root's window before the next root"
        );
        assert_eq!(status_before_the_window_ended, ActorStatus::Running);
    }

    // A later shutdown completes the job: it escalates both vetoing roots,
    // the one the first walk is waiting on and the one after it, and the
    // system returns to service.
    let bound = upper(
        paused,
        (PER_ROOT + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE) * 2,
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
    assert!(!unreached.is_alive(), "the second root must be stopped");
    assert!(
        unreached_asked.load(Ordering::SeqCst) >= 1,
        "the second root is asked to stop"
    );
    // The dropped call's walk is a shutdown in flight of its own, and
    // `reactivate` is refused until every shutdown in flight has completed.
    // It ends on its own tasks, once it finds both roots stopped, so the
    // system becomes reactivatable some time after the later shutdown has
    // returned: wait for that observable end.
    assert!(
        wait_until(HANG_MARGIN.as_millis() as u64, || sys.reactivate()).await,
        "once the dropped call's walk has ended too, the system must be reactivatable"
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
