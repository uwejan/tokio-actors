#![allow(dead_code)]
//! The kill grace: the time an actor is given, after a `Kill` is raised on it,
//! to reach its next turn boundary before its task is aborted. It is a policy
//! of the actor (`kill_grace` on `SpawnBuilder` and `ChildSpawnBuilder`),
//! kept across a supervised child's restarts and read by every kill path.
//!
//! Every target here is parked at `std::future::pending()` or at a gate inside
//! `handle` when its `Kill` is raised, so only the abort can end it. Timing is
//! asserted by ordering and by margins derived from the configured grace: a
//! target's stop is observed no sooner than its whole grace after an instant
//! read before its `Kill` was raised, and no later than the abort its grace
//! schedules. Every awaited outcome is wrapped in
//! `tokio::time::timeout`.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorStatus, ActorSystem, RestartType, Shutdown,
    ShutdownPolicy, StopOutcome, StopReason, SupervisionConfig, SupervisionError, SystemConfig,
};

mod common;

/// The time between a `Kill` and the abort when a spawn builder sets no
/// `kill_grace`. An integration test cannot read the crate's constant, so the
/// value is named here once.
const ASSUMED_KILL_GRACE: Duration = Duration::from_millis(120);

/// The wait after an abort before the Unresponsive verdict, named here once.
const ASSUMED_ABORT_SETTLE: Duration = Duration::from_millis(100);

/// How far past its exact instant an outcome may land on the paused clock:
/// one poll step of a waiting loop and the timers' rounding, far short of any
/// grace a test configures.
const PAUSED_SLACK: Duration = Duration::from_millis(30);

/// The bound on every outcome a test awaits that is not derived from a
/// configured grace.
const BOUND: Duration = Duration::from_secs(10);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// The longest a `Block` turn holds its thread when the test never releases
/// it: past every bound a test awaits.
const GATE_CAP: Duration = Duration::from_secs(15);

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

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("kill-grace-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

/// Awaits `fut` for at most `bound`; a reverted build fails here instead of
/// hanging.
async fn within<T>(bound: Duration, what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match timeout(bound, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {bound:?} waiting for: {what}"),
    }
}

/// Polls `pred` until it holds. The bound only limits how long a real hang takes to report. A shorter `bound` from a caller is
/// raised to `HANG_BOUND`.
async fn wait_until(bound: Duration, what: &str, mut pred: impl FnMut() -> bool) {
    let bound = bound.max(HANG_BOUND);
    let deadline = Instant::now() + bound;
    loop {
        if pred() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {bound:?} waiting for: {what}"
        );
        sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// The targets
// ---------------------------------------------------------------------------

/// What the tests observe of a target, across its incarnations.
#[derive(Default)]
struct Probe {
    /// Instances built.
    built: AtomicUsize,
    /// Set when an instance entered its parked turn.
    parked: AtomicBool,
    /// The gate a parked turn waits at. Never opened by most tests; the
    /// boundary tests open it to let the turn end.
    gate: Notify,
    /// While set, an instance past the first hangs in `on_started` at
    /// `init_gate`.
    hang_restart_init: AtomicBool,
    init_gate: Notify,
    /// While set, an instance's `pre_stop` reports `pre_stop_entered` and then
    /// never returns.
    hold_pre_stop: AtomicBool,
    /// Set when an instance entered a held `pre_stop`.
    pre_stop_entered: AtomicBool,
    /// Messages answered.
    pings: AtomicUsize,
    /// While set, a `Block` turn holds its thread (for at most `GATE_CAP`).
    block_held: AtomicBool,
}

/// Clears a probe's `block_held` when dropped, so a held `Block` turn ends
/// however the test ends.
struct ReleasesTheBlock(Arc<Probe>);

impl Drop for ReleasesTheBlock {
    fn drop(&mut self) {
        self.0.block_held.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug)]
enum Msg {
    /// Answers at once.
    Ping,
    /// Parks at the gate (`park_forever`: never opened).
    Park,
    /// Fails, which stops the instance.
    Crash,
    /// Blocks the thread inside `handle` while the probe's `block_held` is
    /// set: a turn that never reaches a boundary and cannot be aborted until
    /// it ends.
    Block,
    /// A turn that yields at the probe's gate until the test opens it.
    Nap,
    /// Queued behind a turn and never handled: dropping it panics.
    Bomb(#[allow(dead_code)] PanicOnDrop),
}

/// Panics when dropped, after recording that its drop ran. Dropping the
/// mailbox of a stopping actor drops it after the actor's message loop and
/// hooks are over.
#[derive(Debug)]
struct PanicOnDrop(Arc<AtomicBool>);

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
        panic!("a panic in the teardown's drop glue");
    }
}

struct Worker {
    probe: Arc<Probe>,
    /// Parks at `std::future::pending()` instead of at the probe's gate.
    park_forever: bool,
}

impl Actor for Worker {
    type Message = Msg;
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let built = self.probe.built.load(Ordering::SeqCst);
        if built > 1 && self.probe.hang_restart_init.load(Ordering::SeqCst) {
            self.probe.init_gate.notified().await;
        }
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        if self.probe.hold_pre_stop.load(Ordering::SeqCst) {
            self.probe.pre_stop_entered.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
        true
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Ping => {
                self.probe.pings.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            Msg::Park => {
                self.probe.parked.store(true, Ordering::SeqCst);
                if self.park_forever {
                    std::future::pending::<()>().await;
                } else {
                    self.probe.gate.notified().await;
                }
                Ok(())
            }
            Msg::Crash => Err(ActorError::user("crash on command")),
            Msg::Nap => {
                self.probe.parked.store(true, Ordering::SeqCst);
                self.probe.gate.notified().await;
                Ok(())
            }
            Msg::Bomb(_) => Ok(()),
            Msg::Block => {
                self.probe.parked.store(true, Ordering::SeqCst);
                let cap = std::time::Instant::now() + GATE_CAP;
                while self.probe.block_held.load(Ordering::SeqCst)
                    && std::time::Instant::now() < cap
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            }
        }
    }
}

fn worker(probe: &Arc<Probe>, park_forever: bool) -> Worker {
    probe.built.fetch_add(1, Ordering::SeqCst);
    Worker {
        probe: probe.clone(),
        park_forever,
    }
}

/// A supervisor that spawns and stops children on command.
struct Sup {
    probe: Arc<Probe>,
    park_forever: bool,
}

#[derive(Clone)]
enum Cmd {
    Spawn {
        name: String,
        grace: Duration,
        shutdown: Shutdown,
        start_timeout: Option<Duration>,
    },
    Terminate(String),
}

impl Actor for Sup {
    type Message = Cmd;
    type Response = Result<(), String>;

    async fn handle(
        &mut self,
        cmd: Cmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match cmd {
            Cmd::Spawn {
                name,
                grace,
                shutdown,
                start_timeout,
            } => {
                let probe = self.probe.clone();
                let park_forever = self.park_forever;
                let mut builder = ctx
                    .spawn_child(move || worker(&probe, park_forever))
                    .named(name)
                    .restart_type(RestartType::Permanent)
                    .shutdown(shutdown)
                    .kill_grace(grace);
                if let Some(bound) = start_timeout {
                    builder = builder.start_timeout(bound);
                }
                Ok(builder.await.map(|_| ()).map_err(|err| err.to_string()))
            }
            Cmd::Terminate(name) => Ok(ctx
                .terminate_child(name)
                .await
                .map_err(|err: SupervisionError| err.to_string())),
        }
    }
}

async fn spawn_root(
    sys: &Arc<ActorSystem>,
    name: &str,
    probe: &Arc<Probe>,
    grace: Option<Duration>,
) -> ActorHandle<Worker> {
    let mut builder = worker(probe, true).spawn().named(name).on_system(sys);
    if let Some(grace) = grace {
        builder = builder.kill_grace(grace);
    }
    builder.await.unwrap()
}

async fn spawn_sup(
    sys: &Arc<ActorSystem>,
    probe: &Arc<Probe>,
    park_forever: bool,
) -> ActorHandle<Sup> {
    Sup {
        probe: probe.clone(),
        park_forever,
    }
    .spawn()
    .named(uname("sup"))
    .on_system(sys)
    .with_supervision(SupervisionConfig::default().max_restarts(20, Duration::from_secs(60)))
    .await
    .unwrap()
}

async fn spawn_child_via(sup: &ActorHandle<Sup>, name: &str, grace: Duration, shutdown: Shutdown) {
    let answer = sup
        .send(Cmd::Spawn {
            name: name.to_string(),
            grace,
            shutdown,
            start_timeout: None,
        })
        .await
        .unwrap();
    answer.expect("the child spawns");
}

async fn registered(sys: &Arc<ActorSystem>, name: &str) -> ActorHandle<Worker> {
    let mut found = None;
    wait_until(BOUND, "the actor to be registered", || {
        found = sys.get::<Worker>(name);
        found.is_some()
    })
    .await;
    found.unwrap()
}

async fn park(handle: &ActorHandle<Worker>, probe: &Arc<Probe>) {
    probe.parked.store(false, Ordering::SeqCst);
    handle.notify(Msg::Park).await.unwrap();
    wait_until(BOUND, "the turn to park", || {
        probe.parked.load(Ordering::SeqCst)
    })
    .await;
}

/// The target is `Stopped` no earlier than `grace` after `killed_at`, and
/// no later than the abort that its grace schedules.
/// `killed_at` is read before the call that raises the `Kill`, so the abort's
/// grace starts at or after it: a stop observed sooner than `grace` after it
/// is an abort that did not wait out the target's grace, however late the
/// test itself is scheduled.
async fn runs_then_is_aborted(
    handle: &ActorHandle<Worker>,
    killed_at: Instant,
    grace: Duration,
    paused: bool,
) {
    let bound = upper(paused, grace);
    within(
        bound.saturating_sub(killed_at.elapsed()),
        "the target to be aborted once its kill grace passed",
        handle.wait_stopped(),
    )
    .await;
    let ran = killed_at.elapsed();
    assert!(
        ran >= grace,
        "the target must still run its whole kill grace of {grace:?} after its Kill, stopped after {ran:?}"
    );
    assert_eq!(handle.status(), ActorStatus::Stopped);
}

// ---------------------------------------------------------------------------
// The grace of a root
// ---------------------------------------------------------------------------

async fn kill_grace_set_on_a_root_delays_its_abort_body(paused: bool) {
    let sys = ActorSystem::create(uname("root-grace")).unwrap();
    let probe = Arc::new(Probe::default());
    let name = uname("root");
    let grace = Duration::from_millis(600);
    let handle = spawn_root(&sys, &name, &probe, Some(grace)).await;
    park(&handle, &probe).await;

    let killed_at = Instant::now();
    sys.kill(&name).await.expect("the kill is accepted");
    assert!(sys.get::<Worker>(&name).is_some(), "still registered");
    runs_then_is_aborted(&handle, killed_at, grace, paused).await;
}

async fn default_kill_grace_applies_when_none_is_set_body(paused: bool) {
    let sys = ActorSystem::create(uname("default-grace")).unwrap();
    let probe = Arc::new(Probe::default());
    let name = uname("root");
    let handle = spawn_root(&sys, &name, &probe, None).await;
    park(&handle, &probe).await;

    let killed_at = Instant::now();
    sys.kill(&name).await.expect("the kill is accepted");
    runs_then_is_aborted(&handle, killed_at, ASSUMED_KILL_GRACE, paused).await;
}

#[test]
fn zero_kill_grace_aborts_at_once_and_is_not_a_verdict() {
    common::run(
        2,
        zero_kill_grace_aborts_at_once_and_is_not_a_verdict_body(false),
    );
}

async fn zero_kill_grace_aborts_at_once_and_is_not_a_verdict_body(paused: bool) {
    let sys = ActorSystem::create(uname("zero-grace")).unwrap();

    // Two roots killed together: a zero grace aborts at once, a long one
    // does not.
    let (probe_zero, probe_long) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
    let (zero_name, long_name) = (uname("zero"), uname("long"));
    let zero = spawn_root(&sys, &zero_name, &probe_zero, Some(Duration::ZERO)).await;
    let long = spawn_root(&sys, &long_name, &probe_long, Some(Duration::from_secs(2))).await;
    park(&zero, &probe_zero).await;
    park(&long, &probe_long).await;
    sys.kill(&zero_name).await.unwrap();
    sys.kill(&long_name).await.unwrap();
    within(
        upper(paused, Duration::ZERO),
        "the zero-grace root to be aborted",
        zero.wait_stopped(),
    )
    .await;
    sleep(Duration::from_millis(500)).await;
    assert_ne!(
        long.status(),
        ActorStatus::Stopped,
        "the root with a 2 s grace is still running while the zero-grace root is gone"
    );

    // A zero-grace root parked in `handle`, which never sees the graceful
    // request: the Kill step arms an abort at once, the call aborts the
    // task itself, and the wait after the abort is the settle time and not
    // the zero grace, so the call ends aborted, never unresponsive.
    let parked_probe = Arc::new(Probe::default());
    let parked_name = uname("zero-parked");
    let parked = spawn_root(&sys, &parked_name, &parked_probe, Some(Duration::ZERO)).await;
    park(&parked, &parked_probe).await;
    let outcome = within(
        BOUND,
        "stop_within of a parked zero-grace root",
        parked.stop_within(StopReason::Graceful, Duration::from_millis(50)),
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, StopOutcome::Aborted | StopOutcome::Killed),
        "a zero kill grace is not a verdict, got {outcome:?}"
    );

    // A zero-grace root that vetoes the graceful request: the call ends
    // aborted or killed, never in the unresponsive verdict.
    let vetoer = Vetoer
        .spawn()
        .named(uname("vetoer"))
        .on_system(&sys)
        .kill_grace(Duration::ZERO)
        .await
        .unwrap();
    let outcome = within(
        BOUND,
        "stop_within of a zero-grace vetoing root",
        vetoer.stop_within(StopReason::Graceful, Duration::from_millis(50)),
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, StopOutcome::Aborted | StopOutcome::Killed),
        "a zero kill grace is not a verdict, got {outcome:?}"
    );

    // A zero-grace supervised child under `Shutdown::Kill`.
    let probe = Arc::new(Probe::default());
    let sup = spawn_sup(&sys, &probe, true).await;
    let child_name = uname("child");
    spawn_child_via(&sup, &child_name, Duration::ZERO, Shutdown::Kill).await;
    let child = registered(&sys, &child_name).await;
    park(&child, &probe).await;
    let answer = within(
        BOUND,
        "terminate_child of a zero-grace child",
        sup.send(Cmd::Terminate(child_name.clone())),
    )
    .await
    .unwrap();
    assert_eq!(answer, Ok(()), "never ChildUnresponsive for a zero grace");
}

/// A zero-grace root inside a turn that never yields until the returned
/// guard drops: the abort cannot land, so every wait after it runs to its
/// end.
async fn blocked_zero_grace_root(
    sys: &Arc<ActorSystem>,
    base: &str,
) -> (ActorHandle<Worker>, ReleasesTheBlock) {
    let probe = Arc::new(Probe::default());
    let name = uname(base);
    let handle = spawn_root(sys, &name, &probe, Some(Duration::ZERO)).await;
    probe.parked.store(false, Ordering::SeqCst);
    probe.block_held.store(true, Ordering::SeqCst);
    let release = ReleasesTheBlock(probe.clone());
    handle.notify(Msg::Block).await.unwrap();
    wait_until(BOUND, "the turn to block", || {
        probe.parked.load(Ordering::SeqCst)
    })
    .await;
    (handle, release)
}

/// Vetoes every request to stop.
struct Vetoer;

impl Actor for Vetoer {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// The grace of a supervised child
// ---------------------------------------------------------------------------

/// The roster reads the child alive and not restarting, and a message to it is
/// answered: its latest incarnation is adopted and running.
async fn answers_a_ping(
    sys: &Arc<ActorSystem>,
    name: &str,
    probe: &Arc<Probe>,
) -> ActorHandle<Worker> {
    let before = probe.pings.load(Ordering::SeqCst);
    let mut answered = None;
    let deadline = Instant::now() + BOUND;
    while answered.is_none() {
        assert!(Instant::now() < deadline, "the child never answered");
        if let Some(handle) = sys.get::<Worker>(name) {
            if handle.is_alive() && handle.send(Msg::Ping).await.is_ok() {
                answered = Some(handle);
                continue;
            }
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(probe.pings.load(Ordering::SeqCst) > before);
    answered.unwrap()
}

async fn child_kill_grace_survives_restarts_body(paused: bool) {
    let sys = ActorSystem::create(uname("child-grace")).unwrap();
    let probe = Arc::new(Probe::default());
    let sup = spawn_sup(&sys, &probe, true).await;
    let name = uname("child");
    let grace = Duration::from_millis(600);
    spawn_child_via(
        &sup,
        &name,
        grace,
        Shutdown::Timeout(Duration::from_secs(5)),
    )
    .await;

    for _round in 0..2 {
        let built = probe.built.load(Ordering::SeqCst);
        // Crash the running incarnation; the supervisor builds the next.
        let victim = answers_a_ping(&sys, &name, &probe).await;
        victim.notify(Msg::Crash).await.unwrap();
        within(
            BOUND,
            "the crashed incarnation to stop",
            victim.wait_stopped(),
        )
        .await;
        wait_until(BOUND, "the factory to run again", || {
            probe.built.load(Ordering::SeqCst) == built + 1
        })
        .await;
        // The new incarnation answers a message, then parks.
        let current = answers_a_ping(&sys, &name, &probe).await;
        park(&current, &probe).await;

        let by_id = sys
            .get_by_id::<Worker>(current.id())
            .expect("registered by id");
        let killed_at = Instant::now();
        by_id
            .stop(StopReason::Kill)
            .await
            .expect("the kill is accepted");
        runs_then_is_aborted(&current, killed_at, grace, paused).await;
        // A permanent child: the kill is followed by another restart.
        wait_until(BOUND, "the factory to run after the kill", || {
            probe.built.load(Ordering::SeqCst) >= built + 2
        })
        .await;
    }
}

// ---------------------------------------------------------------------------
// Every kill entry point reads the target's own grace
// ---------------------------------------------------------------------------

/// Each case: a target with a 600 ms grace parked in `handle`; the entry point
/// raises its `Kill`; the target is `Stopped` no sooner than its grace after
/// the call and no later than the abort its grace schedules.
const OWN_GRACE: Duration = Duration::from_millis(600);

async fn parked_root(sys: &Arc<ActorSystem>, base: &str) -> (String, ActorHandle<Worker>) {
    let probe = Arc::new(Probe::default());
    let name = uname(base);
    let handle = spawn_root(sys, &name, &probe, Some(OWN_GRACE)).await;
    park(&handle, &probe).await;
    (name, handle)
}

#[test]
fn every_kill_entry_point_waits_the_target_s_own_kill_grace() {
    common::run(
        2,
        every_kill_entry_point_waits_the_target_s_own_kill_grace_body(false),
    );
}

async fn every_kill_entry_point_waits_the_target_s_own_kill_grace_body(paused: bool) {
    // The system's kill by name.
    {
        let sys = ActorSystem::create(uname("entry-kill")).unwrap();
        let (name, handle) = parked_root(&sys, "kill").await;
        let killed_at = Instant::now();
        sys.kill(&name).await.unwrap();
        runs_then_is_aborted(&handle, killed_at, OWN_GRACE, paused).await;
    }
    // The handle's stop with Kill.
    {
        let sys = ActorSystem::create(uname("entry-stop")).unwrap();
        let (_name, handle) = parked_root(&sys, "stop").await;
        let killed_at = Instant::now();
        handle.stop(StopReason::Kill).await.unwrap();
        runs_then_is_aborted(&handle, killed_at, OWN_GRACE, paused).await;
    }
    // The system's kill by id: the call itself waits, so it runs beside
    // the observation.
    {
        let sys = ActorSystem::create(uname("entry-kill-by-id")).unwrap();
        let (_name, handle) = parked_root(&sys, "by-id").await;
        let id = handle.id().clone();
        let killed_at = Instant::now();
        let caller = {
            let sys = sys.clone();
            tokio::spawn(async move { sys.kill_by_id(&id).await })
        };
        runs_then_is_aborted(&handle, killed_at, OWN_GRACE, paused).await;
        assert!(within(BOUND, "kill_by_id to return", caller)
            .await
            .unwrap()
            .is_some());
    }
    // stop_within: the Kill step comes after the graceful window.
    {
        let sys = ActorSystem::create(uname("entry-stop-within")).unwrap();
        let (_name, handle) = parked_root(&sys, "within").await;
        let window = Duration::from_millis(50);
        let started = Instant::now();
        let caller = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.stop_within(StopReason::Graceful, window).await })
        };
        // The graceful request is never seen by a parked turn: the Kill
        // step comes at the end of the window, and its grace after it.
        runs_then_is_aborted(&handle, started, window + OWN_GRACE, paused).await;
        let outcome = within(BOUND, "stop_within to return", caller)
            .await
            .unwrap();
        assert!(matches!(
            outcome.unwrap(),
            StopOutcome::Killed | StopOutcome::Aborted
        ));
    }
    // The system's shutdown: the Kill step of a root after its
    // per-actor timeout.
    {
        let sys = ActorSystem::create_with(
            uname("entry-shutdown"),
            SystemConfig {
                shutdown_policy: ShutdownPolicy {
                    timeout: Duration::from_secs(30),
                    per_actor_timeout: Duration::from_millis(50),
                },
                ..SystemConfig::default()
            },
        )
        .unwrap();
        let (_name, handle) = parked_root(&sys, "shutdown").await;
        let started = Instant::now();
        let caller = {
            let sys = sys.clone();
            tokio::spawn(async move { sys.shutdown().await })
        };
        runs_then_is_aborted(
            &handle,
            started,
            Duration::from_millis(50) + OWN_GRACE,
            paused,
        )
        .await;
        let report = within(BOUND, "shutdown to return", caller).await.unwrap();
        assert_eq!(report.outcomes.len(), 1);
    }
    // A supervisor's stop of a child under `Shutdown::Kill`.
    {
        let sys = ActorSystem::create(uname("entry-runner")).unwrap();
        let probe = Arc::new(Probe::default());
        let sup = spawn_sup(&sys, &probe, true).await;
        let name = uname("child");
        spawn_child_via(&sup, &name, OWN_GRACE, Shutdown::Kill).await;
        let child = registered(&sys, &name).await;
        park(&child, &probe).await;
        let killed_at = Instant::now();
        sup.notify(Cmd::Terminate(name.clone())).await.unwrap();
        runs_then_is_aborted(&child, killed_at, OWN_GRACE, paused).await;
    }
    // A worker whose supervisor is killed: its link guard.
    {
        let sys = ActorSystem::create(uname("entry-guard")).unwrap();
        let probe = Arc::new(Probe::default());
        let sup = spawn_sup(&sys, &probe, true).await;
        let name = uname("child");
        spawn_child_via(
            &sup,
            &name,
            OWN_GRACE,
            Shutdown::Timeout(Duration::from_secs(5)),
        )
        .await;
        let child = registered(&sys, &name).await;
        park(&child, &probe).await;
        let killed_at = Instant::now();
        sup.stop(StopReason::Kill).await.unwrap();
        runs_then_is_aborted(&child, killed_at, OWN_GRACE, paused).await;
    }
}

/// A grace beyond any practical bound keeps a parked turn alive across a
/// long advance of the clock, while `stop_within(Kill, d)` on a target with
/// that grace still aborts it after `d`.
#[test]
fn a_huge_grace_never_aborts_but_stop_within_kill_still_does() {
    common::run_paused(async {
        let sys = ActorSystem::create(uname("huge-grace")).unwrap();

        let probe = Arc::new(Probe::default());
        let name = uname("held");
        let held = spawn_root(&sys, &name, &probe, Some(Duration::MAX)).await;
        park(&held, &probe).await;
        sys.kill(&name).await.expect("the kill is accepted");
        sleep(Duration::from_secs(24 * 3600)).await;
        assert_ne!(
            held.status(),
            ActorStatus::Stopped,
            "a day after its Kill the turn still runs"
        );

        let probe = Arc::new(Probe::default());
        let name = uname("bounded");
        let window = Duration::from_secs(2);
        let target = spawn_root(&sys, &name, &probe, Some(Duration::MAX)).await;
        park(&target, &probe).await;
        let started = Instant::now();
        let outcome = within(
            BOUND,
            "stop_within(Kill) to return",
            target.stop_within(StopReason::Kill, window),
        )
        .await
        .unwrap();
        let took = started.elapsed();
        assert_eq!(outcome, StopOutcome::Aborted);
        assert!(
            took >= window && took <= window + PAUSED_SLACK,
            "the abort lands at the call's own window of {window:?}, not the grace: {took:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// The edge of the representable range
// ---------------------------------------------------------------------------

/// The largest duration `Instant::checked_add` accepts from now, less 500 us.
fn largest_addable() -> Duration {
    let base = Instant::now();
    let mut lo = Duration::ZERO;
    let mut hi = Duration::from_secs(u64::MAX / 2);
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

/// Panics whose text says a duration overflowed an instant, wherever they
/// happen: a panic inside a task that is being dropped is caught by the
/// runtime, so only a hook sees it.
static OVERFLOW_PANICS: AtomicUsize = AtomicUsize::new(0);

/// Counts overflow panics once for the whole binary: the parts below run in
/// parallel and share the count, which no part may see grow.
fn count_overflow_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if info.to_string().contains("overflow") {
                OVERFLOW_PANICS.fetch_add(1, Ordering::SeqCst);
            }
            previous(info);
        }));
    });
}

/// The three graces of the edge: thirty years (inside the range), the largest
/// the clock accepts from now, and `Duration::MAX`.
#[derive(Clone, Copy)]
enum Edge {
    ThirtyYears,
    LargestAddable,
    Max,
}

impl Edge {
    fn grace(self) -> Duration {
        match self {
            Edge::ThirtyYears => Duration::from_secs(86_400 * 365 * 30),
            Edge::LargestAddable => largest_addable(),
            Edge::Max => Duration::MAX,
        }
    }
}

/// A root with an edge-of-range grace: kill by name, kill by id, stop with
/// Kill and the shutdown's Kill step.
async fn edge_of_range_roots(grace: Duration) {
    for entry in 0..4usize {
        let sys = ActorSystem::create_with(
            uname("edge-root"),
            SystemConfig {
                shutdown_policy: ShutdownPolicy {
                    timeout: Duration::from_secs(1),
                    per_actor_timeout: Duration::from_millis(50),
                },
                ..SystemConfig::default()
            },
        )
        .unwrap();
        let probe = Arc::new(Probe::default());
        let name = uname("root");
        let handle = worker(&probe, false)
            .spawn()
            .named(&name)
            .on_system(&sys)
            .kill_grace(grace)
            .await
            .unwrap();
        park(&handle, &probe).await;
        let caller: Option<tokio::task::JoinHandle<()>> = match entry {
            0 => {
                sys.kill(&name).await.unwrap();
                None
            }
            1 => {
                let (sys, id) = (sys.clone(), handle.id().clone());
                Some(tokio::spawn(async move {
                    sys.kill_by_id(&id).await;
                }))
            }
            2 => {
                handle.stop(StopReason::Kill).await.unwrap();
                None
            }
            _ => {
                let sys = sys.clone();
                Some(tokio::spawn(async move {
                    sys.shutdown().await;
                }))
            }
        };
        // Nothing is aborted within the horizon of the test.
        sleep(Duration::from_millis(600)).await;
        assert_ne!(handle.status(), ActorStatus::Stopped, "entry {entry}");
        // The turn ends and the target leaves at its next boundary.
        probe.gate.notify_one();
        within(
            BOUND,
            "the target to end at its boundary",
            handle.wait_stopped(),
        )
        .await;
        if let Some(caller) = caller {
            within(BOUND, "the calling path to return", caller)
                .await
                .unwrap();
        }
    }
}

/// A supervised child with an edge-of-range grace: the supervisor's
/// `Shutdown::Kill` ladder, a killed supervisor's link guard, and a restart
/// attempt's start timeout.
async fn edge_of_range_children(grace: Duration) {
    for entry in 0..3usize {
        let sys = ActorSystem::create(uname("edge-child")).unwrap();
        let probe = Arc::new(Probe::default());
        let sup = spawn_sup(&sys, &probe, false).await;
        let name = uname("child");
        let shutdown = if entry == 0 {
            Shutdown::Kill
        } else {
            Shutdown::Timeout(Duration::from_secs(5))
        };
        // Long enough that a loaded scheduler always polls the
        // restarted incarnation into its hung init before the start
        // timeout raises its Kill: the subject is the grace, not the
        // start timeout.
        let start_timeout = (entry == 2).then_some(Duration::from_secs(2));
        let answer = sup
            .send(Cmd::Spawn {
                name: name.clone(),
                grace,
                shutdown,
                start_timeout,
            })
            .await
            .unwrap();
        answer.expect("the child spawns");
        let child = registered(&sys, &name).await;
        match entry {
            0 => {
                park(&child, &probe).await;
                sup.notify(Cmd::Terminate(name.clone())).await.unwrap();
            }
            1 => {
                park(&child, &probe).await;
                sup.stop(StopReason::Kill).await.unwrap();
            }
            _ => {
                // The restart's init hangs past its start timeout, so
                // the attempt kills the new incarnation.
                probe.hang_restart_init.store(true, Ordering::SeqCst);
                child.notify(Msg::Crash).await.unwrap();
                within(BOUND, "the crashed incarnation", child.wait_stopped()).await;
                wait_until(BOUND, "the restart to be attempted", || {
                    probe.built.load(Ordering::SeqCst) == 2
                })
                .await;
            }
        }
        // Past the start timeout for entry 2, so its Kill was raised and
        // only the edge-of-range grace keeps the incarnation running.
        let horizon = start_timeout.unwrap_or_default() + Duration::from_millis(600);
        sleep(horizon).await;
        if entry < 2 {
            assert_ne!(child.status(), ActorStatus::Stopped, "child entry {entry}");
            probe.gate.notify_one();
            within(
                BOUND,
                "the child to end at its boundary",
                child.wait_stopped(),
            )
            .await;
        } else {
            let hung = sys.get::<Worker>(&name).expect("the restarted incarnation");
            assert_ne!(hung.status(), ActorStatus::Stopped, "child entry {entry}");
            probe.hang_restart_init.store(false, Ordering::SeqCst);
            probe.init_gate.notify_waiters();
            // Released from its init, the incarnation ends instead of
            // being adopted: the start timeout's Kill, held by its
            // lane through the edge-of-range grace, is what ends it.
            within(BOUND, "the hung incarnation to end", hung.wait_stopped()).await;
        }
    }
}

fn edge_of_range_never_panics(edge: Edge, roots: bool) {
    count_overflow_panics();
    common::run(2, async move {
        let before = OVERFLOW_PANICS.load(Ordering::SeqCst);
        let grace = edge.grace();
        if roots {
            edge_of_range_roots(grace).await;
        } else {
            edge_of_range_children(grace).await;
        }
        assert_eq!(
            OVERFLOW_PANICS.load(Ordering::SeqCst),
            before,
            "no path overflowed an instant with an edge-of-range grace"
        );
    });
}

// ---------------------------------------------------------------------------
// The abort follows the target's runtime
// ---------------------------------------------------------------------------

fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime")
}

fn two_worker_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a multi-thread runtime")
}

// Plain synchronous tests that build their own runtimes: a `block_on` inside
// an async context panics, and a runtime that is kept undriven is shut down
// with `shutdown_background`, never dropped.

#[test]
fn kill_backstop_fires_while_another_runtime_hosts_a_never_driven_reaper() {
    let host = current_thread_runtime();
    let rt = two_worker_runtime();
    let sys = ActorSystem::create(uname("never-driven-host")).unwrap();

    // The system's first reaper is created on `host`: a supervisor spawned
    // there spawns a child.
    let host_probe = Arc::new(Probe::default());
    host.block_on(async {
        let sup = spawn_sup(&sys, &host_probe, true).await;
        spawn_child_via(
            &sup,
            &uname("host-child"),
            ASSUMED_KILL_GRACE,
            Shutdown::Timeout(Duration::from_secs(5)),
        )
        .await;
    });
    // `host` is never driven again and stays alive to the end.

    let probe = Arc::new(Probe::default());
    let name = uname("root");
    let grace = Duration::from_millis(100);
    let handle = rt.block_on(async {
        let handle = spawn_root(&sys, &name, &probe, Some(grace)).await;
        park(&handle, &probe).await;
        handle
    });
    rt.block_on(async {
        sys.kill(&name).await.expect("the kill is accepted");
        within(
            upper(false, grace + ASSUMED_ABORT_SETTLE),
            "the root to be aborted by the reaper of its own runtime",
            handle.wait_stopped(),
        )
        .await;
    });
    rt.shutdown_background();
    host.shutdown_background();
}
