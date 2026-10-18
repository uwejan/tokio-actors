//! The boundary table: every duration and capacity parameter of the public
//! API, crossed with the values that sit on the edges of what it accepts.
//!
//! Durations are crossed with {0, 1 ns, 1 ms, 50 ms, 30 years minus 1 ns,
//! 30 years, the largest duration `Instant::checked_add` accepts from now
//! (less 500 us, so it stays inside the last millisecond before the edge of
//! the representable range), `Duration::MAX`}. Counts are crossed with their
//! own edge values. Each `#[test]` loops over the whole value set of one
//! parameter group; every failure message names the parameter and the value.
//! The tests run one at a time, so a panic raised in a spawned task is
//! reported by the test that caused it.
//!
//! What a cell asserts:
//! - A deadline built from a caller duration is capped at 30 years, so a
//!   value of 30 years or more fires at the cap and not before, a smaller one
//!   is plain addition, and nothing panics. Each large value is also run on a
//!   paused clock that advances to just under the cap (nothing fired) and past
//!   it (the capped behavior).
//! - A zero duration fires or times out at once.
//! - A zero recurring interval answers `TimerError::ZeroInterval` and
//!   registers nothing.
//! - A zero kill grace aborts at once and is not an unresponsive verdict.
//! - A supervisor child under a killed parent stops in order inside its own
//!   `Shutdown::Timeout` window: a small window ends it and its vetoing
//!   worker, and under a window of 30 years or more both are still running
//!   after a quiet period, until the child is killed.
//! - A turn warning threshold of zero reads off; one beyond `u64::MAX`
//!   nanoseconds saturates.
//! - A zero mailbox or priority lane capacity answers
//!   `SpawnError::ZeroMailboxCapacity`; one above `Semaphore::MAX_PERMITS` is
//!   clamped to it.
//! - `max_restarts` of 0 refuses the first restart the strategy would make,
//!   whatever the window (a `Temporary` child's failure makes none and leaves
//!   the supervisor running); `u32::MAX` never exhausts; a restart window that
//!   is never shorter than 30 years minus 1 ns never prunes.
//! - A restart counts while it is no older than the window, the end of the
//!   window included, so a zero window counts only restarts that read the same
//!   instant: on a paused clock that does not move it exhausts a budget after
//!   `max_restarts` restarts, and on a paused clock that auto-advances to a
//!   pending timer it keeps restarting like a running one. On a running clock
//!   the outcome is not asserted.
//!
//! A cell whose documentation states no outcome (a restart window of 0, 1 ns,
//! 1 ms or 50 ms on a running clock) asserts that nothing panics and prints
//! what it saw.
//!
//! Where an earlier test already pins a cell, that test is named here:
//! - `send_timeout`, `send_priority_timeout`: this file; the shared
//!   deadline helper is also pinned by the unit test
//!   `saturating_deadline_always_leaves_timer_headroom`.
//! - `every`, `every_with` (zero interval): this file and
//!   `zero_interval_is_rejected_at_registration` (tests/timers.rs).
//! - `schedule(..).after`, `schedule(..).at`: this file.
//! - `stop_within` with `Graceful`: this file and
//!   `edge_of_range_within_does_not_panic` (tests/stop_within.rs); with
//!   `Kill`: this file and `stop_within_kill_at_the_edge_of_range_does_not_panic`
//!   (tests/supervision_matrix.rs).
//! - `SpawnBuilder::kill_grace`, `ChildSpawnBuilder::kill_grace`: this file
//!   and `edge_of_range_kill_grace_never_panics` (tests/kill_grace.rs); a
//!   lost runner's wait with a saturated grace: the unit test
//!   `lost_runner_waits_cap_a_saturated_kill_grace`.
//! - `SpawnBuilder::start_timeout`, `ChildSpawnBuilder::start_timeout`: this
//!   file.
//! - `Shutdown::Timeout` on a worker child: this file and
//!   `edge_of_range_child_shutdown_timeout_does_not_panic` (tests/cascade.rs);
//!   the inherited stop of an edge-of-range window:
//!   `inherited_stop_with_an_edge_of_range_shutdown_does_not_panic`
//!   (tests/cascade.rs); a subtree budget that saturates: the unit test
//!   `huge_budgets_saturate_to_unbounded_instead_of_wrapping`; on a supervisor
//!   child under a killed parent, and the nine pairs with
//!   `kill_grace` through a killed supervisor's restart: this file (the nine
//!   pairs on a running clock only).
//! - `ShutdownPolicy::timeout`, `ShutdownPolicy::per_actor_timeout`: this
//!   file. On the paused clock the global row tells its bound apart (two roots);
//!   the per-actor row shares its cap with the global one, and only the
//!   running-clock rows tell the two apart.
//! - `SystemConfig::turn_warning_threshold`, `set_turn_warning_threshold`:
//!   this file.
//! - The restart window of `SupervisionConfig::max_restarts`: this file.
//! - Mailbox and priority lane capacity: this file and
//!   `oversized_capacities_are_clamped_not_panicking` (tests/config_builder.rs).

use std::future::{Future, IntoFuture};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::{advance, sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorStatus, ActorSystem, AskError,
    MissPolicy, RecurringId, RestartType, Shutdown, ShutdownPolicy, SpawnError, StopOutcome,
    StopReason, StopReport, SupervisionConfig, SystemConfig, TimerError,
};

mod common;

// ---------------------------------------------------------------------------
// Values, bounds and helpers
// ---------------------------------------------------------------------------

const THIRTY_YEARS: Duration = Duration::from_secs(86_400 * 365 * 30);

/// The bound on every outcome a test awaits that no configured duration
/// derives.
const BOUND: Duration = Duration::from_secs(10);

/// How long a cell that must not fire is observed on a running clock.
const QUIET: Duration = Duration::from_millis(250);

/// The wait after an abort before the unresponsive verdict, and the margin on
/// top of a configured grace. An integration test cannot read the crate's
/// constants, so each value is named here once.
const ASSUMED_ABORT_SETTLE: Duration = Duration::from_millis(100);
const MARGIN: Duration = Duration::from_millis(500);

/// The upper bound of a timed outcome: `precise` where an outcome lands on
/// its exact instant (the paused clock), and a hang bound past it on a
/// real-time runtime, loose enough for a host that stalls its threads.
fn upper(paused: bool, precise: Duration) -> Duration {
    if paused {
        precise
    } else {
        precise + BOUND
    }
}

/// How far below the 30 year cap a paused clock stops to check that nothing
/// fired, and how far past it the clock then moves.
const CAP_SLACK: Duration = Duration::from_secs(1);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("boundaries-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

/// The prefix of every failure message: the parameter and the value.
fn cell(parameter: &str, label: &str) -> String {
    let row = format!("parameter {parameter}, value {label}");
    *CURRENT_ROW
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = row.clone();
    row
}

/// The row most recently started: a panic raised in a thread that cannot name
/// its row, such as the test's own, is reported with it.
static CURRENT_ROW: Mutex<String> = Mutex::new(String::new());

/// Stands for the largest duration `Instant::checked_add` accepts, less
/// 500 us, measured at the moment a call receives it: `resolve` turns it into
/// that duration. The margin below the limit is what makes the sum land in
/// the last millisecond before the end of the representable range, where a
/// timer that rounds a deadline up to the next millisecond cannot represent
/// the result; measuring late keeps the clock from moving past that margin
/// between the measurement and the call.
const EDGE: Duration = Duration::new(u64::MAX / 4, 7);

/// The edge duration, measured now.
fn edge_now() -> Duration {
    largest_addable_from(Instant::now())
}

/// The edge duration with `step` further margins of 900 us below it, measured
/// now. The margins cover a call that reads its clock a few milliseconds
/// after this was measured.
fn edge_at(step: u32) -> Duration {
    edge_now().saturating_sub(Duration::from_micros(900) * step)
}

/// How many margins a sweep over the edge covers.
const EDGE_STEPS: u32 = 16;

fn resolve(d: Duration) -> Duration {
    if d == EDGE {
        edge_now()
    } else {
        d
    }
}

fn resolve_shutdown(shutdown: Shutdown) -> Shutdown {
    match shutdown {
        Shutdown::Timeout(d) => Shutdown::Timeout(resolve(d)),
        other => other,
    }
}

fn largest_addable_from(base: Instant) -> Duration {
    let mut lo = Duration::ZERO;
    let mut hi = Duration::MAX;
    while hi - lo > Duration::from_nanos(1) {
        let mid = lo + (hi - lo) / 2;
        if base.checked_add(mid).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo.saturating_sub(Duration::from_micros(500))
}

/// The duration value set, each with the label a failure message prints.
fn durations() -> Vec<(&'static str, Duration)> {
    vec![
        ("0", Duration::ZERO),
        ("1 ns", Duration::from_nanos(1)),
        ("1 ms", Duration::from_millis(1)),
        ("50 ms", Duration::from_millis(50)),
        (
            "30 years minus 1 ns",
            THIRTY_YEARS - Duration::from_nanos(1),
        ),
        ("30 years", THIRTY_YEARS),
        ("largest addable", EDGE),
        ("Duration::MAX", Duration::MAX),
    ]
}

/// The values that reach or pass the 30 year cap.
fn large() -> Vec<(&'static str, Duration)> {
    durations()
        .into_iter()
        .filter(|(_, d)| *d >= THIRTY_YEARS - Duration::from_nanos(1))
        .collect()
}

/// A value that fires, or times out, inside the observation window.
fn is_small(d: Duration) -> bool {
    d <= Duration::from_millis(50)
}

/// Awaits `fut` for at most `bound`; an outcome that never arrives fails the
/// test here, naming the row, instead of hanging it.
async fn within_for<T>(
    bound: Duration,
    at: &str,
    what: &str,
    fut: impl IntoFuture<Output = T>,
) -> T {
    match timeout(bound, fut).await {
        Ok(value) => value,
        Err(_) => panic!("{at}: timed out after {bound:?} waiting for: {what}"),
    }
}

async fn within<T>(at: &str, what: &str, fut: impl IntoFuture<Output = T>) -> T {
    within_for(BOUND, at, what, fut).await
}

async fn wait_until(at: &str, what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + BOUND;
    loop {
        if pred() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{at}: timed out after {BOUND:?} waiting for: {what}"
        );
        sleep(Duration::from_millis(2)).await;
    }
}

/// A bounded request to an actor; the answer must be a success.
async fn ask<A: Actor>(
    at: &str,
    what: &str,
    handle: &ActorHandle<A>,
    msg: A::Message,
) -> A::Response
where
    A::Message: 'static,
{
    match within(at, what, handle.send(msg)).await {
        Ok(answer) => answer,
        Err(err) => panic!("{at}: {what}: the request failed: {err:?}"),
    }
}

/// A bounded stop whose refusal (the actor is already gone) is not an error.
async fn stop_quietly<A: Actor>(at: &str, handle: &ActorHandle<A>, reason: StopReason) {
    let _ = within(at, "a stop request", handle.stop(reason)).await;
}

// ---- Panics ----------------------------------------------------------------

/// The tests of this file run one at a time, so the panics recorded below
/// belong to the test that holds the lock.
static SERIAL: Mutex<()> = Mutex::new(());

/// Panics raised anywhere but in this file's own assertions: a panic inside a
/// spawned task is caught by the runtime, so only a hook sees every one.
static PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn drain_panics() -> Vec<String> {
    std::mem::take(
        &mut *PANICS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

/// Takes the lock that serializes the tests and starts an empty panic record.
fn begin() -> MutexGuard<'static, ()> {
    static ONCE: Once = Once::new();
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let own = info
                .location()
                .is_some_and(|place| place.file().ends_with("boundaries.rs"));
            if !own {
                let row = CURRENT_ROW
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                eprintln!("[boundaries] a panic was raised after starting: {row}");
                PANICS
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(format!("{info} (after starting: {row})"));
            }
            previous(info);
        }));
    });
    let left = drain_panics();
    assert!(
        left.is_empty(),
        "a panic was recorded after the previous test's last check: {left:?}"
    );
    guard
}

/// Fails if a panic escaped since the previous check.
fn assert_no_panic(at: &str) {
    let seen = drain_panics();
    assert!(seen.is_empty(), "{at}: a panic escaped: {seen:?}");
}

// ---- Paused clock ------------------------------------------------------------

/// Runs `body` on a current-thread runtime whose clock is paused.
fn run_paused<F: Future<Output = ()>>(body: F) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("failed to build a paused current-thread runtime")
        .block_on(body);
}

/// Lets every ready task run, without moving the clock.
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

/// Runs the paused runtime until `pred` holds. The clock moves 5 ms after
/// every 100 yields, so a step that waits on a short timer (a kill grace)
/// still completes; a caller that needs a frozen clock does not use this.
async fn drive(at: &str, what: &str, mut pred: impl FnMut() -> bool) {
    for round in 0..20_000 {
        if pred() {
            return;
        }
        tokio::task::yield_now().await;
        if round % 100 == 99 {
            advance(Duration::from_millis(5)).await;
        }
    }
    panic!("{at}: the paused runtime never reached: {what}");
}

/// Moves a paused clock to just under the 30 year cap and checks that
/// `fired` is still false, then past the cap.
async fn pass_the_cap(at: &str, fired: impl Fn() -> bool) {
    // Whatever was just spawned reads the clock and registers its timer first.
    settle().await;
    advance(THIRTY_YEARS - CAP_SLACK).await;
    settle().await;
    assert!(
        !fired(),
        "{at}: fired before the 30 year cap ({CAP_SLACK:?} short of it)"
    );
    advance(CAP_SLACK * 2).await;
    settle().await;
}

// ---------------------------------------------------------------------------
// The targets
// ---------------------------------------------------------------------------

/// What the tests observe and steer in the actors below.
struct Shared {
    /// A parked turn waits here; a test opens it by adding permits.
    gate: Semaphore,
    /// An initialization held by `hang_init_from` waits here.
    init_gate: Semaphore,
    parked: AtomicBool,
    /// Workers built, counting the one a restart builds.
    built: AtomicUsize,
    /// Supervisors started.
    sups: AtomicUsize,
    /// A worker built as the n-th or later hangs in `on_started`.
    hang_init_from: AtomicUsize,
    /// Every worker fails by a timer one millisecond after it starts.
    crash_after_start: AtomicBool,
    /// `pre_stop` calls on every worker, a veto or not.
    asked: AtomicUsize,
}

impl Shared {
    fn new() -> Arc<Shared> {
        Arc::new(Shared {
            gate: Semaphore::new(0),
            init_gate: Semaphore::new(0),
            parked: AtomicBool::new(false),
            built: AtomicUsize::new(0),
            sups: AtomicUsize::new(0),
            hang_init_from: AtomicUsize::new(usize::MAX),
            crash_after_start: AtomicBool::new(false),
            asked: AtomicUsize::new(0),
        })
    }

    fn open_gates(&self) {
        self.gate.add_permits(1_000_000);
        self.init_gate.add_permits(1_000_000);
    }
}

#[derive(Clone, Copy, Debug)]
enum Msg {
    Ping,
    /// Parks the turn at the gate.
    Park,
    /// Fails, which stops the incarnation.
    Crash,
}

/// A worker that can park, fail, veto a stop and hang its initialization.
struct Worker {
    shared: Arc<Shared>,
    veto: bool,
}

impl Worker {
    fn new(shared: &Arc<Shared>, veto: bool) -> Self {
        shared.built.fetch_add(1, Ordering::SeqCst);
        Worker {
            shared: shared.clone(),
            veto,
        }
    }
}

impl Actor for Worker {
    type Message = Msg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let built = self.shared.built.load(Ordering::SeqCst);
        if built >= self.shared.hang_init_from.load(Ordering::SeqCst) {
            if let Ok(permit) = self.shared.init_gate.acquire().await {
                permit.forget();
            }
        }
        if self.shared.crash_after_start.load(Ordering::SeqCst) {
            ctx.schedule(Msg::Crash)
                .after(Duration::from_millis(1))
                .await
                .map_err(|err| ActorError::user(err.to_string()))?;
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Ping => Ok(()),
            Msg::Park => {
                self.shared.parked.store(true, Ordering::SeqCst);
                if let Ok(permit) = self.shared.gate.acquire().await {
                    permit.forget();
                }
                Ok(())
            }
            Msg::Crash => Err(ActorError::user("failed on command")),
        }
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.shared.asked.fetch_add(1, Ordering::SeqCst);
        !self.veto
    }
}

/// A supervisor that spawns worker and supervisor children on command.
struct Sup {
    shared: Arc<Shared>,
}

#[derive(Clone)]
enum Cmd {
    Worker {
        name: String,
        grace: Duration,
        shutdown: Shutdown,
        restart: RestartType,
        veto: bool,
        start_timeout: Option<Duration>,
    },
    Sup {
        name: String,
        grace: Duration,
        shutdown: Shutdown,
        restart: RestartType,
    },
    /// A temporary worker child with its own configuration.
    Sized { name: String, config: ActorConfig },
}

impl Actor for Sup {
    type Message = Cmd;
    type Response = Result<(), String>;

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.shared.sups.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(
        &mut self,
        cmd: Cmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        let shared = self.shared.clone();
        match cmd {
            Cmd::Worker {
                name,
                grace,
                shutdown,
                restart,
                veto,
                start_timeout,
            } => {
                let mut builder = ctx
                    .spawn_child(move || Worker::new(&shared, veto))
                    .named(name)
                    .restart_type(restart)
                    .shutdown(resolve_shutdown(shutdown))
                    .kill_grace(resolve(grace));
                if let Some(bound) = start_timeout {
                    builder = builder.start_timeout(resolve(bound));
                }
                Ok(builder.await.map(|_| ()).map_err(|err| err.to_string()))
            }
            Cmd::Sized { name, config } => Ok(ctx
                .spawn_child(move || Worker::new(&shared, false))
                .named(name)
                .restart_type(RestartType::Temporary)
                .with_config(config)
                .await
                .map(|_| ())
                .map_err(|err| err.to_string())),
            Cmd::Sup {
                name,
                grace,
                shutdown,
                restart,
            } => Ok(ctx
                .spawn_child(move || Sup {
                    shared: shared.clone(),
                })
                .named(name)
                .restart_type(restart)
                .shutdown(resolve_shutdown(shutdown))
                .kill_grace(resolve(grace))
                .with_config(ActorConfig::default().supervisor())
                .await
                .map(|_| ())
                .map_err(|err| err.to_string())),
        }
    }
}

async fn spawn_sup(
    at: &str,
    sys: &Arc<ActorSystem>,
    shared: &Arc<Shared>,
    supervision: Option<SupervisionConfig>,
) -> ActorHandle<Sup> {
    let builder = Sup {
        shared: shared.clone(),
    }
    .spawn()
    .named(uname("sup"))
    .on_system(sys);
    let spawned = match supervision {
        Some(config) => {
            within(
                at,
                "a supervisor to start",
                builder.with_supervision(config),
            )
            .await
        }
        None => within(at, "a supervisor to start", builder.supervisor()).await,
    };
    spawned.unwrap_or_else(|err| panic!("{at}: the supervisor did not start: {err}"))
}

async fn spawn_worker(at: &str, shared: &Arc<Shared>, veto: bool) -> ActorHandle<Worker> {
    within(at, "a worker to start", Worker::new(shared, veto).spawn())
        .await
        .unwrap_or_else(|err| panic!("{at}: the worker did not start: {err}"))
}

async fn ask_spawn(at: &str, sup: &ActorHandle<Sup>, cmd: Cmd) {
    if let Err(err) = ask(at, "the supervisor to answer a spawn", sup, cmd).await {
        panic!("{at}: the child did not spawn: {err}");
    }
}

fn worker_cmd(name: &str, grace: Duration, shutdown: Shutdown, restart: RestartType) -> Cmd {
    Cmd::Worker {
        name: name.to_string(),
        grace,
        shutdown,
        restart,
        veto: false,
        start_timeout: None,
    }
}

async fn registered(at: &str, sys: &Arc<ActorSystem>, name: &str) -> ActorHandle<Worker> {
    let mut found = None;
    wait_until(at, "the actor to be registered", || {
        found = sys.get::<Worker>(name);
        found.is_some()
    })
    .await;
    found.unwrap()
}

async fn park(at: &str, handle: &ActorHandle<Worker>, shared: &Arc<Shared>) {
    shared.parked.store(false, Ordering::SeqCst);
    within(at, "the park request", handle.notify(Msg::Park))
        .await
        .unwrap_or_else(|err| panic!("{at}: the park request was refused: {err:?}"));
    wait_until(at, "the turn to park", || {
        shared.parked.load(Ordering::SeqCst)
    })
    .await;
}

// ---------------------------------------------------------------------------
// Send timeouts
// ---------------------------------------------------------------------------

/// Parks its first turn at the gate; every other message answers at once.
struct Gated {
    gate: Arc<Semaphore>,
    started: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
enum GateMsg {
    Block,
    Fill,
}

impl Actor for Gated {
    type Message = GateMsg;
    type Response = ();

    async fn handle(&mut self, msg: GateMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let GateMsg::Block = msg {
            self.started.store(true, Ordering::SeqCst);
            if let Ok(permit) = self.gate.acquire().await {
                permit.forget();
            }
        }
        Ok(())
    }
}

/// An actor whose turn is parked and whose mailbox (or priority lane) holds
/// one message: a full lane of capacity one.
async fn full_gated(at: &str, priority: bool) -> (ActorHandle<Gated>, Arc<Semaphore>) {
    let gate = Arc::new(Semaphore::new(0));
    let started = Arc::new(AtomicBool::new(false));
    let mut config = ActorConfig::default().with_mailbox_capacity(1);
    if priority {
        config = config.with_priority_lane(1);
    }
    let handle = within(
        at,
        "a gated actor to start",
        Gated {
            gate: gate.clone(),
            started: started.clone(),
        }
        .spawn()
        .with_config(config),
    )
    .await
    .unwrap_or_else(|err| panic!("{at}: the gated actor did not start: {err}"));
    within(at, "the first message", handle.notify(GateMsg::Block))
        .await
        .unwrap_or_else(|err| panic!("{at}: the first message was refused: {err:?}"));
    wait_until(at, "the first turn to park", || {
        started.load(Ordering::SeqCst)
    })
    .await;
    let filled = if priority {
        within(
            at,
            "the filling message",
            handle.notify_priority(GateMsg::Fill),
        )
        .await
    } else {
        within(at, "the filling message", handle.notify(GateMsg::Fill)).await
    };
    filled.unwrap_or_else(|err| panic!("{at}: the filling message was refused: {err:?}"));
    (handle, gate)
}

fn send_parameter(priority: bool) -> &'static str {
    if priority {
        "send_priority_timeout"
    } else {
        "send_timeout"
    }
}

async fn timed_send(
    handle: ActorHandle<Gated>,
    priority: bool,
    d: Duration,
) -> Result<(), AskError> {
    if priority {
        handle.send_priority_timeout(GateMsg::Fill, d).await
    } else {
        handle.send_timeout(GateMsg::Fill, d).await
    }
}

#[test]
fn boundary_send_timeout() {
    let _serial = begin();
    common::run(0, async {
        for priority in [false, true] {
            let parameter = send_parameter(priority);
            for (label, d) in durations() {
                let at = cell(parameter, label);
                let (handle, gate) = full_gated(&at, priority).await;
                let call = timed_send(handle.clone(), priority, resolve(d));
                if is_small(d) {
                    // The lane is full for the whole window: the message is
                    // never enqueued.
                    let answer = within(&at, "a full lane to time out", call).await;
                    assert!(
                        matches!(answer, Err(AskError::Timeout { enqueued: false })),
                        "{at}: expected a timeout before the message was enqueued, got {answer:?}"
                    );
                } else {
                    // A deadline of 30 years or more never fires on a running
                    // clock: the send waits for room and completes once the
                    // gate opens.
                    let pending = tokio::spawn(call);
                    sleep(QUIET).await;
                    assert!(
                        !pending.is_finished(),
                        "{at}: the call returned while the lane was still full"
                    );
                    gate.add_permits(1_000);
                    let answer = within(&at, "the parked call to complete", pending)
                        .await
                        .expect("the call task is not cancelled");
                    assert!(
                        answer.is_ok(),
                        "{at}: expected the send to succeed once room opened, got {answer:?}"
                    );
                }
                gate.add_permits(1_000);
                stop_quietly(&at, &handle, StopReason::Graceful).await;
                assert_no_panic(&at);
            }

            // The edge of the range, with several margins: a call reads its
            // clock a little after the duration was measured, and each margin
            // covers a different delay between the two. The call waits on a
            // full lane and is dropped after a moment; nothing may panic.
            let at_edge = cell(parameter, "largest addable, margin sweep");
            let (handle, gate) = full_gated(&at_edge, priority).await;
            for step in 0..EDGE_STEPS {
                let call = timed_send(handle.clone(), priority, edge_at(step));
                assert!(
                    timeout(Duration::from_millis(5), call).await.is_err(),
                    "{at_edge}: margin {step}: the call returned while the lane was full"
                );
                assert_no_panic(&format!("{at_edge}: margin {step}"));
            }
            gate.add_permits(1_000);
            stop_quietly(&at_edge, &handle, StopReason::Graceful).await;
        }
    });
    run_paused(async {
        // On a paused clock a deadline of 30 years or more waits until the
        // clock passes the 30 year cap, then times out before the message was
        // enqueued.
        for priority in [false, true] {
            let parameter = send_parameter(priority);
            for (label, d) in large() {
                let at = cell(parameter, &format!("{label}, paused clock"));
                let (handle, gate) = full_gated(&at, priority).await;
                let call = tokio::spawn(timed_send(handle.clone(), priority, resolve(d)));
                pass_the_cap(&at, || call.is_finished()).await;
                drive(&at, "the call to return", || call.is_finished()).await;
                let answer = call.await.expect("the call task is not cancelled");
                assert!(
                    matches!(answer, Err(AskError::Timeout { enqueued: false })),
                    "{at}: expected a timeout at the cap, got {answer:?}"
                );
                gate.add_permits(1_000);
                stop_quietly(&at, &handle, StopReason::Graceful).await;
            }
        }
    });
    assert_no_panic("send timeouts, paused clock");
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/// Counts the ticks it receives and cancels its timers on the first one, so a
/// timer with a tiny interval does not flood the mailbox.
struct Ticker {
    ticks: Arc<AtomicUsize>,
}

#[derive(Clone)]
enum TickMsg {
    Tick,
    Every(Duration, MissPolicy),
    EveryWith(Duration, MissPolicy),
    /// Like `Every` and `EveryWith`, but each tick only counts: the timer stays
    /// registered.
    EveryKeep(Duration, MissPolicy),
    EveryWithKeep(Duration, MissPolicy),
    Count,
    After(Duration),
    /// Schedules at `now + offset`, or answers `Skipped` when the sum is not
    /// representable.
    At(Duration),
    /// Schedules at the farthest instant `Instant::checked_add` accepts from
    /// now, less 500 us; the clock is read once.
    AtEdge,
    /// Schedules at `now - offset`, or answers `Skipped` when that is not
    /// representable.
    AtPast(Duration),
    CancelAll,
    Timers,
}

enum TickResp {
    Registered {
        result: Result<RecurringId, TimerError>,
        before: usize,
        after: usize,
    },
    Skipped,
    Timers(usize),
    Done,
}

impl Actor for Ticker {
    type Message = TickMsg;
    type Response = TickResp;

    async fn handle(
        &mut self,
        msg: TickMsg,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<TickResp> {
        let before = ctx.active_timer_count();
        let result = match msg {
            TickMsg::Tick => {
                self.ticks.fetch_add(1, Ordering::SeqCst);
                ctx.cancel_all_timers();
                return Ok(TickResp::Done);
            }
            TickMsg::Count => {
                self.ticks.fetch_add(1, Ordering::SeqCst);
                return Ok(TickResp::Done);
            }
            TickMsg::CancelAll => {
                ctx.cancel_all_timers();
                return Ok(TickResp::Done);
            }
            TickMsg::Timers => return Ok(TickResp::Timers(before)),
            TickMsg::Every(interval, policy) => {
                ctx.schedule(TickMsg::Tick)
                    .every(resolve(interval))
                    .on_miss(policy)
                    .await
            }
            TickMsg::EveryWith(interval, policy) => {
                ctx.every_with(resolve(interval), || TickMsg::Tick)
                    .on_miss(policy)
                    .await
            }
            TickMsg::EveryKeep(interval, policy) => {
                ctx.schedule(TickMsg::Count)
                    .every(resolve(interval))
                    .on_miss(policy)
                    .await
            }
            TickMsg::EveryWithKeep(interval, policy) => {
                ctx.every_with(resolve(interval), || TickMsg::Count)
                    .on_miss(policy)
                    .await
            }
            TickMsg::After(delay) => ctx.schedule(TickMsg::Tick).after(resolve(delay)).await,
            TickMsg::AtEdge => {
                let base = Instant::now();
                let when = base + largest_addable_from(base);
                ctx.schedule(TickMsg::Tick).at(when).await
            }
            TickMsg::At(offset) => match Instant::now().checked_add(offset) {
                Some(when) => ctx.schedule(TickMsg::Tick).at(when).await,
                None => return Ok(TickResp::Skipped),
            },
            TickMsg::AtPast(offset) => match Instant::now().checked_sub(offset) {
                Some(when) => ctx.schedule(TickMsg::Tick).at(when).await,
                None => return Ok(TickResp::Skipped),
            },
        };
        Ok(TickResp::Registered {
            result,
            before,
            after: ctx.active_timer_count(),
        })
    }
}

async fn spawn_ticker(at: &str) -> (ActorHandle<Ticker>, Arc<AtomicUsize>) {
    let ticks = Arc::new(AtomicUsize::new(0));
    let handle = within(
        at,
        "a ticker to start",
        Ticker {
            ticks: ticks.clone(),
        }
        .spawn(),
    )
    .await
    .unwrap_or_else(|err| panic!("{at}: the ticker did not start: {err}"));
    (handle, ticks)
}

async fn register(at: &str, handle: &ActorHandle<Ticker>, msg: TickMsg) -> TickResp {
    ask(at, "the registration to answer", handle, msg).await
}

/// Cancels what is left and checks that nothing stays registered.
async fn finish_ticker(at: &str, handle: &ActorHandle<Ticker>) {
    ask(at, "the timers to be cancelled", handle, TickMsg::CancelAll).await;
    match ask(at, "the timer count", handle, TickMsg::Timers).await {
        TickResp::Timers(count) => assert_eq!(count, 0, "{at}: timers left after cancelling"),
        _ => panic!("{at}: the count was not answered"),
    }
    stop_quietly(at, handle, StopReason::Graceful).await;
}

#[test]
fn boundary_every() {
    let _serial = begin();
    common::run(0, async {
        for (builder, parameter) in [(false, "every"), (true, "every_with")] {
            for policy in [MissPolicy::Skip, MissPolicy::Delay, MissPolicy::CatchUp] {
                for (label, d) in durations() {
                    let at = cell(parameter, &format!("{label} ({policy:?})"));
                    let (handle, ticks) = spawn_ticker(&at).await;
                    let msg = if builder {
                        TickMsg::EveryWith(d, policy)
                    } else {
                        TickMsg::Every(d, policy)
                    };
                    let TickResp::Registered {
                        result,
                        before,
                        after,
                    } = register(&at, &handle, msg).await
                    else {
                        panic!("{at}: no registration answer");
                    };
                    if d.is_zero() {
                        assert!(
                            matches!(result, Err(TimerError::ZeroInterval)),
                            "{at}: a zero interval must answer ZeroInterval, got {result:?}"
                        );
                        assert_eq!(
                            before, after,
                            "{at}: a rejected interval registered a timer"
                        );
                        sleep(Duration::from_millis(100)).await;
                        assert_eq!(
                            ticks.load(Ordering::SeqCst),
                            0,
                            "{at}: a rejected interval ticked"
                        );
                    } else {
                        assert!(
                            result.is_ok(),
                            "{at}: expected a registration, got {result:?}"
                        );
                        assert_eq!(after, before + 1, "{at}: the timer was not registered");
                        if is_small(d) {
                            wait_until(&at, "the first tick", || ticks.load(Ordering::SeqCst) >= 1)
                                .await;
                        } else {
                            sleep(QUIET).await;
                            assert_eq!(
                                ticks.load(Ordering::SeqCst),
                                0,
                                "{at}: an interval of 30 years or more fired"
                            );
                        }
                    }
                    finish_ticker(&at, &handle).await;
                    assert_no_panic(&at);
                }
            }
        }
    });
    run_paused(async {
        // On a paused clock an interval of 30 years or more first ticks at the
        // 30 year cap.
        for (builder, parameter) in [(false, "every"), (true, "every_with")] {
            for (label, d) in large() {
                let at = cell(parameter, &format!("{label}, paused clock"));
                let (handle, ticks) = spawn_ticker(&at).await;
                let msg = if builder {
                    TickMsg::EveryWith(d, MissPolicy::Skip)
                } else {
                    TickMsg::Every(d, MissPolicy::Skip)
                };
                let TickResp::Registered { result, .. } = register(&at, &handle, msg).await else {
                    panic!("{at}: no registration answer");
                };
                assert!(
                    result.is_ok(),
                    "{at}: expected a registration, got {result:?}"
                );
                pass_the_cap(&at, || ticks.load(Ordering::SeqCst) > 0).await;
                drive(&at, "the first tick", || ticks.load(Ordering::SeqCst) > 0).await;
                finish_ticker(&at, &handle).await;
            }
        }

        // The cap holds for every deadline of a recurring timer, not only the
        // first: a timer that stays registered ticks once at each 30 years, for
        // every miss policy, and no later deadline overflows.
        for (builder, parameter) in [(false, "every"), (true, "every_with")] {
            for policy in [MissPolicy::Skip, MissPolicy::Delay, MissPolicy::CatchUp] {
                for (label, d) in large() {
                    let at = at_label(parameter, &format!("{label} ({policy:?}), second tick"));
                    let (handle, ticks) = spawn_ticker(&at).await;
                    let msg = if builder {
                        TickMsg::EveryWithKeep(d, policy)
                    } else {
                        TickMsg::EveryKeep(d, policy)
                    };
                    let TickResp::Registered { result, .. } = register(&at, &handle, msg).await
                    else {
                        panic!("{at}: no registration answer");
                    };
                    assert!(
                        result.is_ok(),
                        "{at}: expected a registration, got {result:?}"
                    );
                    settle().await;
                    advance(THIRTY_YEARS + CAP_SLACK).await;
                    settle().await;
                    drive(&at, "the first tick", || ticks.load(Ordering::SeqCst) >= 1).await;
                    assert_eq!(
                        ticks.load(Ordering::SeqCst),
                        1,
                        "{at}: one tick by the first cap"
                    );
                    advance(THIRTY_YEARS + CAP_SLACK).await;
                    settle().await;
                    drive(&at, "the second tick", || ticks.load(Ordering::SeqCst) >= 2).await;
                    assert_eq!(
                        ticks.load(Ordering::SeqCst),
                        2,
                        "{at}: two ticks by the second cap"
                    );
                    assert_no_panic(&at);
                    finish_ticker(&at, &handle).await;
                }
            }
        }
    });
    assert_no_panic("every, paused clock");
}

#[test]
fn boundary_schedule() {
    let _serial = begin();
    common::run(0, async {
        // `after`: a deadline built from the delay, capped at 30 years.
        for (label, d) in durations() {
            let at = cell("schedule.after", label);
            let (handle, ticks) = spawn_ticker(&at).await;
            let TickResp::Registered { result, .. } =
                register(&at, &handle, TickMsg::After(d)).await
            else {
                panic!("{at}: no registration answer");
            };
            assert!(
                result.is_ok(),
                "{at}: expected a registration, got {result:?}"
            );
            if is_small(d) {
                wait_until(&at, "the one-shot to fire", || {
                    ticks.load(Ordering::SeqCst) >= 1
                })
                .await;
            } else {
                sleep(QUIET).await;
                assert_eq!(
                    ticks.load(Ordering::SeqCst),
                    0,
                    "{at}: a delay of 30 years or more fired"
                );
            }
            finish_ticker(&at, &handle).await;
            assert_no_panic(&at);
        }

        // `at`: now, now plus each value that fits in an instant (the
        // farthest of them within the last millisecond before the end of the
        // representable range), and instants in the past. An instant 30
        // years or more away never fires; a past instant fires at once.
        let mut skipped = 0usize;
        for (label, d) in durations() {
            let at = cell("schedule.at", label);
            let (handle, ticks) = spawn_ticker(&at).await;
            let msg = if label == "largest addable" {
                TickMsg::AtEdge
            } else {
                TickMsg::At(d)
            };
            match register(&at, &handle, msg).await {
                TickResp::Skipped => {
                    skipped += 1;
                    println!("{at}: skipped, now plus the offset overflows");
                }
                TickResp::Registered { result, .. } => {
                    assert!(
                        result.is_ok(),
                        "{at}: expected a registration, got {result:?}"
                    );
                    if is_small(d) {
                        wait_until(&at, "the one-shot to fire", || {
                            ticks.load(Ordering::SeqCst) >= 1
                        })
                        .await;
                    } else {
                        sleep(QUIET).await;
                        let fired = ticks.load(Ordering::SeqCst);
                        assert_eq!(fired, 0, "{at}: an instant 30 years or more away fired");
                    }
                    finish_ticker(&at, &handle).await;
                }
                _ => panic!("{at}: unexpected answer"),
            }
            assert_no_panic(&at);
        }
        assert_eq!(
            skipped, 1,
            "parameter schedule.at: only Duration::MAX can never be added to an instant"
        );
        for (label, offset) in [
            ("1 ms in the past", Duration::from_millis(1)),
            ("1 hour in the past", Duration::from_secs(3_600)),
        ] {
            let at = cell("schedule.at", label);
            let (handle, ticks) = spawn_ticker(&at).await;
            match register(&at, &handle, TickMsg::AtPast(offset)).await {
                TickResp::Registered { result, .. } => {
                    assert!(
                        result.is_ok(),
                        "{at}: expected a registration, got {result:?}"
                    );
                    wait_until(&at, "the past instant to fire", || {
                        ticks.load(Ordering::SeqCst) >= 1
                    })
                    .await;
                    finish_ticker(&at, &handle).await;
                }
                _ => panic!("{at}: the clock is too young to subtract the offset"),
            }
            assert_no_panic(&at);
        }
    });
    run_paused(async {
        // On a paused clock a delay or an instant 30 years or more away fires
        // at the 30 year cap.
        for (label, d) in large() {
            let at = cell("schedule.after", &format!("{label}, paused clock"));
            let (handle, ticks) = spawn_ticker(&at).await;
            let TickResp::Registered { result, .. } =
                register(&at, &handle, TickMsg::After(d)).await
            else {
                panic!("{at}: no registration answer");
            };
            assert!(
                result.is_ok(),
                "{at}: expected a registration, got {result:?}"
            );
            pass_the_cap(&at, || ticks.load(Ordering::SeqCst) > 0).await;
            drive(&at, "the one-shot to fire", || {
                ticks.load(Ordering::SeqCst) > 0
            })
            .await;
            finish_ticker(&at, &handle).await;

            if d == Duration::MAX {
                continue;
            }
            let at = at_label("schedule.at", label);
            let (handle, ticks) = spawn_ticker(&at).await;
            let msg = if label == "largest addable" {
                TickMsg::AtEdge
            } else {
                TickMsg::At(d)
            };
            let TickResp::Registered { result, .. } = register(&at, &handle, msg).await else {
                panic!("{at}: no registration answer");
            };
            assert!(
                result.is_ok(),
                "{at}: expected a registration, got {result:?}"
            );
            pass_the_cap(&at, || ticks.load(Ordering::SeqCst) > 0).await;
            drive(&at, "the one-shot to fire", || {
                ticks.load(Ordering::SeqCst) > 0
            })
            .await;
            finish_ticker(&at, &handle).await;
        }
    });
    assert_no_panic("schedule, paused clock");
}

/// The prefix for a paused-clock cell.
fn at_label(parameter: &str, label: &str) -> String {
    cell(parameter, &format!("{label}, paused clock"))
}

// ---------------------------------------------------------------------------
// stop_within
// ---------------------------------------------------------------------------

#[test]
fn boundary_stop_within() {
    let _serial = begin();
    common::run(0, async {
        for (label, d) in durations() {
            // A cooperative actor, asked to stop gracefully: it is never
            // unresponsive, and it is stopped when the call returns.
            let at_graceful = cell("stop_within(Graceful)", label);
            let shared = Shared::new();
            let handle = spawn_worker(&at_graceful, &shared, false).await;
            let outcome = within(
                &at_graceful,
                "a cooperative stop_within(Graceful)",
                handle.stop_within(StopReason::Graceful, resolve(d)),
            )
            .await
            .unwrap_or_else(|err| panic!("{at_graceful}: the stop was refused: {err:?}"));
            assert!(
                matches!(
                    outcome,
                    StopOutcome::Graceful | StopOutcome::Killed | StopOutcome::Aborted
                ),
                "{at_graceful}: unexpected outcome {outcome:?}"
            );
            if !is_small(d) {
                assert_eq!(outcome, StopOutcome::Graceful, "{at_graceful}");
            }
            within(&at_graceful, "the actor to stop", handle.wait_stopped()).await;
            assert_no_panic(&at_graceful);

            // An actor that vetoes the graceful stop: a small window escalates
            // to Kill; a window of 30 years or more waits until something ends
            // the actor.
            let at_veto = cell("stop_within(Graceful) on a vetoing actor", label);
            let shared = Shared::new();
            let handle = spawn_worker(&at_veto, &shared, true).await;
            if is_small(d) {
                let outcome = within(
                    &at_veto,
                    "a vetoed stop_within(Graceful) to escalate",
                    handle.stop_within(StopReason::Graceful, resolve(d)),
                )
                .await
                .unwrap_or_else(|err| panic!("{at_veto}: the stop was refused: {err:?}"));
                assert!(
                    matches!(outcome, StopOutcome::Killed | StopOutcome::Aborted),
                    "{at_veto}: expected an escalation, got {outcome:?}"
                );
            } else {
                let pending = {
                    let handle = handle.clone();
                    tokio::spawn(async move {
                        handle.stop_within(StopReason::Graceful, resolve(d)).await
                    })
                };
                sleep(QUIET).await;
                assert!(
                    !pending.is_finished(),
                    "{at_veto}: the call returned inside its window"
                );
                stop_quietly(&at_veto, &handle, StopReason::Kill).await;
                let answer = within(&at_veto, "the call to return after the kill", pending)
                    .await
                    .expect("the call task is not cancelled");
                assert!(answer.is_ok(), "{at_veto}: {answer:?}");
            }
            within(&at_veto, "the vetoing actor to stop", handle.wait_stopped()).await;
            assert_no_panic(&at_veto);

            // Kill on a parked turn: `within` replaces the kill grace.
            let at_kill = cell("stop_within(Kill)", label);
            let shared = Shared::new();
            let handle = spawn_worker(&at_kill, &shared, false).await;
            park(&at_kill, &handle, &shared).await;
            if is_small(d) {
                let outcome = within(
                    &at_kill,
                    "stop_within(Kill) to abort a parked turn",
                    handle.stop_within(StopReason::Kill, resolve(d)),
                )
                .await
                .unwrap_or_else(|err| panic!("{at_kill}: the kill was refused: {err:?}"));
                assert!(
                    matches!(outcome, StopOutcome::Killed | StopOutcome::Aborted),
                    "{at_kill}: expected the parked turn to be aborted, got {outcome:?}"
                );
            } else {
                let pending = {
                    let handle = handle.clone();
                    tokio::spawn(
                        async move { handle.stop_within(StopReason::Kill, resolve(d)).await },
                    )
                };
                sleep(QUIET).await;
                assert!(
                    !pending.is_finished(),
                    "{at_kill}: the parked turn was cut inside a window of 30 years or more"
                );
                shared.open_gates();
                let answer = within(&at_kill, "the kill to land at the boundary", pending)
                    .await
                    .expect("the call task is not cancelled");
                assert_eq!(
                    answer.unwrap_or_else(|err| panic!("{at_kill}: the kill was refused: {err:?}")),
                    StopOutcome::Killed,
                    "{at_kill}"
                );
            }
            within(&at_kill, "the parked actor to stop", handle.wait_stopped()).await;
            assert_no_panic(&at_kill);
        }

        // The edge of the range, with several margins (see the sweep over
        // `send_timeout`).
        for step in 0..EDGE_STEPS {
            let at_graceful = cell(
                "stop_within(Graceful)",
                &format!("largest addable, margin {step}"),
            );
            let shared = Shared::new();
            let handle = spawn_worker(&at_graceful, &shared, false).await;
            let outcome = within(
                &at_graceful,
                "stop_within(Graceful) at the edge",
                handle.stop_within(StopReason::Graceful, edge_at(step)),
            )
            .await
            .unwrap_or_else(|err| panic!("{at_graceful}: the stop was refused: {err:?}"));
            assert_eq!(outcome, StopOutcome::Graceful, "{at_graceful}");
            within(&at_graceful, "the actor to stop", handle.wait_stopped()).await;

            let at_kill = cell(
                "stop_within(Kill)",
                &format!("largest addable, margin {step}"),
            );
            let handle = spawn_worker(&at_kill, &shared, false).await;
            let outcome = within(
                &at_kill,
                "stop_within(Kill) at the edge",
                handle.stop_within(StopReason::Kill, edge_at(step)),
            )
            .await
            .unwrap_or_else(|err| panic!("{at_kill}: the kill was refused: {err:?}"));
            assert_eq!(outcome, StopOutcome::Killed, "{at_kill}");
            assert_no_panic(&format!("stop_within, largest addable, margin {step}"));
        }
    });
    run_paused(async {
        // On a paused clock a window of 30 years or more ends at the 30 year
        // cap: a vetoed graceful stop escalates, a Kill on a parked turn
        // aborts it.
        for (label, d) in large() {
            let at_veto = at_label("stop_within(Graceful) on a vetoing actor", label);
            let shared = Shared::new();
            let handle = spawn_worker(&at_veto, &shared, true).await;
            let call = tokio::spawn({
                let handle = handle.clone();
                async move { handle.stop_within(StopReason::Graceful, resolve(d)).await }
            });
            pass_the_cap(&at_veto, || call.is_finished()).await;
            drive(&at_veto, "the call to return", || call.is_finished()).await;
            let outcome = call
                .await
                .expect("the call task is not cancelled")
                .unwrap_or_else(|err| panic!("{at_veto}: the stop was refused: {err:?}"));
            assert!(
                matches!(outcome, StopOutcome::Killed | StopOutcome::Aborted),
                "{at_veto}: expected an escalation at the cap, got {outcome:?}"
            );

            let at_kill = at_label("stop_within(Kill)", label);
            let shared = Shared::new();
            let handle = spawn_worker(&at_kill, &shared, false).await;
            park(&at_kill, &handle, &shared).await;
            let call = tokio::spawn({
                let handle = handle.clone();
                async move { handle.stop_within(StopReason::Kill, resolve(d)).await }
            });
            pass_the_cap(&at_kill, || call.is_finished()).await;
            drive(&at_kill, "the call to return", || call.is_finished()).await;
            let outcome = call
                .await
                .expect("the call task is not cancelled")
                .unwrap_or_else(|err| panic!("{at_kill}: the kill was refused: {err:?}"));
            assert!(
                matches!(outcome, StopOutcome::Killed | StopOutcome::Aborted),
                "{at_kill}: expected the parked turn to end at the cap, got {outcome:?}"
            );
        }
    });
    assert_no_panic("stop_within, paused clock");
}

// ---------------------------------------------------------------------------
// Kill grace
// ---------------------------------------------------------------------------

/// The rows of the kill grace table that run on a real-time runtime and, with
/// `paused`, on a paused clock, where an abort lands on its exact instant.
async fn boundary_kill_grace_rows(paused: bool) {
    for (label, d) in durations() {
        // A root, killed by name and by `stop(Kill)`.
        for entry in ["ActorSystem::kill", "stop(Kill)"] {
            let parameter = format!("SpawnBuilder::kill_grace via {entry}");
            let at = cell(&parameter, label);
            let sys = ActorSystem::create(uname("grace-root")).unwrap();
            let shared = Shared::new();
            let name = uname("root");
            let handle = within(
                &at,
                "the root to start",
                Worker::new(&shared, false)
                    .spawn()
                    .named(&name)
                    .on_system(&sys)
                    .kill_grace(resolve(d)),
            )
            .await
            .unwrap_or_else(|err| panic!("{at}: the root did not start: {err}"));
            park(&at, &handle, &shared).await;
            if entry == "stop(Kill)" {
                within(&at, "the kill request", handle.stop(StopReason::Kill))
                    .await
                    .unwrap_or_else(|err| panic!("{at}: the kill was refused: {err:?}"));
            } else {
                within(&at, "the kill request", sys.kill(&name))
                    .await
                    .unwrap_or_else(|err| panic!("{at}: the kill was refused: {err:?}"));
            }
            if is_small(d) {
                within_for(
                    upper(paused, d + ASSUMED_ABORT_SETTLE + MARGIN),
                    &at,
                    "a parked target to be aborted within its grace",
                    handle.wait_stopped(),
                )
                .await;
            } else {
                sleep(QUIET).await;
                assert_ne!(
                    handle.status(),
                    ActorStatus::Stopped,
                    "{at}: aborted inside a grace of 30 years or more"
                );
                shared.open_gates();
                within(
                    &at,
                    "the target to end at its boundary",
                    handle.wait_stopped(),
                )
                .await;
            }
            assert_no_panic(&at);
        }

        // A zero or tiny grace aborts the parked turn at once and the
        // verdict is an abort, never unresponsive.
        if is_small(d) {
            let at = cell("SpawnBuilder::kill_grace verdict", label);
            let shared = Shared::new();
            let handle = within(
                &at,
                "the actor to start",
                Worker::new(&shared, false).spawn().kill_grace(resolve(d)),
            )
            .await
            .unwrap_or_else(|err| panic!("{at}: the actor did not start: {err}"));
            park(&at, &handle, &shared).await;
            let outcome = within(
                &at,
                "stop_within to escalate a parked turn",
                handle.stop_within(StopReason::Graceful, Duration::from_millis(1)),
            )
            .await
            .unwrap_or_else(|err| panic!("{at}: the stop was refused: {err:?}"));
            assert!(
                matches!(outcome, StopOutcome::Killed | StopOutcome::Aborted),
                "{at}: a parked turn is ended by the abort, not reported unresponsive, \
                 got {outcome:?}"
            );
            assert_no_panic(&at);
        }

        // A supervised child whose supervisor is killed: the child's link
        // guard raises Kill and arms the child's own grace.
        let at = cell("ChildSpawnBuilder::kill_grace", label);
        let sys = ActorSystem::create(uname("grace-child")).unwrap();
        let shared = Shared::new();
        let sup = spawn_sup(&at, &sys, &shared, None).await;
        let name = uname("child");
        ask_spawn(
            &at,
            &sup,
            worker_cmd(
                &name,
                d,
                Shutdown::Timeout(Duration::from_secs(5)),
                RestartType::Temporary,
            ),
        )
        .await;
        let child = registered(&at, &sys, &name).await;
        park(&at, &child, &shared).await;
        within(&at, "the kill request", sup.stop(StopReason::Kill))
            .await
            .unwrap_or_else(|err| panic!("{at}: the kill was refused: {err:?}"));
        if is_small(d) {
            within_for(
                upper(paused, d + ASSUMED_ABORT_SETTLE + MARGIN),
                &at,
                "a parked child to be aborted within its grace",
                child.wait_stopped(),
            )
            .await;
        } else {
            sleep(QUIET).await;
            assert_ne!(
                child.status(),
                ActorStatus::Stopped,
                "{at}: aborted inside a grace of 30 years or more"
            );
            shared.open_gates();
            within(
                &at,
                "the child to end at its boundary",
                child.wait_stopped(),
            )
            .await;
        }
        assert_no_panic(&at);
    }

    // The edge of the range, with several margins (see the sweep over
    // `send_timeout`): the grace is read when the kill is raised, a
    // moment after it was configured.
    for step in 0..EDGE_STEPS {
        let at = cell(
            "SpawnBuilder::kill_grace via ActorSystem::kill",
            &format!("largest addable, margin {step}"),
        );
        let sys = ActorSystem::create(uname("grace-edge-root")).unwrap();
        let shared = Shared::new();
        let name = uname("root");
        let handle = within(
            &at,
            "the root to start",
            Worker::new(&shared, false)
                .spawn()
                .named(&name)
                .on_system(&sys)
                .kill_grace(edge_at(step)),
        )
        .await
        .unwrap_or_else(|err| panic!("{at}: the root did not start: {err}"));
        park(&at, &handle, &shared).await;
        within(&at, "the kill request", sys.kill(&name))
            .await
            .unwrap_or_else(|err| panic!("{at}: the kill was refused: {err:?}"));
        shared.open_gates();
        within(
            &at,
            "the root to end at its boundary",
            handle.wait_stopped(),
        )
        .await;
        sleep(Duration::from_millis(20)).await;
        assert_no_panic(&at);
    }
}

#[test]
fn boundary_kill_grace() {
    let _serial = begin();
    common::run(0, boundary_kill_grace_rows(false));
    run_paused(async {
        // On a paused clock a grace of 30 years or more aborts a parked turn
        // at the 30 year cap.
        for (label, d) in large() {
            let at = at_label("SpawnBuilder::kill_grace", label);
            let shared = Shared::new();
            let handle = within(
                &at,
                "the actor to start",
                Worker::new(&shared, false).spawn().kill_grace(resolve(d)),
            )
            .await
            .unwrap_or_else(|err| panic!("{at}: the actor did not start: {err}"));
            park(&at, &handle, &shared).await;
            within(&at, "the kill request", handle.stop(StopReason::Kill))
                .await
                .unwrap_or_else(|err| panic!("{at}: the kill was refused: {err:?}"));
            let stopped = || handle.status() == ActorStatus::Stopped;
            pass_the_cap(&at, stopped).await;
            drive(&at, "the parked turn to be aborted", stopped).await;

            // A parked child of a supervisor that is killed: its link guard
            // arms the child's own grace.
            let at = at_label("ChildSpawnBuilder::kill_grace", label);
            let sys = ActorSystem::create(uname("grace-child-paused")).unwrap();
            let shared = Shared::new();
            let sup = spawn_sup(&at, &sys, &shared, None).await;
            let name = uname("child");
            ask_spawn(
                &at,
                &sup,
                worker_cmd(
                    &name,
                    d,
                    Shutdown::Timeout(Duration::from_secs(5)),
                    RestartType::Temporary,
                ),
            )
            .await;
            let child = registered(&at, &sys, &name).await;
            park(&at, &child, &shared).await;
            within(&at, "the kill request", sup.stop(StopReason::Kill))
                .await
                .unwrap_or_else(|err| panic!("{at}: the kill was refused: {err:?}"));
            let stopped = || child.status() == ActorStatus::Stopped;
            pass_the_cap(&at, stopped).await;
            drive(&at, "the parked child to be aborted", stopped).await;
        }
    });
    assert_no_panic("kill_grace, paused clock");
}

#[test]
fn boundary_kill_grace_paused() {
    let _serial = begin();
    run_paused(boundary_kill_grace_rows(true));
    assert_no_panic("kill_grace rows, paused clock");
}

// ---------------------------------------------------------------------------
// Start timeouts
// ---------------------------------------------------------------------------

#[test]
fn boundary_start_timeout() {
    let _serial = begin();
    common::run(0, async {
        // SpawnBuilder::start_timeout: the first initialization hangs.
        for (label, d) in durations() {
            let at = cell("SpawnBuilder::start_timeout", label);
            let shared = Shared::new();
            shared.hang_init_from.store(1, Ordering::SeqCst);
            let spawn = {
                let shared = shared.clone();
                async move {
                    Worker::new(&shared, false)
                        .spawn()
                        .start_timeout(resolve(d))
                        .await
                }
            };
            if is_small(d) {
                let answer = within(&at, "the start timeout to elapse", spawn).await;
                assert!(
                    matches!(answer, Err(SpawnError::StartTimeout)),
                    "{at}: expected StartTimeout, got {:?}",
                    answer.map(|_| ())
                );
            } else {
                let pending = tokio::spawn(spawn);
                sleep(QUIET).await;
                assert!(
                    !pending.is_finished(),
                    "{at}: gave up on an initialization inside a window of 30 years or more"
                );
                shared.open_gates();
                let answer = within(&at, "the spawn to complete", pending)
                    .await
                    .expect("the spawn task is not cancelled");
                let handle = answer.unwrap_or_else(|err| {
                    panic!("{at}: the actor did not start once its initialization ended: {err}")
                });
                ask(&at, "a ping", &handle, Msg::Ping).await;
                stop_quietly(&at, &handle, StopReason::Graceful).await;
            }
            shared.open_gates();
            assert_no_panic(&at);
        }

        // ChildSpawnBuilder::start_timeout: bounds the wait for a restart's
        // initialization.
        for (label, d) in durations() {
            let at = cell("ChildSpawnBuilder::start_timeout", label);
            let sys = ActorSystem::create(uname("start-timeout")).unwrap();
            let shared = Shared::new();
            shared.hang_init_from.store(2, Ordering::SeqCst);
            let sup = spawn_sup(
                &at,
                &sys,
                &shared,
                Some(SupervisionConfig::default().max_restarts(10, Duration::from_secs(60))),
            )
            .await;
            let name = uname("child");
            ask_spawn(
                &at,
                &sup,
                Cmd::Worker {
                    name: name.clone(),
                    grace: Duration::from_millis(50),
                    shutdown: Shutdown::Timeout(Duration::from_secs(5)),
                    restart: RestartType::Permanent,
                    veto: false,
                    start_timeout: Some(d),
                },
            )
            .await;
            let child = registered(&at, &sys, &name).await;
            within(&at, "the crash request", child.notify(Msg::Crash))
                .await
                .unwrap_or_else(|err| panic!("{at}: the crash request was refused: {err:?}"));
            wait_until(&at, "the restart to build a new incarnation", || {
                shared.built.load(Ordering::SeqCst) >= 2
            })
            .await;
            if is_small(d) {
                // The hung initialization is given up and another attempt is
                // made.
                wait_until(&at, "a second restart attempt", || {
                    shared.built.load(Ordering::SeqCst) >= 3
                })
                .await;
            } else {
                sleep(QUIET).await;
                assert_eq!(
                    shared.built.load(Ordering::SeqCst),
                    2,
                    "{at}: the supervisor gave up on a restart inside a window of 30 years or more"
                );
                shared.open_gates();
                wait_until(&at, "the restarted child to answer", || {
                    sys.get::<Worker>(&name)
                        .is_some_and(|child| child.status() == ActorStatus::Running)
                })
                .await;
            }
            shared.open_gates();
            stop_quietly(&at, &sup, StopReason::Kill).await;
            within(&at, "the supervisor to stop", sup.wait_stopped()).await;
            assert_no_panic(&at);
        }
    });
    run_paused(async {
        // On a paused clock a start timeout of 30 years or more gives up on a
        // hung initialization at the 30 year cap.
        for (label, d) in large() {
            let at = at_label("SpawnBuilder::start_timeout", label);
            let shared = Shared::new();
            shared.hang_init_from.store(1, Ordering::SeqCst);
            let spawn = tokio::spawn({
                let shared = shared.clone();
                async move {
                    Worker::new(&shared, false)
                        .spawn()
                        .start_timeout(resolve(d))
                        .await
                }
            });
            pass_the_cap(&at, || spawn.is_finished()).await;
            drive(&at, "the spawn to return", || spawn.is_finished()).await;
            let answer = spawn.await.expect("the spawn task is not cancelled");
            assert!(
                matches!(answer, Err(SpawnError::StartTimeout)),
                "{at}: expected StartTimeout at the cap, got {:?}",
                answer.map(|_| ())
            );
            shared.open_gates();

            let at = at_label("ChildSpawnBuilder::start_timeout", label);
            let sys = ActorSystem::create(uname("start-timeout-paused")).unwrap();
            let shared = Shared::new();
            shared.hang_init_from.store(2, Ordering::SeqCst);
            let sup = spawn_sup(
                &at,
                &sys,
                &shared,
                Some(SupervisionConfig::default().max_restarts(10, Duration::from_secs(60))),
            )
            .await;
            let name = uname("child");
            ask_spawn(
                &at,
                &sup,
                Cmd::Worker {
                    name: name.clone(),
                    grace: Duration::from_millis(50),
                    shutdown: Shutdown::Timeout(Duration::from_secs(5)),
                    restart: RestartType::Permanent,
                    veto: false,
                    start_timeout: Some(d),
                },
            )
            .await;
            let child = registered(&at, &sys, &name).await;
            within(&at, "the crash request", child.notify(Msg::Crash))
                .await
                .unwrap_or_else(|err| panic!("{at}: the crash request was refused: {err:?}"));
            drive(&at, "the restart to build a new incarnation", || {
                shared.built.load(Ordering::SeqCst) >= 2
            })
            .await;
            pass_the_cap(&at, || shared.built.load(Ordering::SeqCst) >= 3).await;
            drive(&at, "a second restart attempt at the cap", || {
                shared.built.load(Ordering::SeqCst) >= 3
            })
            .await;
            shared.open_gates();
            stop_quietly(&at, &sup, StopReason::Kill).await;
        }
    });
    assert_no_panic("start timeouts, paused clock");
}

// ---------------------------------------------------------------------------
// Shutdown timeouts
// ---------------------------------------------------------------------------

/// A system whose shutdown policy is `timeout` and `per_actor_timeout`.
fn policy_system(timeout: Duration, per_actor_timeout: Duration) -> Arc<ActorSystem> {
    ActorSystem::create_with(
        uname("policy"),
        SystemConfig {
            shutdown_policy: ShutdownPolicy {
                timeout,
                per_actor_timeout,
            },
            ..SystemConfig::default()
        },
    )
    .unwrap()
}

#[test]
fn boundary_shutdown_timeouts() {
    let _serial = begin();
    common::run(0, async {
        for (label, d) in durations() {
            // Shutdown::Timeout on a worker child that vetoes the graceful
            // stop of its supervisor.
            let at = cell("Shutdown::Timeout on a worker", label);
            let sys = ActorSystem::create(uname("shutdown-worker")).unwrap();
            let shared = Shared::new();
            let sup = spawn_sup(&at, &sys, &shared, None).await;
            ask_spawn(
                &at,
                &sup,
                Cmd::Worker {
                    name: uname("veto"),
                    grace: Duration::from_millis(50),
                    shutdown: Shutdown::Timeout(d),
                    restart: RestartType::Permanent,
                    veto: true,
                    start_timeout: None,
                },
            )
            .await;
            within(&at, "the stop request", sup.stop(StopReason::Graceful))
                .await
                .unwrap_or_else(|err| panic!("{at}: the stop was refused: {err:?}"));
            if is_small(d) {
                within(
                    &at,
                    "the supervisor to escalate past a vetoing child",
                    sup.wait_stopped(),
                )
                .await;
            } else {
                sleep(QUIET).await;
                assert_ne!(
                    sup.status(),
                    ActorStatus::Stopped,
                    "{at}: escalated inside a window of 30 years or more"
                );
                stop_quietly(&at, &sup, StopReason::Kill).await;
                within(&at, "the killed supervisor to stop", sup.wait_stopped()).await;
            }
            assert_no_panic(&at);

            // Shutdown::Timeout on a supervisor child, under a parent that is
            // killed: the child is raised its orderly `ParentKilled` stop and
            // stops its own vetoing worker in order, bounded by its own
            // window. Under a window of 30 years or more both are still
            // running after a quiet period (the child `Stopping`, the worker
            // `Running`), and a `Kill` of the child then ends both.
            let at = cell("Shutdown::Timeout on a supervisor child", label);
            let sys = ActorSystem::create(uname("shutdown-supervisor")).unwrap();
            let shared = Shared::new();
            let top = spawn_sup(&at, &sys, &shared, None).await;
            let mid_name = uname("mid");
            ask_spawn(
                &at,
                &top,
                Cmd::Sup {
                    name: mid_name.clone(),
                    grace: Duration::from_millis(50),
                    shutdown: Shutdown::Timeout(d),
                    restart: RestartType::Permanent,
                },
            )
            .await;
            let mid = sys.get::<Sup>(&mid_name).expect("the supervisor child");
            let leaf_name = uname("leaf");
            ask_spawn(
                &at,
                &mid,
                Cmd::Worker {
                    name: leaf_name.clone(),
                    grace: Duration::from_millis(50),
                    shutdown: Shutdown::Timeout(d),
                    restart: RestartType::Permanent,
                    veto: true,
                    start_timeout: None,
                },
            )
            .await;
            let leaf = registered(&at, &sys, &leaf_name).await;
            within(&at, "the kill request", top.stop(StopReason::Kill))
                .await
                .unwrap_or_else(|err| panic!("{at}: the kill was refused: {err:?}"));
            within(&at, "the killed parent to stop", top.wait_stopped()).await;
            if !is_small(d) {
                sleep(QUIET).await;
                assert_eq!(
                    mid.status(),
                    ActorStatus::Stopping,
                    "{at}: the supervisor child stops in order inside its window"
                );
                assert_eq!(
                    leaf.status(),
                    ActorStatus::Running,
                    "{at}: the vetoing worker is still running"
                );
                assert!(
                    shared.asked.load(Ordering::SeqCst) >= 1,
                    "{at}: the supervisor child asked its worker to stop in order"
                );
                within(&at, "the kill of the supervisor child", sys.kill(&mid_name))
                    .await
                    .unwrap_or_else(|err| panic!("{at}: the kill was refused: {err:?}"));
            }
            within(&at, "the supervisor child to stop", mid.wait_stopped()).await;
            within(&at, "the leaf to stop", leaf.wait_stopped()).await;
            assert_no_panic(&at);
        }

        // Shutdown::Timeout together with kill_grace, through the restart of a
        // killed supervisor whose worker ends at its first turn boundary: the
        // row asserts that the successor starts and nothing panics while the
        // two durations are summed into the subtree's budget. One row per pair
        // of values; running clock only.
        for (shutdown_label, shutdown_d) in large().into_iter().filter(|(_, d)| *d >= THIRTY_YEARS)
        {
            for (grace_label, grace_d) in large().into_iter().filter(|(_, d)| *d >= THIRTY_YEARS) {
                let at = cell(
                    "Shutdown::Timeout with kill_grace",
                    &format!("{shutdown_label} and {grace_label}"),
                );
                let sys = ActorSystem::create(uname("pair")).unwrap();
                let shared = Shared::new();
                let top = spawn_sup(&at, &sys, &shared, None).await;
                let mid_name = uname("mid");
                ask_spawn(
                    &at,
                    &top,
                    Cmd::Sup {
                        name: mid_name.clone(),
                        grace: grace_d,
                        shutdown: Shutdown::Timeout(shutdown_d),
                        restart: RestartType::Permanent,
                    },
                )
                .await;
                let mid = sys.get::<Sup>(&mid_name).expect("the supervisor child");
                ask_spawn(
                    &at,
                    &mid,
                    worker_cmd(
                        &uname("leaf"),
                        grace_d,
                        Shutdown::Timeout(shutdown_d),
                        RestartType::Permanent,
                    ),
                )
                .await;
                wait_until(&at, "both supervisors to be started", || {
                    shared.sups.load(Ordering::SeqCst) == 2
                })
                .await;
                within(&at, "the kill request", sys.kill(&mid_name))
                    .await
                    .unwrap_or_else(|err| panic!("{at}: the kill was refused: {err:?}"));
                wait_until(&at, "the killed supervisor to be restarted", || {
                    shared.sups.load(Ordering::SeqCst) >= 3
                })
                .await;
                stop_quietly(&at, &top, StopReason::Kill).await;
                within(&at, "the top supervisor to stop", top.wait_stopped()).await;
                assert_no_panic(&at);
            }
        }

        // ShutdownPolicy::timeout: a root that vetoes the graceful stop, with
        // a per-actor bound that never ends the wait, so the global timeout
        // decides.
        for (label, d) in durations() {
            let at = cell("ShutdownPolicy::timeout", label);
            let sys = policy_system(resolve(d), Duration::from_secs(30));
            let shared = Shared::new();
            let root = within(
                &at,
                "the root to start",
                Worker::new(&shared, true)
                    .spawn()
                    .named(uname("root"))
                    .on_system(&sys),
            )
            .await
            .unwrap_or_else(|err| panic!("{at}: the root did not start: {err}"));
            if is_small(d) {
                let report = within(&at, "the shutdown to return", sys.shutdown()).await;
                assert!(
                    matches!(
                        report.outcomes.as_slice(),
                        [(
                            _,
                            StopReport {
                                outcome: StopOutcome::Killed | StopOutcome::Aborted,
                                reason: Some(StopReason::Kill),
                            },
                        )]
                    ),
                    "{at}: expected the vetoing root to be killed at the global deadline, got {report:?}"
                );
            } else {
                let pending = {
                    let sys = sys.clone();
                    tokio::spawn(async move { sys.shutdown().await })
                };
                sleep(QUIET).await;
                assert!(
                    !pending.is_finished(),
                    "{at}: the shutdown gave up inside a timeout of 30 years or more"
                );
                stop_quietly(&at, &root, StopReason::Kill).await;
                let report = within(&at, "the shutdown to return after the kill", pending)
                    .await
                    .expect("the shutdown task is not cancelled");
                assert_eq!(report.outcomes.len(), 1, "{at}: {report:?}");
            }
            within(&at, "the root to stop", root.wait_stopped()).await;
            assert_no_panic(&at);
        }

        // ShutdownPolicy::per_actor_timeout: the same root, with a global
        // timeout that never ends the wait.
        for (label, d) in durations() {
            let at = cell("ShutdownPolicy::per_actor_timeout", label);
            let sys = policy_system(Duration::from_secs(30), resolve(d));
            let shared = Shared::new();
            let root = within(
                &at,
                "the root to start",
                Worker::new(&shared, true)
                    .spawn()
                    .named(uname("root"))
                    .on_system(&sys),
            )
            .await
            .unwrap_or_else(|err| panic!("{at}: the root did not start: {err}"));
            if is_small(d) {
                let report = within(&at, "the shutdown to return", sys.shutdown()).await;
                assert!(
                    matches!(
                        report.outcomes.as_slice(),
                        [(
                            _,
                            StopReport {
                                outcome: StopOutcome::Killed | StopOutcome::Aborted,
                                reason: Some(StopReason::Kill),
                            },
                        )]
                    ),
                    "{at}: expected the vetoing root to be killed, got {report:?}"
                );
            } else {
                let pending = {
                    let sys = sys.clone();
                    tokio::spawn(async move { sys.shutdown().await })
                };
                sleep(QUIET).await;
                assert!(
                    !pending.is_finished(),
                    "{at}: the shutdown escalated inside a window of 30 years or more"
                );
                stop_quietly(&at, &root, StopReason::Kill).await;
                let report = within(&at, "the shutdown to return after the kill", pending)
                    .await
                    .expect("the shutdown task is not cancelled");
                assert_eq!(report.outcomes.len(), 1, "{at}: {report:?}");
            }
            within(&at, "the root to stop", root.wait_stopped()).await;
            assert_no_panic(&at);
        }
    });
    run_paused(async {
        // On a paused clock each of these ends at the 30 year cap.
        for (label, d) in large() {
            // Shutdown::Timeout on a vetoing worker child.
            let at = at_label("Shutdown::Timeout on a worker", label);
            let sys = ActorSystem::create(uname("shutdown-worker-paused")).unwrap();
            let shared = Shared::new();
            let sup = spawn_sup(&at, &sys, &shared, None).await;
            ask_spawn(
                &at,
                &sup,
                Cmd::Worker {
                    name: uname("veto"),
                    grace: Duration::from_millis(50),
                    shutdown: Shutdown::Timeout(d),
                    restart: RestartType::Permanent,
                    veto: true,
                    start_timeout: None,
                },
            )
            .await;
            within(&at, "the stop request", sup.stop(StopReason::Graceful))
                .await
                .unwrap_or_else(|err| panic!("{at}: the stop was refused: {err:?}"));
            let stopped = || sup.status() == ActorStatus::Stopped;
            pass_the_cap(&at, stopped).await;
            drive(
                &at,
                "the supervisor to escalate past the vetoing child",
                stopped,
            )
            .await;

            // Shutdown::Timeout on a supervisor child: the parent's wait for the
            // child, which waits forever for its vetoing grandchild, ends at the
            // cap.
            let at = at_label("Shutdown::Timeout on a supervisor child", label);
            let sys = ActorSystem::create(uname("shutdown-supervisor-paused")).unwrap();
            let shared = Shared::new();
            let top = spawn_sup(&at, &sys, &shared, None).await;
            let mid_name = uname("mid");
            ask_spawn(
                &at,
                &top,
                Cmd::Sup {
                    name: mid_name.clone(),
                    grace: Duration::from_millis(50),
                    shutdown: Shutdown::Timeout(d),
                    restart: RestartType::Permanent,
                },
            )
            .await;
            let mid = sys.get::<Sup>(&mid_name).expect("the supervisor child");
            ask_spawn(
                &at,
                &mid,
                Cmd::Worker {
                    name: uname("leaf"),
                    grace: Duration::from_millis(50),
                    shutdown: Shutdown::Infinity,
                    restart: RestartType::Permanent,
                    veto: true,
                    start_timeout: None,
                },
            )
            .await;
            within(&at, "the stop request", top.stop(StopReason::Graceful))
                .await
                .unwrap_or_else(|err| panic!("{at}: the stop was refused: {err:?}"));
            let stopped = || top.status() == ActorStatus::Stopped;
            pass_the_cap(&at, stopped).await;
            drive(&at, "the top supervisor to stop at the cap", stopped).await;

            // Shutdown::Timeout on a supervisor child under a parent that is
            // killed: the child stops in order inside its own window, which
            // ends at the cap with the reaper's `Kill`, and its vetoing worker
            // then ends with it.
            let at = at_label(
                "Shutdown::Timeout on a supervisor child, killed parent",
                label,
            );
            let sys = ActorSystem::create(uname("shutdown-killed-parent-paused")).unwrap();
            let shared = Shared::new();
            let top = spawn_sup(&at, &sys, &shared, None).await;
            let mid_name = uname("mid");
            ask_spawn(
                &at,
                &top,
                Cmd::Sup {
                    name: mid_name.clone(),
                    grace: Duration::from_millis(50),
                    shutdown: Shutdown::Timeout(d),
                    restart: RestartType::Permanent,
                },
            )
            .await;
            let mid = sys.get::<Sup>(&mid_name).expect("the supervisor child");
            let leaf_name = uname("leaf");
            ask_spawn(
                &at,
                &mid,
                Cmd::Worker {
                    name: leaf_name.clone(),
                    grace: Duration::from_millis(50),
                    shutdown: Shutdown::Timeout(d),
                    restart: RestartType::Permanent,
                    veto: true,
                    start_timeout: None,
                },
            )
            .await;
            let leaf = registered(&at, &sys, &leaf_name).await;
            within(&at, "the kill request", top.stop(StopReason::Kill))
                .await
                .unwrap_or_else(|err| panic!("{at}: the kill was refused: {err:?}"));
            drive(&at, "the killed parent to stop", || {
                top.status() == ActorStatus::Stopped
            })
            .await;
            let mid_stopped = || mid.status() == ActorStatus::Stopped;
            pass_the_cap(&at, mid_stopped).await;
            drive(&at, "the supervisor child to stop at the cap", mid_stopped).await;
            drive(&at, "the leaf to stop", || {
                leaf.status() == ActorStatus::Stopped
            })
            .await;

            // ShutdownPolicy::timeout: two vetoing roots and a per-actor bound
            // that is never the one to end the wait. The first root's wait ends
            // at the global deadline, the second is swept at once, so the whole
            // shutdown ends at the cap; a global deadline past the cap would let
            // the second root wait for its own 30 years.
            let at = at_label("ShutdownPolicy::timeout", label);
            let sys = policy_system(resolve(d), Duration::MAX);
            let shared = Shared::new();
            let mut roots = Vec::new();
            for _ in 0..2 {
                roots.push(
                    within(
                        &at,
                        "a root to start",
                        Worker::new(&shared, true)
                            .spawn()
                            .named(uname("root"))
                            .on_system(&sys),
                    )
                    .await
                    .unwrap_or_else(|err| panic!("{at}: a root did not start: {err}")),
                );
            }
            let shutdown = tokio::spawn({
                let sys = sys.clone();
                async move { sys.shutdown().await }
            });
            pass_the_cap(&at, || shutdown.is_finished()).await;
            drive(&at, "the shutdown to return at the cap", || {
                shutdown.is_finished()
            })
            .await;
            let report = shutdown.await.expect("the shutdown task is not cancelled");
            assert!(
                matches!(
                    report.outcomes.as_slice(),
                    [
                        (
                            _,
                            StopReport {
                                outcome: StopOutcome::Killed | StopOutcome::Aborted,
                                reason: Some(StopReason::Kill),
                            },
                        ),
                        (
                            _,
                            StopReport {
                                outcome: StopOutcome::Killed | StopOutcome::Aborted,
                                reason: Some(StopReason::Kill),
                            },
                        )
                    ]
                ),
                "{at}: expected both vetoing roots to be killed at the cap, got {report:?}"
            );

            // ShutdownPolicy::per_actor_timeout: the wait for one root. Its cap is
            // the same wait bound the global deadline uses, so this row cannot say
            // which of the two ended the wait; the running-clock rows do.
            let at = at_label("ShutdownPolicy::per_actor_timeout", label);
            let sys = policy_system(Duration::MAX, resolve(d));
            let shared = Shared::new();
            let root = within(
                &at,
                "the root to start",
                Worker::new(&shared, true)
                    .spawn()
                    .named(uname("root"))
                    .on_system(&sys),
            )
            .await
            .unwrap_or_else(|err| panic!("{at}: the root did not start: {err}"));
            let shutdown = tokio::spawn({
                let sys = sys.clone();
                async move { sys.shutdown().await }
            });
            pass_the_cap(&at, || shutdown.is_finished()).await;
            drive(&at, "the shutdown to return", || shutdown.is_finished()).await;
            let report = shutdown.await.expect("the shutdown task is not cancelled");
            assert!(
                matches!(
                    report.outcomes.as_slice(),
                    [(
                        _,
                        StopReport {
                            outcome: StopOutcome::Killed | StopOutcome::Aborted,
                            reason: Some(StopReason::Kill),
                        },
                    )]
                ),
                "{at}: expected the vetoing root to be killed at the cap, got {report:?}"
            );
            drive(&at, "the root to stop", || {
                root.status() == ActorStatus::Stopped
            })
            .await;
        }
    });
    assert_no_panic("shutdown timeouts, paused clock");
}

// ---------------------------------------------------------------------------
// The turn warning threshold
// ---------------------------------------------------------------------------

#[test]
fn boundary_turn_warning_threshold() {
    let _serial = begin();
    common::run(0, async {
        for (label, d) in durations() {
            let at = cell("turn warning threshold", label);
            let d = resolve(d);
            // What the threshold reads back: zero is off, and a value beyond
            // what a u64 of nanoseconds holds saturates to it.
            let expected = if d.is_zero() {
                None
            } else if d.as_nanos() > u128::from(u64::MAX) {
                Some(Duration::from_nanos(u64::MAX))
            } else {
                Some(d)
            };

            let configured = ActorSystem::create_with(
                uname("threshold-config"),
                SystemConfig::default().with_turn_warning_threshold(d),
            )
            .unwrap();
            assert_eq!(
                configured.turn_warning_threshold(),
                expected,
                "{at}: SystemConfig::with_turn_warning_threshold"
            );

            let direct = ActorSystem::create_with(
                uname("threshold-field"),
                SystemConfig {
                    turn_warning_threshold: Some(d),
                    ..SystemConfig::default()
                },
            )
            .unwrap();
            assert_eq!(
                direct.turn_warning_threshold(),
                expected,
                "{at}: SystemConfig::turn_warning_threshold"
            );

            let sys = ActorSystem::create(uname("threshold-set")).unwrap();
            sys.set_turn_warning_threshold(Some(d));
            assert_eq!(
                sys.turn_warning_threshold(),
                expected,
                "{at}: set_turn_warning_threshold"
            );

            // With the threshold in effect an actor handles messages and the
            // observers answer without panicking.
            let shared = Shared::new();
            let name = uname("timed");
            let handle = within(
                &at,
                "the actor to start",
                Worker::new(&shared, false)
                    .spawn()
                    .named(&name)
                    .on_system(&sys),
            )
            .await
            .unwrap_or_else(|err| panic!("{at}: the actor did not start: {err}"));
            for _ in 0..3 {
                ask(&at, "a ping", &handle, Msg::Ping).await;
            }
            let _ = sys.turn_elapsed(handle.id());
            let stuck = sys.stuck_actors();
            if d >= THIRTY_YEARS {
                assert!(
                    stuck.is_empty(),
                    "{at}: an idle actor was reported stuck: {stuck:?}"
                );
            }

            sys.set_turn_warning_threshold(None);
            assert_eq!(sys.turn_warning_threshold(), None, "{at}: None reads off");
            assert!(
                sys.stuck_actors().is_empty(),
                "{at}: stuck actors with the watchdog off"
            );
            assert_eq!(
                sys.turn_elapsed(handle.id()),
                None,
                "{at}: a turn is timed with the watchdog off"
            );
            stop_quietly(&at, &handle, StopReason::Graceful).await;
            assert_no_panic(&at);
        }
    });
    assert_no_panic("turn warning threshold, after the runtime stopped");
}

// ---------------------------------------------------------------------------
// The restart window
// ---------------------------------------------------------------------------

/// What one crash led to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Next {
    Restarted,
    SupervisorStopped,
}

/// Crashes the live incarnation of `name` and waits for either a restart
/// (another worker built) or the end of the supervisor. On a paused clock the
/// waits only yield, so the clock does not move.
async fn crash_once(
    at: &str,
    paused: bool,
    sys: &Arc<ActorSystem>,
    sup: &ActorHandle<Sup>,
    shared: &Arc<Shared>,
    name: &str,
) -> Next {
    async fn pause(paused: bool) {
        if paused {
            tokio::task::yield_now().await;
        } else {
            sleep(Duration::from_millis(2)).await;
        }
    }
    let built = shared.built.load(Ordering::SeqCst);
    let child = within(at, "a live child to crash", async {
        loop {
            if let Some(child) = sys.get::<Worker>(name) {
                if child.send(Msg::Ping).await.is_ok() {
                    return child;
                }
            }
            pause(paused).await;
        }
    })
    .await;
    let _ = within(at, "the crash request", child.notify(Msg::Crash)).await;
    within(at, "a restart or the end of the supervisor", async {
        loop {
            if shared.built.load(Ordering::SeqCst) > built {
                return Next::Restarted;
            }
            if sup.status() == ActorStatus::Stopped {
                return Next::SupervisorStopped;
            }
            pause(paused).await;
        }
    })
    .await
}

/// A supervisor with one child of the given restart type and the given restart
/// budget.
async fn budgeted_sup(
    at: &str,
    shared: &Arc<Shared>,
    count: u32,
    window: Duration,
    restart: RestartType,
) -> (Arc<ActorSystem>, ActorHandle<Sup>, String) {
    let sys = ActorSystem::create(uname("window")).unwrap();
    let sup = spawn_sup(
        at,
        &sys,
        shared,
        Some(SupervisionConfig::default().max_restarts(count, window)),
    )
    .await;
    let name = uname("child");
    ask_spawn(
        at,
        &sup,
        worker_cmd(
            &name,
            Duration::from_millis(50),
            Shutdown::Timeout(Duration::from_secs(5)),
            restart,
        ),
    )
    .await;
    (sys, sup, name)
}

#[test]
fn boundary_restart_window() {
    let _serial = begin();
    common::run(0, async {
        for (count_label, count) in [("0", 0u32), ("1", 1), ("3", 3), ("u32::MAX", u32::MAX)] {
            for (label, window) in durations() {
                let parameter = format!("max_restarts {count_label} with a restart window");
                let at = cell(&parameter, label);
                let window = resolve(window);
                let shared = Shared::new();
                let (sys, sup, name) =
                    budgeted_sup(&at, &shared, count, window, RestartType::Permanent).await;
                let never_prunes = window >= THIRTY_YEARS - Duration::from_nanos(1);

                // `crashes` is the number of failures tried; the budget is
                // exhausted by the first one past `count`.
                let crashes = if count == u32::MAX { 4 } else { count + 1 };
                let mut seen = Vec::new();
                for _ in 0..crashes {
                    let next = crash_once(&at, false, &sys, &sup, &shared, &name).await;
                    let stopped = next == Next::SupervisorStopped;
                    seen.push(next);
                    if stopped {
                        break;
                    }
                }
                if count == 0 {
                    assert_eq!(
                        seen,
                        [Next::SupervisorStopped],
                        "{at}: a budget of 0 refuses the first restart whatever the window"
                    );
                } else if count == u32::MAX {
                    assert!(
                        seen.iter().all(|next| *next == Next::Restarted),
                        "{at}: a budget of u32::MAX never exhausts, saw {seen:?}"
                    );
                } else if never_prunes {
                    let mut expected = vec![Next::Restarted; count as usize];
                    expected.push(Next::SupervisorStopped);
                    assert_eq!(
                        seen, expected,
                        "{at}: a window of 30 years or more never prunes, so the failure \
                         past the budget ends the supervisor"
                    );
                } else {
                    println!("{at}: saw {seen:?}");
                }
                stop_quietly(&at, &sup, StopReason::Kill).await;
                within(&at, "the supervisor to stop", sup.wait_stopped()).await;
                assert_no_panic(&at);
            }
        }
        // A failure that makes no restart does not charge the budget: a
        // `Temporary` child under a budget of 0 leaves the supervisor running.
        for (label, window) in [("0", Duration::ZERO), ("30 years", THIRTY_YEARS)] {
            let at = cell("max_restarts 0 with a Temporary child", label);
            let shared = Shared::new();
            let (sys, sup, name) =
                budgeted_sup(&at, &shared, 0, window, RestartType::Temporary).await;
            let child = registered(&at, &sys, &name).await;
            within(&at, "the crash request", child.notify(Msg::Crash))
                .await
                .unwrap_or_else(|err| panic!("{at}: the crash request was refused: {err:?}"));
            within(&at, "the child to end", child.wait_stopped()).await;
            sleep(QUIET).await;
            assert_ne!(
                sup.status(),
                ActorStatus::Stopped,
                "{at}: a failure that makes no restart charged the budget"
            );
            assert_eq!(
                shared.built.load(Ordering::SeqCst),
                1,
                "{at}: a Temporary child was restarted"
            );
            stop_quietly(&at, &sup, StopReason::Kill).await;
            assert_no_panic(&at);
        }
    });
    run_paused(async {
        // A zero window counts the restarts that read the same instant, and a
        // paused clock that does not move gives every restart the same instant:
        // the budget is exhausted by the failure after `max_restarts` restarts.
        for count in [1u32, 3] {
            let at = cell(
                &format!("max_restarts {count} with a restart window"),
                "0, paused clock",
            );
            let shared = Shared::new();
            let (sys, sup, name) =
                budgeted_sup(&at, &shared, count, Duration::ZERO, RestartType::Permanent).await;
            let start = Instant::now();
            let mut seen = Vec::new();
            for _ in 0..=count {
                let next = crash_once(&at, true, &sys, &sup, &shared, &name).await;
                let stopped = next == Next::SupervisorStopped;
                seen.push(next);
                if stopped {
                    break;
                }
            }
            let mut expected = vec![Next::Restarted; count as usize];
            expected.push(Next::SupervisorStopped);
            assert_eq!(
                seen, expected,
                "{at}: restarts at one instant exhaust a zero window after max_restarts"
            );
            assert_eq!(Instant::now(), start, "{at}: the paused clock moved");
        }
        // A paused clock that moves to the next pending timer whenever the
        // runtime is idle behaves like a running one: a child that fails by a
        // timer after each start reads a new instant every time, so a zero
        // window never exhausts the budget.
        {
            let at = cell(
                "max_restarts 1 with a restart window",
                "0, paused clock that auto-advances",
            );
            let shared = Shared::new();
            shared.crash_after_start.store(true, Ordering::SeqCst);
            let (_sys, sup, _name) =
                budgeted_sup(&at, &shared, 1, Duration::ZERO, RestartType::Permanent).await;
            sleep(Duration::from_millis(200)).await;
            assert!(
                shared.built.load(Ordering::SeqCst) >= 10,
                "{at}: the child kept failing and restarting, built {}",
                shared.built.load(Ordering::SeqCst)
            );
            assert_ne!(
                sup.status(),
                ActorStatus::Stopped,
                "{at}: the budget was exhausted although every restart read a new instant"
            );
            shared.crash_after_start.store(false, Ordering::SeqCst);
            stop_quietly(&at, &sup, StopReason::Kill).await;
        }
        // A budget of 0 refuses the first restart whatever the window.
        for (label, window) in [("0", Duration::ZERO), ("30 years", THIRTY_YEARS)] {
            let at = cell(
                "max_restarts 0 with a restart window",
                &format!("{label}, paused clock"),
            );
            let shared = Shared::new();
            let (sys, sup, name) =
                budgeted_sup(&at, &shared, 0, window, RestartType::Permanent).await;
            let next = crash_once(&at, true, &sys, &sup, &shared, &name).await;
            assert_eq!(
                next,
                Next::SupervisorStopped,
                "{at}: a budget of 0 refuses the first restart"
            );
        }
    });
    assert_no_panic("restart window, paused clock");
}

// ---------------------------------------------------------------------------
// Capacities
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn boundary_mailbox_capacity() {
    let _serial = begin();
    common::run(0, async {
        let max = tokio::sync::Semaphore::MAX_PERMITS;
        let capacities = [
            ("0", 0usize),
            ("1", 1),
            ("Semaphore::MAX_PERMITS", max),
            ("Semaphore::MAX_PERMITS + 1", max + 1),
            ("usize::MAX", usize::MAX),
        ];
        for (label, capacity) in capacities {
            let clamped = capacity.min(max);

            // The mailbox.
            let at_mailbox = cell("mailbox capacity", label);
            let result = within(
                &at_mailbox,
                "the spawn to answer",
                Idle.spawn()
                    .with_config(ActorConfig::default().with_mailbox_capacity(capacity)),
            )
            .await;
            match (capacity, result) {
                (0, Err(SpawnError::ZeroMailboxCapacity)) => {}
                (0, other) => panic!(
                    "{at_mailbox}: expected ZeroMailboxCapacity, got {:?}",
                    other.map(|_| ())
                ),
                (_, Ok(handle)) => {
                    assert_eq!(handle.mailbox_capacity(), clamped, "{at_mailbox}");
                    ask(&at_mailbox, "a request", &handle, ()).await;
                    stop_quietly(&at_mailbox, &handle, StopReason::Graceful).await;
                }
                (_, Err(err)) => panic!("{at_mailbox}: spawn failed: {err}"),
            }

            // The priority lane, beside an ordinary mailbox.
            let at_lane = cell("priority lane capacity", label);
            let result = within(
                &at_lane,
                "the spawn to answer",
                Idle.spawn().with_config(
                    ActorConfig::default()
                        .with_mailbox_capacity(4)
                        .with_priority_lane(capacity),
                ),
            )
            .await;
            match (capacity, result) {
                (0, Err(SpawnError::ZeroMailboxCapacity)) => {}
                (0, other) => panic!(
                    "{at_lane}: expected ZeroMailboxCapacity, got {:?}",
                    other.map(|_| ())
                ),
                (_, Ok(handle)) => {
                    assert_eq!(handle.priority_capacity(), clamped, "{at_lane}");
                    match within(&at_lane, "a priority request", handle.send_priority(())).await {
                        Ok(()) => {}
                        Err(err) => panic!("{at_lane}: the priority request failed: {err:?}"),
                    }
                    stop_quietly(&at_lane, &handle, StopReason::Graceful).await;
                }
                (_, Err(err)) => panic!("{at_lane}: spawn failed: {err}"),
            }

            // A supervised child takes the same configuration, for the
            // mailbox and for the priority lane.
            let sys = ActorSystem::create(uname("capacity-child")).unwrap();
            let shared = Shared::new();
            let sup = spawn_sup(&at_mailbox, &sys, &shared, None).await;
            for (kind, config) in [
                (
                    "mailbox",
                    ActorConfig::default().with_mailbox_capacity(capacity),
                ),
                (
                    "priority lane",
                    ActorConfig::default()
                        .with_mailbox_capacity(4)
                        .with_priority_lane(capacity),
                ),
            ] {
                let at = cell(&format!("child {kind} capacity"), label);
                let name = uname("sized");
                let answer = ask(
                    &at,
                    "the supervisor to answer",
                    &sup,
                    Cmd::Sized {
                        name: name.clone(),
                        config,
                    },
                )
                .await;
                if capacity == 0 {
                    assert!(
                        answer.is_err(),
                        "{at}: a zero capacity must be refused, got {answer:?}"
                    );
                } else {
                    assert!(answer.is_ok(), "{at}: {answer:?}");
                    let child = registered(&at, &sys, &name).await;
                    let seen = if kind == "mailbox" {
                        child.mailbox_capacity()
                    } else {
                        child.priority_capacity()
                    };
                    assert_eq!(seen, clamped, "{at}");
                }
            }
            stop_quietly(&at_mailbox, &sup, StopReason::Graceful).await;
            assert_no_panic(&at_mailbox);
        }
    });
    assert_no_panic("capacities, after the runtime stopped");
}
