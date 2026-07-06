//! Behavioral suite for the OneForAll / RestForOne group-restart machinery,
//! including the interleaved-failure storm:
//!
//! - When one child crashes (panic in `handle`), the runtime stops the affected
//!   LIVE siblings with `StopReason::ParentRequest` (OneForAll: all of them;
//!   RestForOne: only those started AFTER the failed child), awaits their
//!   watcher-delivered deaths, then restarts the whole affected set in START
//!   order. Children outside the affected set are untouched.
//! - ONE budget charge per triggering failure; group-member deaths during the
//!   group do NOT charge (`group_restart_single_budget_charge`).
//! - `Shutdown::Timeout(d)` escalates to Kill at expiry, so a `pre_stop` veto
//!   cannot stall a group forever (`vetoing_child_killed_by_timeout_escalation`).
//! - No duplicate live instances during restart storms (incarnation guard),
//!   proven with factory-increment / Drop-decrement instance accounting. The
//!   decrement lives in `Drop`, NOT `on_stopped`, because the Kill escalation
//!   path bypasses all lifecycle callbacks while `Drop` always runs.
//!
//! Idioms: multi_thread runtime, per-test-unique actor names (the default
//! `ActorSystem` registry is process-global and tests run concurrently), and
//! event-based waiting (bounded polling) instead of long fixed sleeps.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::sleep;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorSystem, ChildEvent, RestartType, Shutdown,
    StopReason, SupervisionAction, SupervisionConfig,
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

    // Restarts: ordering-strict. START order a, b, c per OTP
    // "processes are restarted left to right".
    let starts: Vec<&str> = tail
        .iter()
        .filter(|e| e.starts_with("start:"))
        .map(String::as_str)
        .collect();
    let start_a = format!("start:{a}");
    let start_b = format!("start:{b}");
    let start_c = format!("start:{c}");
    assert_eq!(
        starts,
        vec![start_a.as_str(), start_b.as_str(), start_c.as_str()],
        "group members must restart in START order: {tail:?}"
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
