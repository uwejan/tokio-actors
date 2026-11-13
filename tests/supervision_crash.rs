#![allow(dead_code)]
//! Behavioral suite for crash-visible supervision, including
//! Err-cast-exception parity on the notify path.
//!
//! Written against the v0.7.0 target API:
//! - `.supervisor()` replaces `.supervised()` on `SpawnBuilder` and `ActorConfig`.
//! - A panic in `handle` stops the actor with `StopReason::Failure(ActorError::Panic)`
//!   on BOTH the notify and send paths; supervised children restart per policy.
//! - A returned `Err` (no panic) stops the actor identically on BOTH paths too:
//!   `Actor::handle_failure` is gone, there is no notify-path limp mode.
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
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorStatusInfo, ActorSystem, ChildEvent,
    RestartType, Shutdown, StopReason, StreamEvent, SupervisionAction, SupervisionConfig,
};

mod common;

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

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[test]
fn panic_in_on_stopped_preserves_original_reason() {
    common::run(0, async {
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
    });
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

/// Shared slot a `PoisonWorker`'s `on_stopped` writes into, so a test can
/// inspect the `StopReason` the actor itself observed.
type StopLog = Arc<tokio::sync::Mutex<Option<StopReason>>>;

/// A worker with a counter (fresh-state probe) that returns `Err` (not a
/// panic) from `handle` on command, and records every `on_stopped` reason.
struct PoisonWorker {
    count: u32,
    stopped: StopLog,
}

#[derive(Clone)]
enum PoisonMsg {
    Bump,
    Count,
    /// Returns `Err` from `handle` (no panic): exercises the Err path of
    /// cast-exception parity, distinct from the panic path covered above.
    Poison,
}

impl Actor for PoisonWorker {
    type Message = PoisonMsg;
    type Response = u32;

    async fn handle(&mut self, msg: PoisonMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        match msg {
            PoisonMsg::Bump => {
                self.count += 1;
                Ok(self.count)
            }
            PoisonMsg::Count => Ok(self.count),
            PoisonMsg::Poison => Err(ActorError::user("poisoned message")),
        }
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.stopped.lock().await = Some(reason.clone());
        Ok(())
    }
}

/// Spawns a single named Permanent `PoisonWorker` child in `on_started` and
/// records every `ChildEvent`. A standalone supervisor type (rather than a
/// new `SupCmd` variant on `RecordingSup`) keeps the addition self-contained.
struct PoisonSupervisor {
    events: EventLog,
    child_name: String,
}

impl Actor for PoisonSupervisor {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| PoisonWorker {
            count: 0,
            stopped: Arc::new(tokio::sync::Mutex::new(None)),
        })
        .named(self.child_name.clone())
        .restart_type(RestartType::Permanent)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
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

/// Re-looks a named `PoisonWorker` up until a FRESH instance responds (counter == 0).
async fn wait_poison_ready(name: &str, timeout_ms: u64) -> Option<ActorHandle<PoisonWorker>> {
    let sys = ActorSystem::default();
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if let Some(handle) = sys.get::<PoisonWorker>(name) {
            if let Ok(0) = handle.send(PoisonMsg::Count).await {
                return Some(handle);
            }
        }
        sleep(Duration::from_millis(10)).await;
    }
    None
}

fn is_err_restart(event: &ChildEvent) -> bool {
    matches!(event.reason, StopReason::Failure(ActorError::User(_)))
        && event.action == SupervisionAction::RestartInitiated
}

#[test]
fn err_from_handle_notify_path_stops_actor_like_send() {
    common::run(0, async {
        let stopped: StopLog = Arc::new(tokio::sync::Mutex::new(None));
        let handle = PoisonWorker {
            count: 0,
            stopped: stopped.clone(),
        }
        .spawn()
        .await
        .unwrap();

        // notify (fire-and-forget): under the removed `handle_failure` limp mode
        // this would have continued running. v0.8.0 makes a returned `Err`
        // stop the actor identically on the notify and send paths.
        handle.notify(PoisonMsg::Poison).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), handle.wait_stopped())
            .await
            .expect("wait_stopped must resolve after a notify-path Err (cast-exception parity)");
        // wait_stopped resolves on the status plane's terminal state; is_alive
        // (mailbox closure) can lag it by one teardown beat under heavy
        // instrumentation, so poll briefly instead of asserting instantly.
        assert!(
            wait_until(2_000, || !handle.is_alive()).await,
            "actor must be dead after the poison notify"
        );

        let observed = stopped.lock().await.take();
        match observed {
            Some(StopReason::Failure(ActorError::User(msg))) => {
                assert!(msg.contains("poisoned"), "unexpected message: {msg}");
            }
            other => {
                panic!("expected on_stopped to observe StopReason::Failure(User), got {other:?}")
            }
        }
    });
}

// ---------------------------------------------------------------------------
// A restart in flight is torn down along with its parent: killing the
// supervisor while a fresh incarnation is stuck in `on_started` still frees
// the child's registered name (a dedicated `ActorSystem` isolates this test's
// reaper task from any other test's, so its short link-guard grace is never
// starved by unrelated concurrent activity on the shared default system).
// ---------------------------------------------------------------------------

/// A child whose SECOND construction (the crash's own restart attempt) hangs
/// forever in `on_started`; every other construction returns immediately.
struct HungRestartWorker {
    hang: bool,
}

#[derive(Clone)]
enum HungMsg {
    Crash,
}

impl Actor for HungRestartWorker {
    type Message = HungMsg;
    type Response = ();

    async fn handle(&mut self, msg: HungMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            HungMsg::Crash => panic!("hung-restart worker crashed on command"),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.hang {
            std::future::pending().await
        } else {
            Ok(())
        }
    }
}

struct HungRestartSup {
    name: String,
    constructions: Arc<AtomicUsize>,
}

impl Actor for HungRestartSup {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let constructions = self.constructions.clone();
        ctx.spawn_child(move || {
            let n = constructions.fetch_add(1, Ordering::SeqCst);
            HungRestartWorker { hang: n == 1 }
        })
        .named(self.name.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Kill)
        .await?;
        Ok(())
    }
}
