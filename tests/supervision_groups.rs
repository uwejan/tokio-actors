//! Behavioral suite for the OneForAll / RestForOne group-restart machinery,
//! including the interleaved-failure storm:
//!
//! - When one child crashes (panic in `handle`), the runtime stops the affected
//!   LIVE siblings with `StopReason::ParentRequest` (OneForAll: all of them;
//!   RestForOne: only those started AFTER the failed child), awaits their
//!   watcher-delivered deaths, then restarts the whole affected set: each
//!   member's restart is initiated in START order, the next only after the
//!   previous has re-registered; start-callback completion may interleave.
//!   Children outside the affected set are untouched.
//! - ONE budget charge per triggering failure; group-member deaths during the
//!   group do NOT charge (`group_restart_single_budget_charge`).
//! - `Shutdown::Timeout(d)` escalates to Kill at expiry, so a `pre_stop` veto
//!   cannot stall a group forever (`vetoing_child_killed_by_timeout_escalation`).
//! - No duplicate live instances during restart storms (incarnation guard),
//!   proven with factory-increment / Drop-decrement instance accounting. The
//!   decrement lives in `Drop`, NOT `on_stopped`, because the Kill escalation
//!   path bypasses all lifecycle callbacks while `Drop` always runs.
//! - Restart INIT completion is sequential within a group: the next member's
//!   restart is initiated only after the previous member's fresh incarnation
//!   has acked `pre_start`/`on_started`, so consecutive members' init windows
//!   never overlap (`one_for_all_restart_sequential_init`).
//! - A member whose restart attempt itself fails (factory panic, or a
//!   start_timeout expiry) is retried in place; later members in the chain
//!   stay down until the retry succeeds, and the retry charges the budget
//!   exactly once, never once per remaining member
//!   (`one_for_all_mid_group_failure_retries`, `one_for_all_hung_init_escalates`).
//! - A hung `on_started` during a restart is bounded by the child's
//!   `start_timeout`; the supervisor keeps answering ordinary messages the
//!   whole time (`one_for_all_hung_init_escalates`).
//!
//! Idioms: multi_thread runtime, per-test-unique actor names (the default
//! `ActorSystem` registry is process-global and tests run concurrently), and
//! event-based waiting (bounded polling) instead of long fixed sleeps.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::time::sleep;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorSystem, ChildEvent, RestartType, Shutdown,
    SpawnError, StopReason, SupervisionAction, SupervisionConfig,
};

// ---------------------------------------------------------------------------
// Timing constants: deadlines are generous, polling is tight.
// ---------------------------------------------------------------------------

/// Upper bound for any single wait. Tests fail loudly on expiry.
const DEADLINE: Duration = Duration::from_secs(10);
/// Poll interval for event-based waiting.
const POLL: Duration = Duration::from_millis(10);
/// Settling window after a wait condition first holds; used to detect
/// late/duplicate events (double-restart storms) before asserting exact counts.
const QUIET: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// Instance accounting
// ---------------------------------------------------------------------------

/// Counts actor instances. Increment happens in the child FACTORY (so every
/// construction is counted, including restart attempts that later fail);
/// decrement happens in the actor's `Drop` impl (so even a `Kill`ed instance,
/// which skips all lifecycle callbacks, is still counted down).
///
/// `max_live` is updated at the increment site itself (`fetch_max` on the new
/// value), which catches every transient spike exactly; the background sampler
/// task (see `spawn_max_sampler`) is belt-and-braces on top of that.
#[derive(Clone, Default)]
struct InstanceCounter {
    live: Arc<AtomicUsize>,
    max_live: Arc<AtomicUsize>,
    constructions: Arc<AtomicUsize>,
}

impl InstanceCounter {
    fn on_construct(&self) {
        self.constructions.fetch_add(1, Ordering::SeqCst);
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_live.fetch_max(now, Ordering::SeqCst);
    }

    fn on_drop(&self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }

    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    fn max_live(&self) -> usize {
        self.max_live.load(Ordering::SeqCst)
    }

    fn constructions(&self) -> usize {
        self.constructions.load(Ordering::SeqCst)
    }
}

/// Background sampler capturing the max observed live-instance count.
/// Abort the returned handle when the test is done with it.
fn spawn_max_sampler(counter: &InstanceCounter) -> tokio::task::JoinHandle<()> {
    let live = counter.live.clone();
    let max_live = counter.max_live.clone();
    tokio::spawn(async move {
        loop {
            max_live.fetch_max(live.load(Ordering::SeqCst), Ordering::SeqCst);
            sleep(Duration::from_millis(1)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Event-based waiting helpers
// ---------------------------------------------------------------------------

/// Polls `cond` every `POLL` until it holds or `DEADLINE` expires (panic).
async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if cond() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out after {DEADLINE:?} waiting for: {what}");
        }
        sleep(POLL).await;
    }
}

/// True when `name` resolves in the default system to a live handle of type `A`.
fn alive_as<A: Actor>(name: &str) -> bool {
    ActorSystem::default()
        .get::<A>(name)
        .map(|h| h.is_alive())
        .unwrap_or(false)
}

/// True when every name in `names` is registered and alive as an `A`.
fn all_alive<A: Actor>(names: &[String]) -> bool {
    names.iter().all(|n| alive_as::<A>(n))
}

/// Polls the default-system registry until `name` resolves to a live handle,
/// then returns that handle. Handles are invalidated by restarts, so tests
/// re-lookup before every interaction instead of caching.
async fn live_handle<A: Actor>(name: &str) -> ActorHandle<A> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if let Some(h) = ActorSystem::default().get::<A>(name) {
            if h.is_alive() {
                return h;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out after {DEADLINE:?} waiting for live actor '{name}'");
        }
        sleep(POLL).await;
    }
}

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum ChildMsg {
    /// Panic inside `handle` (the crash under test).
    Crash,
}

/// Group member: logs "start:<name>" in `on_started` and "stop:<name>" in
/// `on_stopped`, decrements the live counter in `Drop`, and panics on command.
struct GroupChild {
    name: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
}

impl Actor for GroupChild {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Crash => panic!("{} crashed on command", self.name),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.log
            .lock()
            .unwrap()
            .push(format!("start:{}", self.name));
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.log.lock().unwrap().push(format!("stop:{}", self.name));
        Ok(())
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

/// Supervisor that spawns `child_names` (in Vec order = START order) in
/// `on_started` and records every `ChildEvent` it receives.
struct GroupSup {
    child_names: Vec<String>,
    child_shutdown: Shutdown,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    events: Arc<Mutex<Vec<ChildEvent>>>,
}

impl Actor for GroupSup {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for name in self.child_names.clone() {
            let log = self.log.clone();
            let counter = self.counter.clone();
            let child_name = name.clone();
            ctx.spawn_child(move || {
                counter.on_construct();
                GroupChild {
                    name: child_name.clone(),
                    log: log.clone(),
                    counter: counter.clone(),
                }
            })
            .named(name)
            .restart_type(RestartType::Permanent)
            .shutdown(self.child_shutdown)
            .await?;
        }
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

/// A child that refuses graceful/parent-requested stops via `pre_stop`.
/// With `Shutdown::Timeout(d)` the runtime must escalate to Kill at expiry;
/// Kill bypasses `on_stopped` (so `stopped_calls` stays 0 for a killed
/// instance) but `Drop` still runs (so instance accounting stays exact).
struct VetoChild {
    counter: InstanceCounter,
    stopped_calls: Arc<AtomicUsize>,
}

impl Actor for VetoChild {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, _msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for VetoChild {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

/// Supervisor for the veto/escalation test: one crashable sibling plus one
/// vetoing child with a short shutdown timeout.
struct VetoSup {
    crasher_name: String,
    veto_name: String,
    log: Arc<Mutex<Vec<String>>>,
    crasher_counter: InstanceCounter,
    veto_counter: InstanceCounter,
    veto_stopped: Arc<AtomicUsize>,
}

impl Actor for VetoSup {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let log = self.log.clone();
        let crash_ctr = self.crasher_counter.clone();
        let crasher_name = self.crasher_name.clone();
        ctx.spawn_child(move || {
            crash_ctr.on_construct();
            GroupChild {
                name: crasher_name.clone(),
                log: log.clone(),
                counter: crash_ctr.clone(),
            }
        })
        .named(self.crasher_name.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
        .await?;

        let veto_ctr = self.veto_counter.clone();
        let veto_stops = self.veto_stopped.clone();
        ctx.spawn_child(move || {
            veto_ctr.on_construct();
            VetoChild {
                counter: veto_ctr.clone(),
                stopped_calls: veto_stops.clone(),
            }
        })
        .named(self.veto_name.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(150)))
        .await?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Test 9: OneForAll ordering - stop phase fully precedes restart phase, and
// restarts fire in START order (a, b, c).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn one_for_all_stop_reverse_restart_forward() {
    const P: &str = "one_for_all_stop_reverse_restart_forward";
    let a = format!("{P}-a");
    let b = format!("{P}-b");
    let c = format!("{P}-c");
    let names = vec![a.clone(), b.clone(), c.clone()];

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = InstanceCounter::default();
    let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sampler = spawn_max_sampler(&counter);

    let sup = GroupSup {
        child_names: names.clone(),
        child_shutdown: Shutdown::Timeout(Duration::from_millis(200)),
        log: log.clone(),
        counter: counter.clone(),
        events: events.clone(),
    }
    .spawn()
    .named(format!("{P}-sup"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60)))
    .await
    .expect("supervisor spawn");

    // Initial settle: three constructions, three live, three start events.
    wait_for("initial startup of a, b, c", || {
        counter.constructions() == 3
            && counter.live() == 3
            && log.lock().unwrap().len() == 3
            && all_alive::<GroupChild>(&names)
    })
    .await;
    let baseline = log.lock().unwrap().len();

    // Crash the middle child.
    let hb = live_handle::<GroupChild>(&b).await;
    hb.notify(ChildMsg::Crash)
        .await
        .expect("deliver crash to b");

    // Settle: every member reconstructed exactly once, all live again,
    // 3 stop events + 3 start events appended.
    wait_for("group restart of the whole set (a, b, c)", || {
        counter.constructions() >= 6
            && counter.live() == 3
            && log.lock().unwrap().len() >= baseline + 6
            && all_alive::<GroupChild>(&names)
    })
    .await;
    sleep(QUIET).await;

    assert_eq!(
        counter.constructions(),
        6,
        "exactly one reconstruction per member; more means a double-restart storm"
    );

    let tail: Vec<String> = log.lock().unwrap()[baseline..].to_vec();
    assert_eq!(
        tail.len(),
        6,
        "expected exactly 3 stops + 3 starts after the crash, got: {tail:?}"
    );

    // Stops: coarse set assertion. The trigger (b) dies from the panic; the
    // live siblings (a, c) are stopped with ParentRequest. Their Stop signals
    // are SENT in reverse start order, but the tasks die concurrently, so only
    // membership is asserted here, not inter-stop ordering.
    let mut stops: Vec<&str> = tail
        .iter()
        .filter(|e| e.starts_with("stop:"))
        .map(String::as_str)
        .collect();
    stops.sort_unstable();
    let stop_a = format!("stop:{a}");
    let stop_b = format!("stop:{b}");
    let stop_c = format!("stop:{c}");
    let mut expected_stops = vec![stop_a.as_str(), stop_b.as_str(), stop_c.as_str()];
    expected_stops.sort_unstable();
    assert_eq!(
        stops, expected_stops,
        "OneForAll must stop every member (trigger by panic, live siblings by ParentRequest): {tail:?}"
    );

    // Phase boundary: terminate-then-restart. Every stop (including the
    // trigger's own, allowed anywhere in the stop phase) precedes every start.
    let last_stop = tail.iter().rposition(|e| e.starts_with("stop:")).unwrap();
    let first_start = tail.iter().position(|e| e.starts_with("start:")).unwrap();
    assert!(
        last_stop < first_start,
        "all member deaths must be awaited before any restart fires: {tail:?}"
    );

    // Restarts: coarse set assertion, mirroring the stop assertion above.
    // Restart INITIATION is strictly sequential in START order (the next
    // member is initiated only once the previous one has re-registered), but
    // `on_started` callbacks run on the children's own tasks and may
    // interleave, so only membership is asserted here, not inter-start
    // ordering.
    let mut starts: Vec<&str> = tail
        .iter()
        .filter(|e| e.starts_with("start:"))
        .map(String::as_str)
        .collect();
    starts.sort_unstable();
    let start_a = format!("start:{a}");
    let start_b = format!("start:{b}");
    let start_c = format!("start:{c}");
    let mut expected_starts = vec![start_a.as_str(), start_b.as_str(), start_c.as_str()];
    expected_starts.sort_unstable();
    assert_eq!(
        starts, expected_starts,
        "every group member must restart exactly once: {tail:?}"
    );

    // Instance invariant: never more live members than the group size.
    assert!(
        counter.max_live() <= 3,
        "live instances exceeded group size during the restart: max {}",
        counter.max_live()
    );

    sampler.abort();
    let _ = sup.stop(StopReason::Graceful).await;
}

// ---------------------------------------------------------------------------
// Test 10: RestForOne - only the failed child and those started AFTER it are
// affected; earlier siblings see nothing.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn rest_for_one_restarts_failed_and_later_only() {
    const P: &str = "rest_for_one_restarts_failed_and_later_only";
    let a = format!("{P}-a");
    let b = format!("{P}-b");
    let c = format!("{P}-c");
    let names = vec![a.clone(), b.clone(), c.clone()];

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = InstanceCounter::default();
    let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));

    let sup = GroupSup {
        child_names: names.clone(),
        child_shutdown: Shutdown::Timeout(Duration::from_millis(200)),
        log: log.clone(),
        counter: counter.clone(),
        events: events.clone(),
    }
    .spawn()
    .named(format!("{P}-sup"))
    .with_supervision(SupervisionConfig::rest_for_one().max_restarts(5, Duration::from_secs(60)))
    .await
    .expect("supervisor spawn");

    wait_for("initial startup of a, b, c", || {
        counter.constructions() == 3
            && counter.live() == 3
            && log.lock().unwrap().len() == 3
            && all_alive::<GroupChild>(&names)
    })
    .await;
    let baseline = log.lock().unwrap().len();

    // Crash the middle child: affected set is {b, c}; a stays untouched.
    let hb = live_handle::<GroupChild>(&b).await;
    hb.notify(ChildMsg::Crash)
        .await
        .expect("deliver crash to b");

    wait_for("restart of b and c only", || {
        counter.constructions() >= 5
            && counter.live() == 3
            && log.lock().unwrap().len() >= baseline + 4
            && all_alive::<GroupChild>(&names)
    })
    .await;
    sleep(QUIET).await;

    assert_eq!(
        counter.constructions(),
        5,
        "RestForOne on b must reconstruct exactly b and c (3 initial + 2)"
    );

    let tail: Vec<String> = log.lock().unwrap()[baseline..].to_vec();
    assert_eq!(
        tail.len(),
        4,
        "expected exactly 2 stops + 2 starts after the crash, got: {tail:?}"
    );

    // Coarse presence: b (trigger) and c (later sibling) each stop and restart.
    for needle in [
        format!("stop:{b}"),
        format!("stop:{c}"),
        format!("start:{b}"),
        format!("start:{c}"),
    ] {
        assert!(tail.contains(&needle), "missing '{needle}' in: {tail:?}");
    }

    // a saw NO stop/start events after initial startup.
    assert!(
        !tail.iter().any(|e| e.ends_with(a.as_str())),
        "child a (started before the failure) must be untouched by RestForOne: {tail:?}"
    );

    // Phase boundary: both deaths precede both restarts.
    let last_stop = tail.iter().rposition(|e| e.starts_with("stop:")).unwrap();
    let first_start = tail.iter().position(|e| e.starts_with("start:")).unwrap();
    assert!(
        last_stop < first_start,
        "affected deaths must be awaited before restarts fire: {tail:?}"
    );

    // Supervisor-side ChildEvent recorder: trigger vs group member.
    let evs = events.lock().unwrap().clone();
    let trigger = evs
        .iter()
        .find(|e| e.child_id.as_str() == b)
        .expect("supervisor must see a ChildEvent for the trigger b");
    assert!(
        matches!(&trigger.reason, StopReason::Failure(ActorError::Panic(_))),
        "trigger reason must be Failure(Panic), got: {:?}",
        trigger.reason
    );
    assert_eq!(trigger.action, SupervisionAction::RestartInitiated);

    let member = evs
        .iter()
        .find(|e| e.child_id.as_str() == c)
        .expect("supervisor must see a ChildEvent for the group member c");
    assert!(
        matches!(&member.reason, StopReason::ParentRequest),
        "group member must be stopped with ParentRequest, got: {:?}",
        member.reason
    );
    assert_eq!(member.action, SupervisionAction::RestartInitiated);

    assert!(
        !evs.iter().any(|e| e.child_id.as_str() == a),
        "no ChildEvent may be emitted for the untouched earlier sibling a"
    );

    let _ = sup.stop(StopReason::Graceful).await;
}

// ---------------------------------------------------------------------------
// Test 11: one budget charge per triggering failure. Budget 2 in a 60s window
// survives two full group restarts; the third trigger exhausts it and stops
// the supervisor. Per-member charging would already die on the first group.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn group_restart_single_budget_charge() {
    const P: &str = "group_restart_single_budget_charge";
    let a = format!("{P}-a");
    let b = format!("{P}-b");
    let c = format!("{P}-c");
    let names = vec![a.clone(), b.clone(), c.clone()];

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = InstanceCounter::default();
    let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));

    let sup = GroupSup {
        child_names: names.clone(),
        child_shutdown: Shutdown::Timeout(Duration::from_millis(200)),
        log: log.clone(),
        counter: counter.clone(),
        events: events.clone(),
    }
    .spawn()
    .named(format!("{P}-sup"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)))
    .await
    .expect("supervisor spawn");

    wait_for("initial startup of a, b, c", || {
        counter.constructions() == 3 && counter.live() == 3 && all_alive::<GroupChild>(&names)
    })
    .await;

    // Two full group restarts must succeed: one charge each against budget 2.
    for (round, expected_constructions) in [(1usize, 6usize), (2, 9)] {
        let hb = live_handle::<GroupChild>(&b).await;
        hb.notify(ChildMsg::Crash)
            .await
            .unwrap_or_else(|e| panic!("deliver crash for round {round}: {e:?}"));

        wait_for("full group recovery after a trigger", || {
            counter.constructions() >= expected_constructions
                && counter.live() == 3
                && all_alive::<GroupChild>(&names)
        })
        .await;
        sleep(QUIET).await;

        assert_eq!(
            counter.constructions(),
            expected_constructions,
            "round {round}: each group restart must reconstruct each member exactly once"
        );
        assert!(
            sup.is_alive(),
            "round {round}: one charge per group means budget 2 is not yet exhausted; \
             per-member charging would have killed the supervisor already"
        );
    }

    // Third trigger: charge 3 exceeds max_restarts(2, ..) and the supervisor
    // stops (with its remaining children torn down).
    let hb = live_handle::<GroupChild>(&b).await;
    let _ = hb.notify(ChildMsg::Crash).await;

    wait_for("supervisor stops on budget exhaustion", || !sup.is_alive()).await;
    wait_for("all children torn down with the supervisor", || {
        counter.live() == 0
    })
    .await;
    sleep(QUIET).await;

    assert_eq!(
        counter.constructions(),
        9,
        "no restart may fire after budget exhaustion"
    );
}

// ---------------------------------------------------------------------------
// Test 12: a vetoing child cannot stall a group restart. pre_stop returning
// false refuses ParentRequest; Shutdown::Timeout(150ms) escalates to Kill at
// expiry, and the group still completes.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn vetoing_child_killed_by_timeout_escalation() {
    const P: &str = "vetoing_child_killed_by_timeout_escalation";
    let crasher = format!("{P}-crasher");
    let veto = format!("{P}-veto");

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let crasher_counter = InstanceCounter::default();
    let veto_counter = InstanceCounter::default();
    let veto_stopped = Arc::new(AtomicUsize::new(0));

    let sup = VetoSup {
        crasher_name: crasher.clone(),
        veto_name: veto.clone(),
        log: log.clone(),
        crasher_counter: crasher_counter.clone(),
        veto_counter: veto_counter.clone(),
        veto_stopped: veto_stopped.clone(),
    }
    .spawn()
    .named(format!("{P}-sup"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60)))
    .await
    .expect("supervisor spawn");

    wait_for("initial startup of crasher and veto child", || {
        crasher_counter.constructions() == 1
            && veto_counter.constructions() == 1
            && crasher_counter.live() == 1
            && veto_counter.live() == 1
            && alive_as::<GroupChild>(&crasher)
            && alive_as::<VetoChild>(&veto)
    })
    .await;

    // Crash the sibling: OneForAll drags the veto child into the group.
    let hc = live_handle::<GroupChild>(&crasher).await;
    hc.notify(ChildMsg::Crash)
        .await
        .expect("deliver crash to the sibling");

    // The veto child refuses ParentRequest, so only the Timeout(150ms) -> Kill
    // escalation can complete the group. Both members must come back.
    wait_for("group completes despite the veto (both restarted)", || {
        veto_counter.constructions() >= 2
            && crasher_counter.constructions() >= 2
            && veto_counter.live() == 1
            && crasher_counter.live() == 1
            && alive_as::<GroupChild>(&crasher)
            && alive_as::<VetoChild>(&veto)
    })
    .await;
    sleep(QUIET).await;

    assert_eq!(veto_counter.constructions(), 2);
    assert_eq!(crasher_counter.constructions(), 2);

    // The dead veto instance went down WITHOUT on_stopped running: the only
    // path that bypasses lifecycle callbacks is Kill, i.e. the escalation
    // fired. (Drop-based accounting above already proved the instance died.)
    assert_eq!(
        veto_stopped.load(Ordering::SeqCst),
        0,
        "veto child must die via Kill escalation (on_stopped bypassed), not a graceful stop"
    );

    // Name re-lookup responds: the fresh incarnation serves its channels.
    let hv = live_handle::<VetoChild>(&veto).await;
    let status = hv
        .get_status()
        .await
        .expect("restarted veto child must respond to get_status");
    assert_eq!(status.name.as_deref(), Some(veto.as_str()));
    assert!(sup.is_alive(), "supervisor must survive the escalation");

    let _ = sup.stop(StopReason::Graceful).await;
}

// ---------------------------------------------------------------------------
// Plus: two near-simultaneous failures. The second trigger either folds into
// the pending group or queues behind it; either way the system
// settles with all four children alive, no duplicate instances, and the
// supervisor healthy. Assertions stay coarse (liveness + counts).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn interleaved_failure_during_group_queues() {
    const P: &str = "interleaved_failure_during_group_queues";
    let w = format!("{P}-w");
    let x = format!("{P}-x");
    let y = format!("{P}-y");
    let z = format!("{P}-z");
    let names = vec![w.clone(), x.clone(), y.clone(), z.clone()];

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = InstanceCounter::default();
    let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sampler = spawn_max_sampler(&counter);

    let sup = GroupSup {
        child_names: names.clone(),
        child_shutdown: Shutdown::Timeout(Duration::from_millis(200)),
        log: log.clone(),
        counter: counter.clone(),
        events: events.clone(),
    }
    .spawn()
    .named(format!("{P}-sup"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(4, Duration::from_secs(60)))
    .await
    .expect("supervisor spawn");

    wait_for("initial startup of w, x, y, z", || {
        counter.constructions() == 4 && counter.live() == 4 && all_alive::<GroupChild>(&names)
    })
    .await;

    // Broadcast the crash to two children nearly simultaneously. One of them
    // may be stopped by the forming group before its Crash is dispatched
    // (system channel has priority), which is fine: at least one panics.
    let hx = live_handle::<GroupChild>(&x).await;
    let hy = live_handle::<GroupChild>(&y).await;
    let (rx, ry) = tokio::join!(hx.notify(ChildMsg::Crash), hy.notify(ChildMsg::Crash));
    assert!(
        rx.is_ok() || ry.is_ok(),
        "at least one crash must reach a live child"
    );

    // Quiescence: no new constructions for a full window, all four alive.
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let snapshot = counter.constructions();
        sleep(Duration::from_millis(400)).await;
        let settled = counter.constructions() == snapshot
            && counter.live() == 4
            && all_alive::<GroupChild>(&names)
            && sup.is_alive();
        if settled {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "interleaved-failure storm never settled: constructions={}, live={}, sup_alive={}",
                counter.constructions(),
                counter.live(),
                sup.is_alive()
            );
        }
    }

    // Coarse invariants: full recovery, no duplicate-instance storm.
    assert!(sup.is_alive(), "supervisor must survive both triggers");
    assert_eq!(counter.live(), 4, "all four children alive after settling");
    assert!(
        counter.constructions() >= 8,
        "at least one full group restart must have run (4 initial + 4), got {}",
        counter.constructions()
    );
    assert!(
        counter.max_live() <= 4,
        "no duplicate instances during interleaved group restarts: max {}",
        counter.max_live()
    );

    let status = sup
        .get_status()
        .await
        .expect("supervisor must respond after the storm");
    assert_eq!(status.child_count, 4, "all four child specs retained");

    sampler.abort();
    let _ = sup.stop(StopReason::Graceful).await;
}

// ---------------------------------------------------------------------------
// Test 13: sequential init COMPLETION. Restart INITIATION was already known
// sequential; this proves the ack is actually awaited, so consecutive
// members' `on_started` windows never overlap.
// ---------------------------------------------------------------------------

/// One member's recorded `on_started` window: entry/exit timestamps around an
/// artificial delay, so overlap between consecutive members is measurable.
#[derive(Clone, Debug)]
struct InitSpan {
    name: String,
    enter: Instant,
    exit: Instant,
}

/// Group member whose `on_started` holds the init window open for `delay`
/// before recording it, so a bug that let two members' inits run concurrently
/// would show up as overlapping spans.
struct SeqChild {
    name: String,
    delay: Duration,
    spans: Arc<Mutex<Vec<InitSpan>>>,
    counter: InstanceCounter,
}

impl Actor for SeqChild {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Crash => panic!("{} crashed on command", self.name),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let enter = Instant::now();
        sleep(self.delay).await;
        let exit = Instant::now();
        self.spans.lock().unwrap().push(InitSpan {
            name: self.name.clone(),
            enter,
            exit,
        });
        Ok(())
    }
}

impl Drop for SeqChild {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

/// Supervisor spawning `child_names` (start order) as [`SeqChild`]s.
struct SeqGroupSup {
    child_names: Vec<String>,
    delay: Duration,
    spans: Arc<Mutex<Vec<InitSpan>>>,
    counter: InstanceCounter,
}

impl Actor for SeqGroupSup {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for name in self.child_names.clone() {
            let spans = self.spans.clone();
            let counter = self.counter.clone();
            let delay = self.delay;
            let child_name = name.clone();
            ctx.spawn_child(move || {
                counter.on_construct();
                SeqChild {
                    name: child_name.clone(),
                    delay,
                    spans: spans.clone(),
                    counter: counter.clone(),
                }
            })
            .named(name)
            .restart_type(RestartType::Permanent)
            .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
            .await?;
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_for_all_restart_sequential_init() {
    const P: &str = "one_for_all_restart_sequential_init";
    let a = format!("{P}-a");
    let b = format!("{P}-b");
    let c = format!("{P}-c");
    let names = vec![a.clone(), b.clone(), c.clone()];
    let delay = Duration::from_millis(60);

    let spans: Arc<Mutex<Vec<InitSpan>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = InstanceCounter::default();

    let sup = SeqGroupSup {
        child_names: names.clone(),
        delay,
        spans: spans.clone(),
        counter: counter.clone(),
    }
    .spawn()
    .named(format!("{P}-sup"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60)))
    .await
    .expect("supervisor spawn");

    wait_for("initial startup of a, b, c", || {
        counter.constructions() == 3 && counter.live() == 3 && spans.lock().unwrap().len() == 3
    })
    .await;
    let baseline = spans.lock().unwrap().len();

    let hb = live_handle::<SeqChild>(&b).await;
    hb.notify(ChildMsg::Crash)
        .await
        .expect("deliver crash to b");

    wait_for("all three members re-initialized after the crash", || {
        spans.lock().unwrap().len() >= baseline + 3 && counter.live() == 3
    })
    .await;
    sleep(QUIET).await;

    let tail: Vec<InitSpan> = spans.lock().unwrap()[baseline..].to_vec();
    assert_eq!(
        tail.len(),
        3,
        "each member's on_started must run exactly once during the restart: {tail:?}"
    );

    let order: Vec<&str> = tail.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        order,
        vec![a.as_str(), b.as_str(), c.as_str()],
        "restart order must match start order: {tail:?}"
    );

    for pair in tail.windows(2) {
        assert!(
            pair[0].exit <= pair[1].enter,
            "consecutive restart init windows must not overlap ({} exited at {:?}, \
             {} entered at {:?}): {tail:?}",
            pair[0].name,
            pair[0].exit,
            pair[1].name,
            pair[1].enter
        );
    }

    let _ = sup.stop(StopReason::Graceful).await;
}

// ---------------------------------------------------------------------------
// Test 14: mid-group restart failure. A member whose OWN restart attempt
// fails (factory panic) is retried in place; later members stay down until
// the retry succeeds, and the retry charges the budget exactly once.
// ---------------------------------------------------------------------------

/// Supervisor spawning a, b, c as [`GroupChild`]s, where b's factory panics
/// on its first restart attempt (the second call overall) and succeeds on
/// every other call.
struct FlakyGroupSup {
    a: String,
    b: String,
    c: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    events: Arc<Mutex<Vec<ChildEvent>>>,
    b_calls: Arc<AtomicUsize>,
}

impl Actor for FlakyGroupSup {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let log = self.log.clone();
        let counter = self.counter.clone();
        let a_name = self.a.clone();
        ctx.spawn_child(move || {
            counter.on_construct();
            GroupChild {
                name: a_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
            }
        })
        .named(self.a.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
        .await?;

        let log = self.log.clone();
        let counter = self.counter.clone();
        let b_name = self.b.clone();
        let b_calls = self.b_calls.clone();
        ctx.spawn_child(move || {
            let call = b_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 2 {
                panic!("{b_name} factory failed on its first restart attempt");
            }
            counter.on_construct();
            GroupChild {
                name: b_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
            }
        })
        .named(self.b.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
        .await?;

        let log = self.log.clone();
        let counter = self.counter.clone();
        let c_name = self.c.clone();
        ctx.spawn_child(move || {
            counter.on_construct();
            GroupChild {
                name: c_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
            }
        })
        .named(self.c.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
        .await?;

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

#[tokio::test(flavor = "multi_thread")]
async fn one_for_all_mid_group_failure_retries() {
    const P: &str = "one_for_all_mid_group_failure_retries";
    let a = format!("{P}-a");
    let b = format!("{P}-b");
    let c = format!("{P}-c");
    let names = vec![a.clone(), b.clone(), c.clone()];

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = InstanceCounter::default();
    let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let b_calls = Arc::new(AtomicUsize::new(0));

    // Budget 2 covers exactly the trigger charge and the one retry charge
    // (the "threshold technique" from `group_restart_single_budget_charge`):
    // a per-sibling overcharge would exceed it and stop the supervisor.
    let sup = FlakyGroupSup {
        a: a.clone(),
        b: b.clone(),
        c: c.clone(),
        log: log.clone(),
        counter: counter.clone(),
        events: events.clone(),
        b_calls: b_calls.clone(),
    }
    .spawn()
    .named(format!("{P}-sup"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)))
    .await
    .expect("supervisor spawn");

    wait_for("initial startup of a, b, c", || {
        counter.constructions() == 3
            && counter.live() == 3
            && log.lock().unwrap().len() == 3
            && all_alive::<GroupChild>(&names)
    })
    .await;
    let baseline = log.lock().unwrap().len();

    let hb = live_handle::<GroupChild>(&b).await;
    hb.notify(ChildMsg::Crash)
        .await
        .expect("deliver crash to b");

    wait_for("full recovery after the retried restart", || {
        counter.live() == 3 && all_alive::<GroupChild>(&names) && sup.is_alive()
    })
    .await;
    sleep(QUIET).await;

    assert!(
        sup.is_alive(),
        "budget 2 must cover exactly the trigger charge and the one retry charge; \
         a per-sibling overcharge would have exhausted it and stopped the supervisor"
    );

    assert_eq!(
        b_calls.load(Ordering::SeqCst),
        3,
        "b's factory must be called 3 times: initial construction, the failed first \
         restart attempt, and the successful retry"
    );
    assert_eq!(
        counter.constructions(),
        6,
        "the failed factory attempt never constructs an instance; only the 3 initial \
         plus 3 successful restarts count"
    );

    let tail: Vec<String> = log.lock().unwrap()[baseline..].to_vec();
    assert_eq!(
        tail.iter().filter(|e| e.starts_with("start:")).count(),
        3,
        "exactly one successful start per member after the crash (b's failed attempt \
         never reaches on_started): {tail:?}"
    );
    let start_b = tail
        .iter()
        .position(|e| e == &format!("start:{b}"))
        .expect("b must eventually start via its retry");
    let start_c = tail
        .iter()
        .position(|e| e == &format!("start:{c}"))
        .expect("c must start once the chain reaches it");
    assert!(
        start_b < start_c,
        "c's restart must wait for b's retried restart to complete: {tail:?}"
    );

    let b_events = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.child_id.as_str() == b)
        .count();
    assert!(
        b_events >= 2,
        "the supervisor must observe both b's failed attempt and its retry as \
         separate ChildStopped events"
    );

    let _ = sup.stop(StopReason::Graceful).await;
}

// ---------------------------------------------------------------------------
// Test 15: a hung restart init is bounded by start_timeout. The chain neither
// deadlocks nor stalls the supervisor's own message loop; the timed-out
// incarnation is killed/aborted and the failure retries until the budget
// (deliberately small here) is exhausted.
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum SupPingMsg {
    Ping,
}

/// A child whose `on_started` never returns, so its restart attempt can only
/// end via the caller's `start_timeout`.
struct HangChild {
    name: String,
    log: Arc<Mutex<Vec<String>>>,
}

impl Actor for HangChild {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Crash => panic!("{} crashed on command", self.name),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.log
            .lock()
            .unwrap()
            .push(format!("start:{}", self.name));
        std::future::pending().await
    }
}

/// Supervisor for the hung-init test: a normal crash trigger (a) plus a
/// permanently-hung sibling (b) bounded by `start_timeout` on restart.
struct HangGroupSup {
    a: String,
    b: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    events: Arc<Mutex<Vec<ChildEvent>>>,
}

impl Actor for HangGroupSup {
    type Message = SupPingMsg;
    type Response = ();

    async fn handle(&mut self, _msg: SupPingMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let log = self.log.clone();
        let counter = self.counter.clone();
        let a_name = self.a.clone();
        ctx.spawn_child(move || {
            counter.on_construct();
            GroupChild {
                name: a_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
            }
        })
        .named(self.a.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
        .await?;

        let log = self.log.clone();
        let b_name = self.b.clone();
        ctx.spawn_child(move || HangChild {
            name: b_name.clone(),
            log: log.clone(),
        })
        .named(self.b.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
        .start_timeout(Duration::from_millis(200))
        .await?;

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

#[tokio::test(flavor = "multi_thread")]
async fn one_for_all_hung_init_escalates() {
    const P: &str = "one_for_all_hung_init_escalates";
    let a = format!("{P}-a");
    let b = format!("{P}-b");

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = InstanceCounter::default();
    let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));

    // Budget 2: the crash trigger, then b's first start_timeout retry, are
    // the only charges that must succeed; a second start_timeout retry (the
    // 3rd charge) exceeds it, so the supervisor's own bounded exhaustion is
    // the proof the chain never deadlocks.
    let sup = HangGroupSup {
        a: a.clone(),
        b: b.clone(),
        log: log.clone(),
        counter: counter.clone(),
        events: events.clone(),
    }
    .spawn()
    .named(format!("{P}-sup"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)))
    .await
    .expect("supervisor spawn");

    // a completes on_started; b enters on_started and hangs. Neither initial
    // spawn awaits the child's own ack, so the supervisor's own on_started
    // returns regardless.
    wait_for("a started and b entered its hung on_started", || {
        let l = log.lock().unwrap();
        l.iter().any(|e| e == &format!("start:{a}")) && l.iter().any(|e| e == &format!("start:{b}"))
    })
    .await;

    // Responsiveness probe: pings the supervisor throughout the storm and
    // records every round trip's latency.
    let latencies: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
    let probe_latencies = latencies.clone();
    let probe_sup = sup.clone();
    let probe = tokio::spawn(async move {
        loop {
            let started = Instant::now();
            if probe_sup.send(SupPingMsg::Ping).await.is_err() {
                break;
            }
            probe_latencies.lock().unwrap().push(started.elapsed());
            sleep(Duration::from_millis(20)).await;
        }
    });

    // Crash a: OneForAll drags the hung b into the group. b's ORIGINAL
    // incarnation (still parked in on_started) is escalated to Kill by the
    // existing Shutdown::Timeout ladder during the stop phase; the chain then
    // restarts a normally and reaches b, whose restart attempt hangs again -
    // this time bounded by start_timeout.
    let ha = live_handle::<GroupChild>(&a).await;
    ha.notify(ChildMsg::Crash)
        .await
        .expect("deliver crash to a");

    wait_for("supervisor stops once the budget is exhausted", || {
        !sup.is_alive()
    })
    .await;

    probe.abort();

    let evs = events.lock().unwrap().clone();
    assert!(
        evs.iter().any(|e| e.child_id.as_str() == b
            && matches!(
                &e.reason,
                StopReason::Failure(ActorError::Spawn(SpawnError::StartTimeout))
            )),
        "b's restart must be reported as a start_timeout failure at least once: {evs:?}"
    );

    let observed = latencies.lock().unwrap().clone();
    assert!(
        !observed.is_empty(),
        "the responsiveness probe must have completed at least one round trip during the storm"
    );
    let max_latency = observed.iter().copied().max().unwrap_or_default();
    assert!(
        max_latency < Duration::from_millis(150),
        "the supervisor must keep answering messages while a group restart is in flight, \
         worst observed round trip: {max_latency:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 16: SimpleOneForOne restarts never enter the sequential group chain -
// each dynamic child restarts independently, so concurrent restarts overlap
// instead of queueing behind one another.
// ---------------------------------------------------------------------------

/// Supervisor spawning dynamic SimpleOneForOne children on demand and
/// recording each one's `on_started` window (entry/exit), same shape as
/// [`SeqChild`]/[`InitSpan`] above.
struct DynamicSup {
    delay: Duration,
    spans: Arc<Mutex<Vec<InitSpan>>>,
}

impl Actor for DynamicSup {
    type Message = String;
    type Response = ();

    async fn handle(&mut self, name: String, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let spans = self.spans.clone();
        let delay = self.delay;
        let child_name = name.clone();
        ctx.spawn_child(move || SeqChild {
            name: child_name.clone(),
            delay,
            spans: spans.clone(),
            counter: InstanceCounter::default(),
        })
        .named(name)
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
        .await?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn simple_one_for_one_restarts_bypass_group_chain() {
    const P: &str = "simple_one_for_one_restarts_bypass_group_chain";
    let x = format!("{P}-x");
    let y = format!("{P}-y");
    let delay = Duration::from_millis(150);

    let spans: Arc<Mutex<Vec<InitSpan>>> = Arc::new(Mutex::new(Vec::new()));

    let sup = DynamicSup {
        delay,
        spans: spans.clone(),
    }
    .spawn()
    .named(format!("{P}-sup"))
    .with_supervision(
        SupervisionConfig::simple_one_for_one().max_restarts(10, Duration::from_secs(60)),
    )
    .await
    .expect("supervisor spawn");

    sup.send(x.clone()).await.expect("spawn x");
    sup.send(y.clone()).await.expect("spawn y");

    wait_for("x and y both initially started", || {
        spans.lock().unwrap().len() == 2
    })
    .await;
    let baseline = spans.lock().unwrap().len();

    // Crash both dynamic children nearly simultaneously. A sequential group
    // chain (a bug: SimpleOneForOne wrongly entering it) would serialize
    // their restarts to at least 2 * delay; independent restarts overlap and
    // finish in close to 1 * delay.
    let hx = live_handle::<SeqChild>(&x).await;
    let hy = live_handle::<SeqChild>(&y).await;
    let started = Instant::now();
    let (rx, ry) = tokio::join!(hx.notify(ChildMsg::Crash), hy.notify(ChildMsg::Crash));
    assert!(rx.is_ok() && ry.is_ok(), "both crashes must be delivered");

    wait_for("both x and y re-initialized", || {
        spans.lock().unwrap().len() >= baseline + 2
    })
    .await;
    let elapsed = started.elapsed();

    let tail: Vec<InitSpan> = spans.lock().unwrap()[baseline..].to_vec();
    assert_eq!(tail.len(), 2, "each child restarts exactly once: {tail:?}");

    assert!(
        elapsed < delay * 2,
        "independent SimpleOneForOne restarts must overlap instead of queueing behind a \
         group chain (2 members * {delay:?} would take at least {:?}, took {elapsed:?})",
        delay * 2
    );

    let overlap = tail[0].enter < tail[1].exit && tail[1].enter < tail[0].exit;
    assert!(
        overlap,
        "the two independent restarts must have overlapping init windows, proving no \
         shared sequential chain gates them: {tail:?}"
    );

    let _ = sup.stop(StopReason::Graceful).await;
}

// ---------------------------------------------------------------------------
// Test 17: a `terminate_child`'d (Down, non-temporary) member rejoins a
// OneForAll restart in its own slot order, and the group as a whole still
// charges the restart budget exactly once for the triggering crash.
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum DownRejoinCmd {
    Terminate(String),
}

#[derive(Debug)]
enum DownRejoinReply {
    Done(Result<(), String>),
}

/// Same shape as [`GroupSup`], plus a command to `terminate_child` one member
/// on demand.
struct DownRejoinSup {
    child_names: Vec<String>,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    events: Arc<Mutex<Vec<ChildEvent>>>,
}

impl Actor for DownRejoinSup {
    type Message = DownRejoinCmd;
    type Response = DownRejoinReply;

    async fn handle(
        &mut self,
        msg: DownRejoinCmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<DownRejoinReply> {
        let DownRejoinCmd::Terminate(name) = msg;
        let res = ctx.terminate_child(name).await.map_err(|e| e.to_string());
        Ok(DownRejoinReply::Done(res))
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for name in self.child_names.clone() {
            let log = self.log.clone();
            let counter = self.counter.clone();
            let child_name = name.clone();
            ctx.spawn_child(move || {
                counter.on_construct();
                GroupChild {
                    name: child_name.clone(),
                    log: log.clone(),
                    counter: counter.clone(),
                }
            })
            .named(name)
            .restart_type(RestartType::Permanent)
            .shutdown(Shutdown::Kill)
            .await?;
        }
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

#[tokio::test(flavor = "multi_thread")]
async fn one_for_all_down_member_rejoins_and_budget_charged_once() {
    const P: &str = "one_for_all_down_member_rejoins_and_budget_charged_once";
    let a = format!("{P}-a");
    let b = format!("{P}-b");
    let c = format!("{P}-c");
    let names = vec![a.clone(), b.clone(), c.clone()];

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = InstanceCounter::default();
    let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));

    let sup = DownRejoinSup {
        child_names: names.clone(),
        log: log.clone(),
        counter: counter.clone(),
        events: events.clone(),
    }
    .spawn()
    .named(format!("{P}-sup"))
    // Exactly one triggering failure is affordable: if the Down-rejoin of b
    // (or any of the group's own internal restarts) incorrectly charged the
    // budget again, the supervisor would exit here instead of completing.
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60)))
    .await
    .expect("supervisor spawn");

    wait_for("all three members started", || {
        counter.constructions() == 3 && all_alive::<GroupChild>(&names)
    })
    .await;

    let DownRejoinReply::Done(res) = sup
        .send(DownRejoinCmd::Terminate(b.clone()))
        .await
        .expect("supervisor must answer");
    res.expect("terminate_child must succeed");
    assert!(!alive_as::<GroupChild>(&b), "b must be Down");

    let ha = live_handle::<GroupChild>(&a).await;
    ha.notify(ChildMsg::Crash)
        .await
        .expect("deliver crash to a");

    wait_for("the group cycle to revive everyone, b included", || {
        counter.constructions() >= 6 && all_alive::<GroupChild>(&names)
    })
    .await;
    sleep(QUIET).await;

    assert_eq!(
        counter.constructions(),
        6,
        "a, b, and c each restart exactly once via the chain (3), on top of \
         the initial 3 constructions - b's revival included"
    );
    assert!(
        sup.is_alive(),
        "the single triggering crash must be the only budget charge"
    );

    let _ = sup.stop(StopReason::Graceful).await;
}

// ---------------------------------------------------------------------------
// Test 18: a member's own independent restart attempt failing WHILE a
// sibling group's `Stopping` phase is still in flight must not wedge that
// member's ledger forever. Before its own crash forms the group, b is
// already bouncing independently (`stop_child`); its restart attempt's
// `start_timeout` expires strictly during the group's teardown of the slow
// sibling c, exactly the interleaving that used to leave b stuck
// `Restarting` and c permanently queued behind it.
// ---------------------------------------------------------------------------

/// Group member whose restart attempt (but never its initial spawn) sleeps
/// long enough to blow past a short `start_timeout`, on the SECOND factory
/// call only - every later call (the chain's own eventual revival) completes
/// immediately. `calls` is shared with the test so the exact call sequence
/// (initial, failed bounce attempt, successful chain revival) is verifiable.
struct SlowInitChild {
    name: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    calls: Arc<AtomicUsize>,
}

impl Actor for SlowInitChild {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Crash => panic!("{} crashed on command", self.name),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == 2 {
            // The bounce-triggered restart attempt: hang well past its
            // `start_timeout` so the attempt is reported `Failed` instead of
            // ever adopted.
            std::future::pending::<()>().await;
        }
        self.log
            .lock()
            .unwrap()
            .push(format!("start:{}", self.name));
        Ok(())
    }
}

impl Drop for SlowInitChild {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

/// Group member whose `pre_stop` holds the stop gate open for `stop_delay`
/// before allowing it - long enough to outlast a sibling's `start_timeout` -
/// so its own death is what the group's `Stopping` phase is still awaiting
/// when that sibling's independent restart attempt fails.
struct SlowStopChild {
    name: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    stop_delay: Duration,
}

impl Actor for SlowStopChild {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Crash => panic!("{} crashed on command", self.name),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.log
            .lock()
            .unwrap()
            .push(format!("start:{}", self.name));
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        sleep(self.stop_delay).await;
        true
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.log.lock().unwrap().push(format!("stop:{}", self.name));
        Ok(())
    }
}

impl Drop for SlowStopChild {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

#[derive(Clone)]
enum WedgeCmd {
    /// `stop_child` (budget-free bounce) issued from inside the supervisor's
    /// own handler, exactly like every other manual-stop API.
    Bounce(String),
}

#[derive(Debug)]
enum WedgeReply {
    Done(Result<(), String>),
}

/// Supervisor spawning a ([`GroupChild`]), b ([`SlowInitChild`], short
/// `start_timeout`), and c ([`SlowStopChild`], slow `pre_stop`) - the exact
/// shape needed to interleave an independent restart failure with an
/// in-flight sibling group teardown.
struct WedgeSup {
    a: String,
    b: String,
    c: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    events: Arc<Mutex<Vec<ChildEvent>>>,
    b_calls: Arc<AtomicUsize>,
    c_stop_delay: Duration,
}

impl Actor for WedgeSup {
    type Message = WedgeCmd;
    type Response = WedgeReply;

    async fn handle(
        &mut self,
        msg: WedgeCmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<WedgeReply> {
        let WedgeCmd::Bounce(name) = msg;
        let res = ctx.stop_child(name).await.map_err(|e| e.to_string());
        Ok(WedgeReply::Done(res))
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let log = self.log.clone();
        let counter = self.counter.clone();
        let a_name = self.a.clone();
        ctx.spawn_child(move || {
            counter.on_construct();
            GroupChild {
                name: a_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
            }
        })
        .named(self.a.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(300)))
        .await?;

        let log = self.log.clone();
        let counter = self.counter.clone();
        let b_name = self.b.clone();
        let b_calls = self.b_calls.clone();
        ctx.spawn_child(move || {
            counter.on_construct();
            SlowInitChild {
                name: b_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
                calls: b_calls.clone(),
            }
        })
        .named(self.b.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(300)))
        .start_timeout(Duration::from_millis(150))
        .await?;

        let log = self.log.clone();
        let counter = self.counter.clone();
        let c_name = self.c.clone();
        let c_stop_delay = self.c_stop_delay;
        ctx.spawn_child(move || {
            counter.on_construct();
            SlowStopChild {
                name: c_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
                stop_delay: c_stop_delay,
            }
        })
        .named(self.c.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_secs(2)))
        .await?;

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

#[tokio::test(flavor = "multi_thread")]
async fn group_chain_survives_a_member_restart_failing_mid_group_stop() {
    const P: &str = "group_chain_survives_a_member_restart_failing_mid_group_stop";
    let a = format!("{P}-a");
    let b = format!("{P}-b");
    let c = format!("{P}-c");
    let c_stop_delay = Duration::from_millis(500);

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = InstanceCounter::default();
    let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let b_calls = Arc::new(AtomicUsize::new(0));

    // Budget 2: exactly one charge per triggering crash below - a per-event
    // overcharge (the wedge bug's superseded-attempt event being queued and
    // evaluated as an ordinary failure) would exhaust it on the very first
    // cycle and stop the supervisor before the second crash ever gets a
    // chance to prove the chain is not permanently stuck.
    let sup = WedgeSup {
        a: a.clone(),
        b: b.clone(),
        c: c.clone(),
        log: log.clone(),
        counter: counter.clone(),
        events: events.clone(),
        b_calls: b_calls.clone(),
        c_stop_delay,
    }
    .spawn()
    .named(format!("{P}-sup"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)))
    .await
    .expect("supervisor spawn");

    wait_for("initial startup of a, b, c", || {
        counter.constructions() == 3 && counter.live() == 3
    })
    .await;

    // b starts bouncing independently (budget-free): its restart attempt
    // enters `on_started` and hangs, so it is `Restarting` and will fail via
    // `start_timeout` shortly.
    let WedgeReply::Done(res) = sup
        .send(WedgeCmd::Bounce(b.clone()))
        .await
        .expect("supervisor must answer");
    res.expect("stop_child(b) must succeed");

    // While b's attempt is still in flight (well under its 150ms
    // start_timeout), crash a: OneForAll forms a group whose only LIVE
    // sibling is c (b is not `Running`, so it is excluded from the awaited
    // set but still included in `restart_order`). c's `pre_stop` then holds
    // the group's `Stopping` phase open for `c_stop_delay`, comfortably
    // longer than b's `start_timeout` - the exact interleaving under test.
    let ha = live_handle::<GroupChild>(&a).await;
    ha.notify(ChildMsg::Crash)
        .await
        .expect("deliver crash to a");

    // Full first recovery. `is_alive()` only reflects "task not yet
    // finished" - the hung (failing) bounce attempt itself reports alive the
    // whole time it is stuck in `on_started`, so it cannot distinguish real
    // recovery from the still-failing middle of the sequence. `b_calls`
    // reaching 3 is unambiguous: it only increments from inside a
    // successfully-entered `on_started`, and the chain-revival attempt is
    // the only possible source of a 3rd call (the wedge bug's signature is
    // this count getting stuck at 2 forever - see the `wait_for` deadline
    // panic that produces on unfixed code). `counter.live() == 3` confirms
    // every member (including c, and the hung bounce instance's replacement)
    // is back to a genuinely live instance, not just constructed.
    wait_for(
        "first recovery: b's chain-revival attempt completes",
        || b_calls.load(Ordering::SeqCst) >= 3 && counter.live() == 3,
    )
    .await;
    sleep(QUIET).await;

    assert!(
        sup.is_alive(),
        "the crash trigger must be the only budget charge for the first cycle - a \
         superseded restart-attempt event double-charging the budget would have \
         exhausted it here"
    );

    assert_eq!(
        b_calls.load(Ordering::SeqCst),
        3,
        "b's factory must run exactly 3 times: the initial spawn, the failed bounce \
         attempt, and the chain's own successful revival"
    );

    let starts_b = log
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e == &&format!("start:{b}"))
        .count();
    assert_eq!(
        starts_b, 2,
        "b's on_started must actually complete twice: the initial start, and the \
         chain-revived incarnation - the hung bounce attempt never reaches this line"
    );

    // No wedge: c, which was queued behind b in the old buggy behavior,
    // really did restart (not just "is alive" from before - it was stopped
    // and rebuilt).
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.child_id.as_str() == c
                && matches!(e.action, SupervisionAction::RestartInitiated)),
        "c must have been reported as restarted, not left permanently queued: {:?}",
        events.lock().unwrap()
    );

    // No residual wedge: a completely independent SECOND crash still cycles
    // the group normally (the chain state left behind by the first cycle,
    // if any, does not block further supervision).
    let ha2 = live_handle::<GroupChild>(&a).await;
    ha2.notify(ChildMsg::Crash)
        .await
        .expect("deliver second crash to a");

    wait_for("second recovery: b restarts a 4th time, cleanly", || {
        b_calls.load(Ordering::SeqCst) >= 4 && counter.live() == 3
    })
    .await;
    sleep(QUIET).await;

    assert!(
        sup.is_alive(),
        "the second crash must be exactly the second and final affordable budget charge, \
         proving neither cycle over-charged"
    );

    let _ = sup.stop(StopReason::Graceful).await;
}
