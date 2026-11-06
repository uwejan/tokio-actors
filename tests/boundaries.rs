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
    ActorError, ActorHandle, ActorResult, ActorSystem, MissPolicy, RecurringId, StopReason,
    SystemConfig, TimerError,
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

fn resolve(d: Duration) -> Duration {
    if d == EDGE {
        edge_now()
    } else {
        d
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

// ---------------------------------------------------------------------------
// Send timeouts
// ---------------------------------------------------------------------------

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
    let _begun = begin();
    run(0, async {
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

// ---------------------------------------------------------------------------
// Start timeouts
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Shutdown timeouts
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The turn warning threshold
// ---------------------------------------------------------------------------

#[test]
fn boundary_turn_warning_threshold() {
    let _begun = begin();
    run(0, async {
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

// ---------------------------------------------------------------------------
// Capacities
// ---------------------------------------------------------------------------
