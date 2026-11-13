#![allow(dead_code)]
//! The boundary table: every duration and capacity parameter of the public
//! API, crossed with the values that sit on the edges of what it accepts.
//!
//! Durations are crossed with {0, 1 ns, 1 ms, 50 ms, 30 years minus 1 ns,
//! 30 years, the largest duration `Instant::checked_add` accepts from now
//! (less 500 us, so it stays inside the last millisecond before the edge of
//! the representable range), `Duration::MAX`}. Counts are crossed with their
//! own edge values. Each `#[test]` loops over the whole value set of one
//! parameter group; every failure message names the parameter and the value.
//! The tests run in parallel, and each runs its runtimes on threads named
//! after the test, so a panic raised in a spawned task is reported by the
//! test that caused it.
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
    MissPolicy, RecurringId, RestartType, Shutdown, ShutdownPolicy, StopOutcome, StopReason,
    SupervisionConfig, SystemConfig, TimerError,
};

// ---------------------------------------------------------------------------
// Values, bounds and helpers
// ---------------------------------------------------------------------------

const THIRTY_YEARS: Duration = Duration::from_secs(86_400 * 365 * 30);

/// The bound on every outcome a test awaits that no configured duration
/// derives.
const BOUND: Duration = Duration::from_secs(10);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// How long a cell that must not fire is observed on a running clock.
const QUIET: Duration = Duration::from_millis(250);

/// How far past its exact instant an outcome may land on the paused clock:
/// one poll step of a waiting loop and the timers' rounding, far short of any
/// grace a test configures.
const PAUSED_SLACK: Duration = Duration::from_millis(30);

/// The upper bound of a timed outcome: its `exact` instant plus
/// [`PAUSED_SLACK`] on the paused clock, where an outcome lands on its
/// instant, and a hang bound past it on a real-time runtime, loose enough for
/// a host that stalls its threads.
fn upper(paused: bool, exact: Duration) -> Duration {
    if paused {
        exact + PAUSED_SLACK
    } else {
        exact + BOUND
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
    with_record(|record| record.row = row.clone());
    row
}

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

/// Polls `pred` until it holds. The bound only limits how long a real hang takes to report.
async fn wait_until(at: &str, what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_BOUND;
    loop {
        if pred() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{at}: timed out after {HANG_BOUND:?} waiting for: {what}"
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

/// What one test has seen: the row it most recently started, and the panics
/// raised on its threads anywhere but in this file's own assertions (a panic
/// inside a spawned task is caught by the runtime, so only a hook sees every
/// one).
#[derive(Default)]
struct Record {
    row: String,
    panics: Vec<String>,
}

/// The records of the tests running now, keyed by the name of the test's
/// thread. Every runtime a test builds names its threads the same, so a panic
/// on any of them reaches the record of the test that raised it.
static RECORDS: Mutex<Vec<(String, Record)>> = Mutex::new(Vec::new());

fn owner() -> String {
    std::thread::current()
        .name()
        .unwrap_or_default()
        .to_string()
}

fn records() -> MutexGuard<'static, Vec<(String, Record)>> {
    RECORDS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs `f` on the record of the calling test, if it has one.
fn with_record<R>(f: impl FnOnce(&mut Record) -> R) -> Option<R> {
    let name = owner();
    records()
        .iter_mut()
        .find(|(key, _)| *key == name)
        .map(|(_, record)| f(record))
}

fn drain_panics() -> Vec<String> {
    with_record(|record| std::mem::take(&mut record.panics)).unwrap_or_default()
}

/// A test's record, removed when the test ends. The last check runs there: a
/// panic recorded after the test's final check fails the test.
struct Begun;

impl Drop for Begun {
    fn drop(&mut self) {
        let name = owner();
        let record = {
            let mut all = records();
            let at = all.iter().position(|(key, _)| *key == name);
            at.map(|at| all.remove(at).1)
        };
        if !std::thread::panicking() {
            if let Some(record) = record {
                assert!(
                    record.panics.is_empty(),
                    "a panic was recorded after the last check: {:?}",
                    record.panics
                );
            }
        }
    }
}

/// Starts the calling test's record and installs the panic hook once.
fn begin() -> Begun {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let own = info
                .location()
                .is_some_and(|place| place.file().ends_with("boundaries.rs"));
            if !own {
                let name = owner();
                let mut all = records();
                // A thread no test owns cannot name its row: every running
                // test hears of it.
                let known = all.iter().any(|(key, _)| *key == name);
                for (key, record) in all.iter_mut() {
                    if *key == name || !known {
                        eprintln!(
                            "[boundaries] a panic was raised after starting: {}",
                            record.row
                        );
                        record
                            .panics
                            .push(format!("{info} (after starting: {})", record.row));
                    }
                }
            }
            previous(info);
        }));
    });
    let mut all = records();
    let name = owner();
    all.retain(|(key, _)| *key != name);
    all.push((name, Record::default()));
    Begun
}

/// Fails if a panic escaped since the previous check.
fn assert_no_panic(at: &str) {
    let seen = drain_panics();
    assert!(seen.is_empty(), "{at}: a panic escaped: {seen:?}");
}

// ---- Paused clock ------------------------------------------------------------

/// Runs `body` on a Tokio runtime whose flavor is chosen by the
/// `TOKIO_ACTORS_TEST_FLAVOR` environment variable (`current_thread` selects a
/// single-threaded runtime, anything else a multi-threaded one), with its
/// threads named after the calling test.
fn run<F: Future<Output = ()>>(workers: usize, body: F) {
    let runtime = if std::env::var("TOKIO_ACTORS_TEST_FLAVOR").as_deref() == Ok("current_thread") {
        tokio::runtime::Builder::new_current_thread()
            .thread_name(owner())
            .enable_all()
            .build()
            .expect("failed to build current-thread test runtime")
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        if workers > 0 {
            builder.worker_threads(workers);
        }
        builder
            .thread_name(owner())
            .enable_all()
            .build()
            .expect("failed to build multi-thread test runtime")
    };
    runtime.block_on(body);
}

/// Runs `body` on a current-thread runtime whose clock is paused.
fn run_paused<F: Future<Output = ()>>(body: F) {
    tokio::runtime::Builder::new_current_thread()
        .thread_name(owner())
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
fn boundary_schedule() {
    let _begun = begin();
    run(0, async {
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
                    upper(paused, d),
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
                upper(paused, d),
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

// ---------------------------------------------------------------------------
// Start timeouts
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// The turn warning threshold
// ---------------------------------------------------------------------------

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
