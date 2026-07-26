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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, RestartType, ShutdownPolicy, SpawnError, StopOutcome, StopReason,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn uname(base: &str) -> String {
    format!("shutdown-{base}-{}", uuid::Uuid::new_v4())
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
        report.outcomes,
        vec![(root_id, StopOutcome::Graceful)],
        "the report must contain only the root's outcome, no children"
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

    tokio::time::timeout(Duration::from_secs(5), sys.shutdown())
        .await
        .expect("shutdown must not hang");

    assert_eq!(
        *log.lock().unwrap(),
        vec!["B", "A"],
        "the most-recently-registered root (B) must stop first"
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
        report.outcomes,
        vec![(root_id, StopOutcome::Killed)],
        "a root that always vetoes ParentRequest must be escalated to Kill"
    );
    assert!(
        elapsed < per_actor_timeout + Duration::from_secs(3),
        "escalation to Kill must complete within per_actor_timeout + grace \
         (plus CI scheduling slack), took {elapsed:?}"
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
    assert_eq!(report.outcomes, vec![(root_id, StopOutcome::Aborted)]);
}

// ---------------------------------------------------------------------------
// Test 13: global deadline bounds total shutdown time
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn global_deadline_bounds_total_shutdown_time() {
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

    let started = Instant::now();
    let report = tokio::time::timeout(Duration::from_secs(5), sys.shutdown_with(policy))
        .await
        .expect("shutdown must not hang");
    let elapsed = started.elapsed();

    // A broken (deadline-blind) sequential ladder would take roughly
    // N * (per_actor_timeout + 2 * kill grace) here - over 2.8 seconds.
    // The deadline-aware implementation returns in roughly the global
    // timeout plus one concurrent Kill+abort sweep - a few hundred ms.
    // The 2s bound keeps discriminating between the two even when a CI
    // runner stalls the good path.
    assert!(
        elapsed < Duration::from_secs(2),
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
            .map(|(_, o)| *o);
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

    let result = Idle.spawn().named("too-late").on_system(&sys).await;
    match result {
        Err(SpawnError::SystemShuttingDown(_)) => {}
        Ok(_) => panic!("registration must fail once shutdown has begun"),
        Err(other) => panic!("expected SpawnError::SystemShuttingDown, got: {other}"),
    }
}
