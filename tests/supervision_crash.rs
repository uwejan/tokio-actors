//! Behavioral suite for crash-visible supervision.
//!
//! Written against the v0.7.0 target API:
//! - `.supervisor()` replaces `.supervised()` on `SpawnBuilder` and `ActorConfig`.
//! - A panic in `handle` stops the actor with `StopReason::Failure(ActorError::Panic)`
//!   on BOTH the notify and send paths; supervised children restart per policy.
//! - `send()` surfaces a mid-request handler panic as `AskError::Actor(ActorError::Panic)`.
//! - Budget exhaustion stops the supervisor with `StopReason::ParentRequest`.
//! - `ActorId` is stable across restarts, anonymous children included.
//!
//! All tests use event-based waiting (poll with a deadline) instead of fixed sleeps
//! wherever the condition is observable.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{sleep, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorStatusInfo, ActorSystem, AskError,
    ChildEvent, RestartType, Shutdown, StopReason, StreamEvent, SupervisionAction,
    SupervisionConfig,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type EventLog = Arc<tokio::sync::Mutex<Vec<ChildEvent>>>;

fn recorder() -> EventLog {
    Arc::new(tokio::sync::Mutex::new(Vec::new()))
}

/// Unique registry names so tests can run in parallel within one process.
static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("crash-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
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
        sleep(Duration::from_millis(10)).await;
    }
}

/// Polls the recorder until `pred` holds (or the deadline passes) and returns a snapshot.
async fn wait_for_events<P>(events: &EventLog, timeout_ms: u64, pred: P) -> Vec<ChildEvent>
where
    P: Fn(&[ChildEvent]) -> bool,
{
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        {
            let guard = events.lock().await;
            if pred(&guard) {
                return guard.clone();
            }
        }
        if Instant::now() >= deadline {
            return events.lock().await.clone();
        }
        sleep(Duration::from_millis(10)).await;
    }
}

/// Re-looks a named `PanicWorker` up until a FRESH instance responds (counter == 0).
///
/// A fresh counter doubles as proof of a completed restart: a pre-crash instance
/// with a bumped counter can never satisfy this predicate.
async fn wait_worker_ready(name: &str, timeout_ms: u64) -> Option<ActorHandle<PanicWorker>> {
    let sys = ActorSystem::default();
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if let Some(handle) = sys.get::<PanicWorker>(name) {
            if let Ok(0) = handle.send(WorkerMsg::Count).await {
                return Some(handle);
            }
        }
        sleep(Duration::from_millis(10)).await;
    }
    None
}

/// Re-looks a named actor up until it answers `get_status` (proof of liveness).
async fn wait_status<A: Actor>(name: &str, timeout_ms: u64) -> Option<ActorStatusInfo> {
    let sys = ActorSystem::default();
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if let Some(handle) = sys.get::<A>(name) {
            if let Ok(info) = handle.get_status().await {
                return Some(info);
            }
        }
        sleep(Duration::from_millis(10)).await;
    }
    None
}

fn is_panic_restart(event: &ChildEvent) -> bool {
    matches!(event.reason, StopReason::Failure(ActorError::Panic(_)))
        && event.action == SupervisionAction::RestartInitiated
}

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

/// A worker with a counter (fresh-state probe) that panics on command.
#[derive(Default)]
struct PanicWorker {
    count: u32,
}

#[derive(Clone)]
enum WorkerMsg {
    Bump,
    Count,
    Crash,
}

impl Actor for PanicWorker {
    type Message = WorkerMsg;
    type Response = u32;

    async fn handle(&mut self, msg: WorkerMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        match msg {
            WorkerMsg::Bump => {
                self.count += 1;
                Ok(self.count)
            }
            WorkerMsg::Count => Ok(self.count),
            WorkerMsg::Crash => panic!("worker crashed on command"),
        }
    }
}

/// Panics in `handle` AND in `on_stopped` (double-panic, test 4).
#[derive(Default)]
struct DoublePanicker;

#[derive(Clone)]
enum DoubleMsg {
    Crash,
}

impl Actor for DoublePanicker {
    type Message = DoubleMsg;
    type Response = ();

    async fn handle(&mut self, _msg: DoubleMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("original handle panic");
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        panic!("secondary on_stopped panic");
    }
}

/// Anonymous child that crashes in `handle` on its first two incarnations (test 5).
/// The shared counter is captured by the factory, so it survives restarts.
struct AnonCrasher {
    crashes: Arc<AtomicUsize>,
}

#[derive(Clone)]
enum AnonMsg {
    Tick,
}

impl Actor for AnonCrasher {
    type Message = AnonMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        // Self-trigger so the crash needs no external (name-based) addressing.
        let _ = ctx.self_handle().try_notify(AnonMsg::Tick);
        Ok(())
    }

    async fn handle(&mut self, _msg: AnonMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.crashes.fetch_add(1, Ordering::SeqCst) < 2 {
            panic!("anonymous incarnation crashed");
        }
        Ok(())
    }
}

/// Supervisor-child with custom mailbox capacity (test 6). Crashes on command;
/// on request it spawns an anonymous grandchild and reports its child count.
#[derive(Default)]
struct CfgWorker;

#[derive(Clone)]
enum CfgMsg {
    Crash,
    SpawnGrandchild,
}

impl Actor for CfgWorker {
    type Message = CfgMsg;
    type Response = u32;

    async fn handle(&mut self, msg: CfgMsg, ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        match msg {
            CfgMsg::Crash => panic!("cfg worker crashed"),
            CfgMsg::SpawnGrandchild => {
                ctx.spawn_child(PanicWorker::default).await?;
                Ok(ctx.children().len() as u32)
            }
        }
    }
}

/// Child whose `pre_start` always panics (test 3): drains its supervisor's budget.
#[derive(Default)]
struct PreStartPanicker;

impl Actor for PreStartPanicker {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("pre_start always panics");
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Mid-tier supervisor (test 3): supervises a `PreStartPanicker` so its own
/// restart budget exhausts, forcing it to stop with `StopReason::ParentRequest`.
#[derive(Default)]
struct MidSup;

impl Actor for MidSup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(PreStartPanicker::default)
            .restart_type(RestartType::Permanent)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Temporary child that schedules its own crash (test 8 mass death).
struct TimedCrasher {
    delay_ms: u64,
}

#[derive(Clone)]
enum TimedMsg {
    Crash,
}

impl Actor for TimedCrasher {
    type Message = TimedMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.schedule(TimedMsg::Crash)
            .after(Duration::from_millis(self.delay_ms))
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: TimedMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("timed mass crash");
    }
}

/// Actor with a recurring timer and an attached stream (test 15).
struct Ticker;

#[derive(Clone)]
enum TickMsg {
    Tick,
    Data,
    Done,
    Boom,
}

impl From<StreamEvent<u32>> for TickMsg {
    fn from(event: StreamEvent<u32>) -> Self {
        match event {
            StreamEvent::Data(_) => TickMsg::Data,
            StreamEvent::Finished => TickMsg::Done,
        }
    }
}

impl Actor for Ticker {
    type Message = TickMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.schedule(TickMsg::Tick)
            .every(Duration::from_millis(20))
            .await?;
        ctx.add_stream(tokio_stream::iter(0u32..5_000_000));
        Ok(())
    }

    async fn handle(&mut self, msg: TickMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            TickMsg::Boom => panic!("ticker boom"),
            _ => Ok(()),
        }
    }
}

/// The recording supervisor: pushes every `ChildEvent` into the shared log and
/// spawns children on command (one command variant per test scenario).
struct RecordingSup {
    events: EventLog,
}

#[derive(Clone)]
#[allow(clippy::enum_variant_names)] // Spawn* mirrors the scenario names
enum SupCmd {
    /// Named Permanent `PanicWorker` (tests 1, 2, 7).
    SpawnWorker { name: String },
    /// Named Permanent `DoublePanicker` (test 4).
    SpawnDoublePanicker { name: String },
    /// Anonymous Permanent `AnonCrasher` (test 5).
    SpawnAnonCrasher { crashes: Arc<AtomicUsize> },
    /// Named Permanent `CfgWorker` with mailbox capacity 7 + supervisor role (test 6).
    SpawnCfgWorker { name: String },
    /// Named Transient `MidSup` with a tight restart budget (test 3).
    SpawnMidSup { name: String },
    /// `count` anonymous Temporary `TimedCrasher`s (test 8).
    SpawnCrashers { count: usize, delay_ms: u64 },
}

impl Actor for RecordingSup {
    type Message = SupCmd;
    type Response = ();

    async fn handle(&mut self, msg: SupCmd, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            SupCmd::SpawnWorker { name } => {
                ctx.spawn_child(PanicWorker::default)
                    .named(name)
                    .restart_type(RestartType::Permanent)
                    .shutdown(Shutdown::Timeout(Duration::from_secs(5)))
                    .await?;
            }
            SupCmd::SpawnDoublePanicker { name } => {
                ctx.spawn_child(DoublePanicker::default)
                    .named(name)
                    .restart_type(RestartType::Permanent)
                    .await?;
            }
            SupCmd::SpawnAnonCrasher { crashes } => {
                ctx.spawn_child(move || AnonCrasher {
                    crashes: crashes.clone(),
                })
                .restart_type(RestartType::Permanent)
                .await?;
            }
            SupCmd::SpawnCfgWorker { name } => {
                ctx.spawn_child(CfgWorker::default)
                    .named(name)
                    .restart_type(RestartType::Permanent)
                    .with_config(ActorConfig::default().with_mailbox_capacity(7).supervisor())
                    .await?;
            }
            SupCmd::SpawnMidSup { name } => {
                ctx.spawn_child(MidSup::default)
                    .named(name)
                    .restart_type(RestartType::Transient)
                    .with_config(ActorConfig::default().with_supervision(
                        SupervisionConfig::one_for_one().max_restarts(2, Duration::from_secs(60)),
                    ))
                    .await?;
            }
            SupCmd::SpawnCrashers { count, delay_ms } => {
                for _ in 0..count {
                    ctx.spawn_child(move || TimedCrasher { delay_ms })
                        .restart_type(RestartType::Temporary)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().await.push(event.clone());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn panic_in_handle_notify_path_restarts_child() {
    let events = recorder();
    let sup = RecordingSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-notify"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("notify-worker");
    sup.send(SupCmd::SpawnWorker { name: name.clone() })
        .await
        .unwrap();

    // Dirty the state so the post-restart counter proves freshness.
    let worker = wait_worker_ready(&name, 5_000).await.expect("worker up");
    assert_eq!(worker.send(WorkerMsg::Bump).await.unwrap(), 1);
    assert_eq!(worker.send(WorkerMsg::Bump).await.unwrap(), 2);

    // Crash via the fire-and-forget path: a panic must STOP the actor (new in v0.7).
    worker.notify(WorkerMsg::Crash).await.unwrap();

    let evs = wait_for_events(&events, 10_000, |e| e.iter().any(is_panic_restart)).await;
    let ev = evs
        .iter()
        .find(|e| is_panic_restart(e))
        .unwrap_or_else(|| panic!("expected Failure(Panic) + Restarted, got {evs:?}"));
    assert_eq!(ev.child_name.as_deref(), Some(name.as_str()));
    if let StopReason::Failure(ActorError::Panic(payload)) = &ev.reason {
        assert!(
            payload.contains("worker crashed on command"),
            "panic payload lost: {payload}"
        );
    }

    // Re-lookup via the ActorSystem: restarted child is alive with FRESH state.
    let fresh = wait_worker_ready(&name, 10_000)
        .await
        .expect("restarted worker should re-register under the same name");
    assert_eq!(fresh.send(WorkerMsg::Count).await.unwrap(), 0);
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn panic_in_handle_send_path_returns_panic_error() {
    let events = recorder();
    let sup = RecordingSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-send"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("send-worker");
    sup.send(SupCmd::SpawnWorker { name: name.clone() })
        .await
        .unwrap();

    let worker = wait_worker_ready(&name, 5_000).await.expect("worker up");
    assert_eq!(worker.send(WorkerMsg::Bump).await.unwrap(), 1);

    // The caller of send() must see the panic as a matchable error, not a
    // generic dropped-response. AskError is flattened in v0.7 (no Send wrapper).
    let res = worker.send(WorkerMsg::Crash).await;
    match res {
        Err(AskError::Actor(ActorError::Panic(payload))) => {
            assert!(
                payload.contains("worker crashed on command"),
                "panic payload must reach the caller, got: {payload}"
            );
        }
        other => panic!("expected Err(AskError::Actor(ActorError::Panic)), got {other:?}"),
    }

    // Supervision proceeds concurrently with the caller-side error.
    let evs = wait_for_events(&events, 10_000, |e| e.iter().any(is_panic_restart)).await;
    assert!(
        evs.iter().any(is_panic_restart),
        "supervisor must record Failure(Panic) + Restarted, got {evs:?}"
    );

    let fresh = wait_worker_ready(&name, 10_000)
        .await
        .expect("restarted worker should re-register");
    assert_eq!(fresh.send(WorkerMsg::Count).await.unwrap(), 0);
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn panic_in_pre_start_budget_exhaustion_stops_supervisor_with_parent_request() {
    let events = recorder();
    // Grandparent supervises MidSup as a Transient child.
    let grand = RecordingSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("grandparent"))
    .supervisor()
    .await
    .unwrap();

    let mid_name = uname("mid-sup");
    grand
        .send(SupCmd::SpawnMidSup {
            name: mid_name.clone(),
        })
        .await
        .unwrap();

    // MidSup's child panics in pre_start on every (re)incarnation. Its budget
    // (2 restarts / 60s) exhausts; MidSup must stop with ParentRequest (OTP
    // `shutdown` parity), NOT Failure.
    let evs = wait_for_events(&events, 15_000, |e| {
        e.iter()
            .any(|ev| ev.child_name.as_deref() == Some(mid_name.as_str()))
    })
    .await;
    let ev = evs
        .iter()
        .find(|ev| ev.child_name.as_deref() == Some(mid_name.as_str()))
        .unwrap_or_else(|| panic!("grandparent never observed MidSup stopping: {evs:?}"));

    assert!(
        matches!(ev.reason, StopReason::ParentRequest),
        "budget exhaustion must surface as ParentRequest, got {:?}",
        ev.reason
    );
    assert_eq!(
        ev.action,
        SupervisionAction::Removed,
        "grandparent must NOT restart a Transient child that stopped with ParentRequest"
    );

    // MidSup is really gone: name released, no restart, exactly one event.
    let sys = ActorSystem::default();
    assert!(wait_until(5_000, || sys.get::<MidSup>(&mid_name).is_none()).await);
    sleep(Duration::from_millis(200)).await;
    let final_evs = events.lock().await.clone();
    let mid_events = final_evs
        .iter()
        .filter(|e| e.child_name.as_deref() == Some(mid_name.as_str()))
        .count();
    assert_eq!(mid_events, 1, "no restart may follow: {final_evs:?}");
    assert!(grand.is_alive(), "grandparent must remain alive");
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn panic_in_on_stopped_preserves_original_reason() {
    let events = recorder();
    let sup = RecordingSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-double"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("double-worker");
    sup.send(SupCmd::SpawnDoublePanicker { name: name.clone() })
        .await
        .unwrap();
    assert!(wait_status::<DoublePanicker>(&name, 5_000).await.is_some());

    // Crash: handle panics, then on_stopped panics too (double panic).
    let sys = ActorSystem::default();
    sys.get::<DoublePanicker>(&name)
        .unwrap()
        .notify(DoubleMsg::Crash)
        .await
        .unwrap();

    let evs = wait_for_events(&events, 10_000, |e| !e.is_empty()).await;
    let ev = evs
        .first()
        .unwrap_or_else(|| panic!("supervisor never saw the death"));
    match &ev.reason {
        StopReason::Failure(ActorError::Panic(payload)) => {
            assert!(
                payload.contains("original handle panic"),
                "the ORIGINAL handle panic must be reported, got: {payload}"
            );
            assert!(
                !payload.contains("secondary on_stopped panic"),
                "the on_stopped panic must not mask the root cause: {payload}"
            );
        }
        other => panic!("expected Failure(Panic), got {other:?}"),
    }
    assert_eq!(ev.action, SupervisionAction::RestartInitiated);

    // Restart still happens despite the double panic.
    assert!(
        wait_status::<DoublePanicker>(&name, 10_000).await.is_some(),
        "child must be restarted after a double panic"
    );
    let final_len = events.lock().await.len();
    assert_eq!(final_len, 1, "a double panic is exactly ONE death event");
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_child_restarts_twice_with_stable_id() {
    let events = recorder();
    let sup = RecordingSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-anon"))
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(60)))
    .await
    .unwrap();

    let crashes = Arc::new(AtomicUsize::new(0));
    sup.send(SupCmd::SpawnAnonCrasher {
        crashes: crashes.clone(),
    })
    .await
    .unwrap();

    // Incarnations 1 and 2 crash in handle; incarnation 3 survives.
    let evs = wait_for_events(&events, 10_000, |e| e.len() >= 2).await;
    assert!(evs.len() >= 2, "expected two crash events, got {evs:?}");
    assert_eq!(
        evs[0].child_id, evs[1].child_id,
        "anonymous ActorId must be stable across restarts"
    );
    assert!(is_panic_restart(&evs[0]), "event 1: {:?}", evs[0]);
    assert!(is_panic_restart(&evs[1]), "event 2: {:?}", evs[1]);

    // Third incarnation runs (counter reaches 3) and does not crash.
    assert!(
        wait_until(10_000, || crashes.load(Ordering::SeqCst) >= 3).await,
        "third incarnation should have processed its tick"
    );
    let status = sup.get_status().await.unwrap();
    assert_eq!(status.child_count, 1, "exactly one child, no zombies");
    let final_len = events.lock().await.len();
    assert_eq!(final_len, 2, "no death events beyond the two crashes");
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn config_and_supervisor_role_survive_restart() {
    let events = recorder();
    let sup = RecordingSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-cfg"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("cfg-worker");
    sup.send(SupCmd::SpawnCfgWorker { name: name.clone() })
        .await
        .unwrap();

    let pre = wait_status::<CfgWorker>(&name, 5_000)
        .await
        .expect("cfg worker up");
    assert_eq!(pre.mailbox_capacity, 7, "custom capacity applies on spawn");

    let sys = ActorSystem::default();
    sys.get::<CfgWorker>(&name)
        .unwrap()
        .notify(CfgMsg::Crash)
        .await
        .unwrap();

    let evs = wait_for_events(&events, 10_000, |e| e.iter().any(is_panic_restart)).await;
    assert!(evs.iter().any(is_panic_restart), "got {evs:?}");

    // The restart closure captured the resolved ActorConfig by value:
    // custom mailbox capacity must survive the restart.
    let post = wait_status::<CfgWorker>(&name, 10_000)
        .await
        .expect("restarted cfg worker up");
    assert_eq!(
        post.mailbox_capacity, 7,
        "mailbox capacity must survive restart (no ActorConfig::default() reset)"
    );

    // The supervisor role survives too: the restarted child can spawn a grandchild.
    let handle = sys.get::<CfgWorker>(&name).expect("restarted worker");
    let grandchildren = handle.send(CfgMsg::SpawnGrandchild).await.unwrap();
    assert_eq!(
        grandchildren, 1,
        "restarted child must still be a supervisor with one grandchild"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn named_child_restart_loop_no_nametaken() {
    let events = recorder();
    let sup = RecordingSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-loop"))
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(10, Duration::from_secs(60)))
    .await
    .unwrap();

    let name = uname("loop-worker");
    sup.send(SupCmd::SpawnWorker { name: name.clone() })
        .await
        .unwrap();

    // Five crash/restart rounds. Each round waits for the fresh instance to
    // resolve by name before crashing again; a NameTaken-style restart failure
    // would leave the name unresolvable and fail the round.
    for round in 1..=5usize {
        let worker = wait_worker_ready(&name, 10_000).await.unwrap_or_else(|| {
            panic!("round {round}: worker never became ready again (NameTaken regression?)")
        });
        worker.notify(WorkerMsg::Crash).await.unwrap();

        let evs = wait_for_events(&events, 10_000, |e| e.len() >= round).await;
        assert!(
            evs.len() >= round,
            "round {round}: death event missing, got {evs:?}"
        );
    }

    // After the fifth crash the child must come back once more.
    assert!(wait_worker_ready(&name, 10_000).await.is_some());
    let evs = events.lock().await.clone();
    assert_eq!(evs.len(), 5, "exactly five deaths expected: {evs:?}");
    assert!(
        evs.iter().all(is_panic_restart),
        "every round must be Failure(Panic) + Restarted (no budget/NameTaken noise): {evs:?}"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn mass_death_zero_loss() {
    // More children than the 64-slot system channel: dying watchers must park
    // on awaited sends instead of dropping events.
    const N: usize = 80;

    let events = recorder();
    let sup = RecordingSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-mass"))
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(200, Duration::from_secs(60)))
    .await
    .unwrap();

    // Every child schedules its own crash 500ms after start: near-simultaneous
    // mass death without name-based addressing.
    sup.send(SupCmd::SpawnCrashers {
        count: N,
        delay_ms: 500,
    })
    .await
    .unwrap();

    let evs = wait_for_events(&events, 30_000, |e| e.len() >= N).await;
    assert_eq!(evs.len(), N, "every death must be observed (zero loss)");
    for ev in &evs {
        assert!(
            matches!(ev.reason, StopReason::Failure(ActorError::Panic(_))),
            "unexpected reason: {:?}",
            ev.reason
        );
        assert_eq!(
            ev.action,
            SupervisionAction::Removed,
            "Temporary children are removed, never restarted"
        );
    }
    assert!(sup.is_alive(), "supervisor must survive the mass death");

    // Registry hygiene: all Temporary children pruned, no zombies.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count = sup.get_status().await.unwrap().child_count;
        if count == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "children not pruned from the registry: {count} left"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn forwarders_exit_after_panic() {
    // Unsupervised actor with a recurring timer and an attached stream.
    let handle = Ticker.spawn().await.unwrap();

    // Let both forwarders run for a bit.
    sleep(Duration::from_millis(150)).await;
    assert!(handle.is_alive());

    handle.notify(TickMsg::Boom).await.unwrap();

    // The panic stops the actor; the timer and stream forwarders lose their
    // target and exit. Nothing hangs: the dead handle is observable promptly.
    assert!(
        wait_until(10_000, || !handle.is_alive()).await,
        "actor must be dead after the panic"
    );
    assert!(
        handle.notify(TickMsg::Tick).await.is_err(),
        "messages to the dead actor must fail"
    );
}
