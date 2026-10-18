//! A restart, a `terminate_child` and a `spawn_child` under a reused id wait
//! until every actor the previous incarnation spawned has ended.
//!
//! The tree: a root `Top` supervises a supervisor child `Sub`, which spawns
//! its workers in `on_started`. A worker is named (a second one under the same
//! name is refused while the first lives) or anonymous (a count of live
//! instances shows an overlap). The first `Sub` incarnation puts its workers
//! inside a long turn, so they are still alive when the `Sub` is killed.
//!
//! Every outcome a test awaits is bounded, and the order of events is read
//! from one timeline the actors append to, never from head-start sleeps.

use super::*;

use tokio_actors::{ActorStatus, SpawnError};

// ---------------------------------------------------------------------------
// The timeline and the counters
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Probe {
    /// Workers alive now, and the most that were alive at once.
    w_live: AtomicUsize,
    w_max: AtomicUsize,
    w_built: AtomicUsize,
    /// `Sub` incarnations built, and the `Sub`s whose `on_started` finished.
    p_built: AtomicUsize,
    p_started: AtomicUsize,
    /// Plain children (`a` and `q`) built.
    q_built: AtomicUsize,
    /// A held worker's turn ends when the test says so.
    release: tokio::sync::Notify,
    /// Set by the test to end every blocked turn before its length.
    unblock: AtomicBool,
    /// Set by the test to end the root's held handler.
    top_release: AtomicBool,
    /// How long dropping a worker value takes, in milliseconds. Zero reads as
    /// the default of 60. A worker whose turn an abort cut drops at once.
    drop_ms: AtomicU64,
    /// Errors of a worker spawn inside `Sub::on_started`.
    spawn_errs: Mutex<Vec<String>>,
    /// Events in the order they happened.
    timeline: Mutex<Vec<String>>,
    /// What the root's `on_child_stopped` was given: child, reason, action,
    /// and the root's status at that moment.
    reports: Mutex<Vec<(String, String, SupervisionAction, ActorStatus)>>,
    /// The reason the root stopped with.
    top_stopped: Mutex<Option<String>>,
}

impl Probe {
    fn note(&self, line: impl Into<String>) {
        self.timeline.lock().unwrap().push(line.into());
    }

    /// The position of the first timeline line that starts with `prefix`.
    fn at(&self, prefix: &str) -> Option<usize> {
        self.timeline
            .lock()
            .unwrap()
            .iter()
            .position(|l| l.starts_with(prefix))
    }

    fn starts_with(&self, prefix: &str) -> Option<String> {
        self.timeline
            .lock()
            .unwrap()
            .iter()
            .find(|l| l.starts_with(prefix))
            .cloned()
    }

    fn lines(&self) -> Vec<String> {
        self.timeline.lock().unwrap().clone()
    }

    fn live(&self) -> usize {
        self.w_live.load(Ordering::SeqCst)
    }

    fn built_p(&self) -> usize {
        self.p_built.load(Ordering::SeqCst)
    }

    fn errs(&self) -> Vec<String> {
        self.spawn_errs.lock().unwrap().clone()
    }

    /// Asserts that line `first` precedes line `second` on the timeline.
    fn assert_before(&self, first: &str, second: &str) {
        let (a, b) = (self.at(first), self.at(second));
        assert!(
            matches!((a, b), (Some(a), Some(b)) if a < b),
            "{first:?} ({a:?}) must precede {second:?} ({b:?}): {:?}",
            self.lines()
        );
    }

    fn reports_of(&self, child: &str) -> Vec<SupervisionAction> {
        self.reports
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, ..)| name == child)
            .map(|(_, _, action, _)| *action)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The worker: a turn that awaits, or one that blocks its thread
// ---------------------------------------------------------------------------

struct Worker {
    probe: Arc<Probe>,
    id: usize,
    /// Set while a turn runs: still set at the drop when an abort cut the
    /// turn.
    in_turn: bool,
}

impl Worker {
    fn new(probe: Arc<Probe>) -> Self {
        let id = probe.w_built.fetch_add(1, Ordering::SeqCst) + 1;
        let live = probe.w_live.fetch_add(1, Ordering::SeqCst) + 1;
        probe.w_max.fetch_max(live, Ordering::SeqCst);
        Self {
            probe,
            id,
            in_turn: false,
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // A slow drop: a successor that starts as soon as the subtree reads
        // as ended, but before this value is gone, shows as two live workers.
        // A worker whose turn an abort cut drops at once: a wait gives an
        // aborted actor the settle time to end before its verdict, and the
        // end must land inside it however late the host runs the drop.
        let ms = self.probe.drop_ms.load(Ordering::SeqCst);
        if !self.in_turn {
            std::thread::sleep(Duration::from_millis(if ms == 0 { 60 } else { ms }));
        }
        self.probe.w_live.fetch_sub(1, Ordering::SeqCst);
        self.probe.note(format!("w_dropped:{}", self.id));
    }
}

#[derive(Clone)]
enum WMsg {
    /// A turn that awaits for this many milliseconds.
    Busy(u64),
    /// A turn that blocks its thread for this many milliseconds, or until
    /// the test sets the probe's `unblock`.
    Block(u64),
    /// A turn that awaits until the test releases it.
    Held,
}

impl Actor for Worker {
    type Message = WMsg;
    type Response = ();

    async fn handle(&mut self, msg: WMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.probe.note(format!("w_turn_started:{}", self.id));
        self.in_turn = true;
        match msg {
            WMsg::Busy(ms) => sleep(Duration::from_millis(ms)).await,
            WMsg::Block(ms) => {
                let until = std::time::Instant::now() + Duration::from_millis(ms);
                while !self.probe.unblock.load(Ordering::SeqCst)
                    && std::time::Instant::now() < until
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            WMsg::Held => self.probe.release.notified().await,
        }
        self.in_turn = false;
        self.probe.note(format!("w_turn_ended:{}", self.id));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The supervisor child
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct SubSetup {
    /// A named worker, spawned with this kill grace and shutdown policy.
    worker: Option<String>,
    w_grace: Option<Duration>,
    w_shutdown: Option<Shutdown>,
    /// Anonymous workers.
    anon: usize,
    /// The first incarnation's workers take one turn of this length (an
    /// awaiting turn, or one that blocks the thread).
    busy_ms: u64,
    block_ms: u64,
    /// The first incarnation's workers take a turn that the test ends.
    held: bool,
    /// Every incarnation's workers take that turn, not only the first's.
    busy_all: bool,
    /// `pre_stop` vetoes every request, or takes this long first.
    veto_stop: bool,
    slow_pre_stop_ms: u64,
    /// The incarnation with this number hangs in `on_started` after it
    /// spawned its workers.
    hang_on_build: Option<usize>,
}

struct Sub {
    probe: Arc<Probe>,
    setup: SubSetup,
    seq: usize,
}

impl Sub {
    fn new(probe: Arc<Probe>, setup: SubSetup) -> Self {
        let seq = probe.p_built.fetch_add(1, Ordering::SeqCst) + 1;
        probe.note(format!("p_built:{seq}"));
        Self { probe, setup, seq }
    }

    async fn spawn_worker(
        &self,
        ctx: &mut ActorContext<Self>,
        name: Option<String>,
    ) -> ActorResult<()> {
        let probe = self.probe.clone();
        let mut builder = ctx.spawn_child(move || Worker::new(probe.clone()));
        if let Some(name) = name {
            builder = builder.named(name);
        }
        if let Some(grace) = self.setup.w_grace {
            builder = builder.kill_grace(grace);
        }
        if let Some(shutdown) = self.setup.w_shutdown {
            builder = builder.shutdown(shutdown);
        }
        match builder.await {
            Ok(handle) => {
                let turn = self.seq == 1 || self.setup.busy_all;
                if turn && self.setup.busy_ms > 0 {
                    let _ = handle.notify(WMsg::Busy(self.setup.busy_ms)).await;
                }
                if turn && self.setup.held {
                    let _ = handle.notify(WMsg::Held).await;
                }
                if turn && self.setup.block_ms > 0 {
                    let _ = handle.notify(WMsg::Block(self.setup.block_ms)).await;
                }
                Ok(())
            }
            Err(err) => {
                self.probe
                    .spawn_errs
                    .lock()
                    .unwrap()
                    .push(format!("{err:?}"));
                Err(err.into())
            }
        }
    }
}

impl Actor for Sub {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(name) = self.setup.worker.clone() {
            self.spawn_worker(ctx, Some(name)).await?;
        }
        for _ in 0..self.setup.anon {
            self.spawn_worker(ctx, None).await?;
        }
        self.probe.p_started.fetch_add(1, Ordering::SeqCst);
        self.probe.note(format!("p_started:{}", self.seq));
        if self.setup.hang_on_build == Some(self.seq) {
            sleep(Duration::from_secs(3600)).await;
        }
        self.probe.note(format!("p_ready:{}", self.seq));
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        if self.setup.slow_pre_stop_ms > 0 {
            sleep(Duration::from_millis(self.setup.slow_pre_stop_ms)).await;
        }
        !self.setup.veto_stop
    }
}

// ---------------------------------------------------------------------------
// A plain sibling: crashes on command
// ---------------------------------------------------------------------------

struct Plain {
    probe: Arc<Probe>,
}

impl Plain {
    fn new(probe: Arc<Probe>) -> Self {
        probe.q_built.fetch_add(1, Ordering::SeqCst);
        Self { probe }
    }
}

impl Actor for Plain {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _ = &self.probe;
        Err(ActorError::user("crashed on command"))
    }
}

// ---------------------------------------------------------------------------
// The root
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TopSetup {
    p_name: String,
    p_restart: RestartType,
    p_shutdown: Shutdown,
    p_start_timeout: Option<Duration>,
    sub: SubSetup,
    /// A plain child spawned before `Sub`, and one spawned after it.
    a_name: Option<String>,
    q_name: Option<String>,
    /// A report of this child makes the root spawn `Sub` again under its id.
    respawn_on: Option<String>,
    sys: Option<Arc<ActorSystem>>,
}

struct Top {
    probe: Arc<Probe>,
    setup: TopSetup,
}

#[derive(Clone)]
enum TopMsg {
    Setup,
    /// `stop_child(P)`.
    StopP,
    /// `terminate_child(P)`, logged with its answer, its duration and the
    /// number of live workers when it returned.
    TerminateP,
    /// `terminate_child(P)` abandoned 20 ms after its commit.
    TerminateAbandoned,
    /// `delete_child(P)` and a same-id `spawn_child(P)`.
    DeleteSpawn,
    /// `terminate_child`, `delete_child` and a same-id spawn in one handler.
    Cycle,
    /// `stop_child(P)` and a same-id spawn in one handler.
    StopSpawn,
    /// Kills the plain child `q` from inside the handler, waits until it
    /// reads `Stopped`, then holds the handler for this many milliseconds.
    KillQThenHold(u64),
    /// Kills `q` the same way, then holds the handler until the test sets
    /// the probe's `top_release` (for at most `GATE_CAP`).
    KillQThenHoldUntilReleased,
}

/// Sets the probe's `top_release` when dropped, so the root's held handler
/// ends however the test ends.
struct ReleasesTheTop(Arc<Probe>);

impl Drop for ReleasesTheTop {
    fn drop(&mut self) {
        self.0.top_release.store(true, Ordering::SeqCst);
    }
}

impl Top {
    async fn spawn_p(&self, ctx: &mut ActorContext<Self>) -> Result<(), SpawnError> {
        let probe = self.probe.clone();
        let sub = self.setup.sub.clone();
        let mut builder = ctx
            .spawn_child(move || Sub::new(probe.clone(), sub.clone()))
            .named(self.setup.p_name.clone())
            .restart_type(self.setup.p_restart)
            .shutdown(self.setup.p_shutdown)
            .with_config(ActorConfig::default().supervisor());
        if let Some(timeout) = self.setup.p_start_timeout {
            builder = builder.start_timeout(timeout);
        }
        builder.await.map(|_| ())
    }

    async fn spawn_plain(&self, ctx: &mut ActorContext<Self>, name: &str) -> ActorResult<()> {
        let probe = self.probe.clone();
        ctx.spawn_child(move || Plain::new(probe.clone()))
            .named(name.to_string())
            .await?;
        Ok(())
    }

    fn live_children(&self, ctx: &ActorContext<Self>) -> String {
        let info = ctx
            .children()
            .into_iter()
            .find(|c| c.name.as_deref() == Some(self.setup.p_name.as_str()));
        match info {
            Some(c) => format!("alive={} pending={}", c.is_alive, c.restart_pending),
            None => "absent".to_string(),
        }
    }
}

impl Actor for Top {
    type Message = TopMsg;
    type Response = ();

    async fn handle(&mut self, msg: TopMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let p_name = self.setup.p_name.clone();
        match msg {
            TopMsg::Setup => {
                if let Some(name) = self.setup.a_name.clone() {
                    self.spawn_plain(ctx, &name).await?;
                }
                self.spawn_p(ctx).await?;
                if let Some(name) = self.setup.q_name.clone() {
                    self.spawn_plain(ctx, &name).await?;
                }
            }
            TopMsg::StopP => {
                let t = Instant::now();
                let r = ctx.stop_child(p_name).await;
                self.probe
                    .note(format!("stop_child:{r:?} in {:?}", t.elapsed()));
            }
            TopMsg::TerminateP => {
                let t = Instant::now();
                let r = ctx.terminate_child(p_name).await;
                let elapsed = t.elapsed();
                self.probe.note(format!(
                    "terminate:{r:?} micros={} live={} {}",
                    elapsed.as_micros(),
                    self.probe.live(),
                    self.live_children(ctx)
                ));
            }
            TopMsg::TerminateAbandoned => {
                let _ = timeout(Duration::from_millis(20), ctx.terminate_child(p_name)).await;
                self.probe.note("terminate_abandoned");
            }
            TopMsg::DeleteSpawn => {
                let deleted = ctx.delete_child(p_name);
                let t = Instant::now();
                let spawned = self.spawn_p(ctx).await;
                self.probe.note(format!(
                    "delete:{deleted:?} spawn:{spawned:?} micros={} live={}",
                    t.elapsed().as_micros(),
                    self.probe.live()
                ));
            }
            TopMsg::Cycle => {
                let t = Instant::now();
                let terminated = ctx.terminate_child(p_name.clone()).await;
                let live = self.probe.live();
                let deleted = ctx.delete_child(p_name);
                let spawned = self.spawn_p(ctx).await;
                self.probe.note(format!(
                    "cycle terminate:{terminated:?} live_after_terminate={live} \
                     delete:{deleted:?} spawn:{spawned:?} in {:?}",
                    t.elapsed()
                ));
            }
            TopMsg::StopSpawn => {
                let stopped = ctx.stop_child(p_name).await;
                let t = Instant::now();
                let spawned = self.spawn_p(ctx).await;
                self.probe.note(format!(
                    "stop_spawn stop:{stopped:?} spawn:{spawned:?} in {:?}",
                    t.elapsed()
                ));
            }
            TopMsg::KillQThenHold(ms) => {
                let sys = self.setup.sys.clone().expect("the system");
                let q_name = self.setup.q_name.clone().expect("a plain child q");
                let q = sys.get::<Plain>(&q_name).expect("q runs");
                sys.kill(&q_name).await.expect("kill q");
                within(BOUND, "q to read Stopped", q.wait_stopped()).await;
                self.probe.note("q_stopped");
                sleep(Duration::from_millis(ms)).await;
            }
            TopMsg::KillQThenHoldUntilReleased => {
                let sys = self.setup.sys.clone().expect("the system");
                let q_name = self.setup.q_name.clone().expect("a plain child q");
                let q = sys.get::<Plain>(&q_name).expect("q runs");
                sys.kill(&q_name).await.expect("kill q");
                within(BOUND, "q to read Stopped", q.wait_stopped()).await;
                self.probe.note("q_stopped");
                let cap = Instant::now() + GATE_CAP;
                while !self.probe.top_release.load(Ordering::SeqCst) && Instant::now() < cap {
                    sleep(Duration::from_millis(5)).await;
                }
            }
        }
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        ev: &ChildEvent,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let name = ev.child_name.clone().unwrap_or_default();
        self.probe.reports.lock().unwrap().push((
            name.clone(),
            format!("{:?}", ev.reason),
            ev.action,
            ctx.status(),
        ));
        self.probe.note(format!("report:{name}:{:?}", ev.action));
        if self.setup.respawn_on.as_deref() == Some(name.as_str()) {
            let before = self.probe.built_p();
            let t = Instant::now();
            let spawned = self.spawn_p(ctx).await;
            self.probe.note(format!(
                "respawn_in_report spawn:{spawned:?} micros={} built_delta={}",
                t.elapsed().as_micros(),
                self.probe.built_p() - before
            ));
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.probe.top_stopped.lock().unwrap() = Some(format!("{reason:?}"));
        self.probe.note("g_stopped");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The world a test runs in
// ---------------------------------------------------------------------------

struct World {
    sys: Arc<ActorSystem>,
    top: ActorHandle<Top>,
    probe: Arc<Probe>,
    p_name: String,
    q_name: Option<String>,
    expected_workers: usize,
}

fn setup(shutdown: Shutdown, sub: SubSetup) -> TopSetup {
    TopSetup {
        p_name: uname("p"),
        p_restart: RestartType::Permanent,
        p_shutdown: shutdown,
        p_start_timeout: None,
        sub,
        a_name: None,
        q_name: None,
        respawn_on: None,
        sys: None,
    }
}

fn named_worker(sub: SubSetup) -> SubSetup {
    SubSetup {
        worker: Some(uname("w")),
        ..sub
    }
}

/// One restart per minute: a second charge exhausts the budget.
fn one_restart() -> SupervisionConfig {
    SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60))
}

/// The bound of a wait on a subtree whose one worker has the default kill
/// grace: the worker's budget (its grace and the settle time) and the slack of
/// one more settle time.
const WORKER_BOUND: Duration = Duration::from_millis(
    ASSUMED_KILL_GRACE.as_millis() as u64 + 2 * ASSUMED_ABORT_SETTLE.as_millis() as u64,
);

/// How far past its bound a wait may end before the test fails: loose
/// enough for a host that stalls its threads, and far short of the
/// `LONG_BLOCK` turn a wait that outlived its bound would end with.
const BOUND_SLACK: Duration = STALL_SLACK;

/// The blocked turn of the tests that time a wait at its bound: it holds its
/// thread far longer than the bound and the slack, and the test ends it once
/// its checks are done.
const LONG_BLOCK_MS: u64 = 15_000;

/// Sets the probe's `unblock` when dropped, so a blocked turn ends however
/// the test ends.
struct Unblocks(Arc<Probe>);

impl Drop for Unblocks {
    fn drop(&mut self) {
        self.0.unblock.store(true, Ordering::SeqCst);
    }
}

/// The duration a timeline line recorded as `micros=N`.
fn recorded(line: &str) -> Duration {
    let micros: u64 = line
        .split("micros=")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .expect("a recorded duration");
    Duration::from_micros(micros)
}

/// Asserts that a wait ended at its bound: not before it, and within the
/// slack after it.
fn assert_at_the_bound(waited: Duration, what: &str) {
    assert!(
        waited >= WORKER_BOUND && waited < WORKER_BOUND + BOUND_SLACK,
        "{what} ended after {waited:?}, its bound is {WORKER_BOUND:?}"
    );
}

async fn start(config: SupervisionConfig, mut top_setup: TopSetup) -> World {
    let sys = ActorSystem::create(uname("sys")).unwrap();
    top_setup.sys = Some(sys.clone());
    let probe = Arc::new(Probe::default());
    let expected_workers = usize::from(top_setup.sub.worker.is_some()) + top_setup.sub.anon;
    let p_name = top_setup.p_name.clone();
    let q_name = top_setup.q_name.clone();
    let top = Top {
        probe: probe.clone(),
        setup: top_setup,
    }
    .spawn()
    .named(uname("top"))
    .on_system(&sys)
    .with_supervision(config)
    .await
    .unwrap();
    top.notify(TopMsg::Setup).await.unwrap();
    let observed = probe.clone();
    wait_until(BOUND, "the first workers to be built", move || {
        observed.w_built.load(Ordering::SeqCst) >= expected_workers
            && observed.p_started.load(Ordering::SeqCst) >= 1
    })
    .await;
    World {
        sys,
        top,
        probe,
        p_name,
        q_name,
        expected_workers,
    }
}

impl World {
    async fn send(&self, msg: TopMsg) {
        self.top.notify(msg).await.unwrap();
    }

    async fn wait_line(&self, what: &str, prefix: &'static str) -> String {
        let probe = self.probe.clone();
        wait_until(BOUND, what, move || probe.starts_with(prefix).is_some()).await;
        self.probe.starts_with(prefix).unwrap()
    }

    async fn wait_started(&self, n: usize) {
        let probe = self.probe.clone();
        wait_until(BOUND, "the supervisor child to start again", move || {
            probe.p_started.load(Ordering::SeqCst) >= n
        })
        .await;
    }

    async fn wait_no_workers(&self) {
        let probe = self.probe.clone();
        wait_until(BOUND, "every old worker to end", move || probe.live() == 0).await;
    }

    fn top_running(&self) -> bool {
        self.top.status() == ActorStatus::Running
    }

    fn assert_no_collision(&self) {
        assert!(self.probe.errs().is_empty(), "{:?}", self.probe.errs());
        assert!(
            !self.probe.lines().iter().any(|l| l.contains("NameTaken")),
            "{:?}",
            self.probe.lines()
        );
    }

    fn assert_no_budget_exhaustion(&self) {
        assert!(
            self.probe
                .reports
                .lock()
                .unwrap()
                .iter()
                .all(|(_, _, action, _)| *action != SupervisionAction::BudgetExhausted),
            "{:?}",
            self.probe.lines()
        );
    }
}

/// The five ways the supervisor child is ended while its worker is mid-turn.
#[derive(Clone, Copy, Debug)]
enum KillPath {
    /// `ActorSystem::kill(name)`.
    ByName,
    /// `ActorSystem::kill_by_id`.
    ById,
    /// A sibling crash in a `OneForAll` group whose stop kills the child
    /// (policy `Kill`).
    GroupKillPolicy,
    /// The same, with a window the child's slow `pre_stop` outlives, so the
    /// group's own ladder escalates to `Kill`.
    GroupTimeoutEscalated,
    /// The escalated path with a worker under `Kill`, a long grace and a long
    /// turn, so it outlives the child by the whole grace: only a budget
    /// derived from that grace covers it.
    GroupTimeoutEscalatedLongGrace,
    /// `stop_child` on a child under policy `Kill`.
    BounceKillPolicy,
}

async fn kill_path_scenario(path: KillPath) {
    let escalated = matches!(
        path,
        KillPath::GroupTimeoutEscalated | KillPath::GroupTimeoutEscalatedLongGrace
    );
    let long_grace = matches!(path, KillPath::GroupTimeoutEscalatedLongGrace);
    let sub = named_worker(SubSetup {
        busy_ms: if long_grace { 1000 } else { 300 },
        w_shutdown: long_grace.then_some(Shutdown::Kill),
        w_grace: long_grace.then_some(Duration::from_millis(500)),
        slow_pre_stop_ms: if escalated { 200 } else { 0 },
        ..SubSetup::default()
    });
    let shutdown = match path {
        KillPath::GroupKillPolicy | KillPath::BounceKillPolicy => Shutdown::Kill,
        KillPath::GroupTimeoutEscalated | KillPath::GroupTimeoutEscalatedLongGrace => {
            Shutdown::Timeout(Duration::from_millis(50))
        }
        KillPath::ByName | KillPath::ById => Shutdown::default(),
    };
    let mut top_setup = setup(shutdown, sub);
    let group = matches!(
        path,
        KillPath::GroupKillPolicy
            | KillPath::GroupTimeoutEscalated
            | KillPath::GroupTimeoutEscalatedLongGrace
    );
    if group {
        top_setup.a_name = Some(uname("a"));
    }
    let a_name = top_setup.a_name.clone();
    let config = if group {
        SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60))
    } else {
        one_restart()
    };
    let world = start(config, top_setup).await;
    // The worker is inside its first turn.
    match path {
        KillPath::ByName => world.sys.kill(&world.p_name).await.unwrap(),
        KillPath::ById => {
            let id = world.sys.get::<Sub>(&world.p_name).unwrap().id().clone();
            assert!(world.sys.kill_by_id(&id).await.is_some());
        }
        KillPath::GroupKillPolicy
        | KillPath::GroupTimeoutEscalated
        | KillPath::GroupTimeoutEscalatedLongGrace => {
            let a = world.sys.get::<Plain>(&a_name.unwrap()).unwrap();
            a.notify(()).await.unwrap();
        }
        KillPath::BounceKillPolicy => world.send(TopMsg::StopP).await,
    }
    world.wait_started(2).await;
    sleep(QUIET).await;
    assert!(world.top_running(), "{path:?}: {:?}", world.probe.lines());
    world.assert_no_collision();
    world.assert_no_budget_exhaustion();
    assert_eq!(
        world.probe.p_started.load(Ordering::SeqCst),
        2,
        "{path:?}: {:?}",
        world.probe.lines()
    );
    world.probe.assert_before("w_dropped:1", "p_built:2");
}

/// Six variants, one per path to the child's `Kill` and the escalated path a
/// second time with a long worker grace, each on both runtime flavors through
/// `common::run`.
mod restart_of_a_killed_supervisor_waits_for_its_old_subtree_to_release_its_names {
    use super::*;

    #[test]
    fn by_name() {
        common::run(4, async { kill_path_scenario(KillPath::ByName).await });
    }

    #[test]
    fn by_id() {
        common::run(4, async { kill_path_scenario(KillPath::ById).await });
    }

    #[test]
    fn group_kill_policy() {
        common::run(4, async {
            kill_path_scenario(KillPath::GroupKillPolicy).await
        });
    }

    #[test]
    fn group_timeout_escalated() {
        common::run(4, async {
            kill_path_scenario(KillPath::GroupTimeoutEscalated).await
        });
    }

    #[test]
    fn group_timeout_escalated_long_grace() {
        common::run(4, async {
            kill_path_scenario(KillPath::GroupTimeoutEscalatedLongGrace).await
        });
    }

    #[test]
    fn bounce_kill_policy() {
        common::run(4, async {
            kill_path_scenario(KillPath::BounceKillPolicy).await
        });
    }
}

#[test]
fn restart_of_a_killed_supervisor_with_an_idle_named_worker_never_collides() {
    common::run(4, async {
        let sub = named_worker(SubSetup::default());
        let world = start(one_restart(), setup(Shutdown::default(), sub)).await;
        world.sys.kill(&world.p_name).await.unwrap();
        world.wait_started(2).await;
        sleep(QUIET).await;
        assert!(world.top_running());
        world.assert_no_collision();
        world.assert_no_budget_exhaustion();
        assert_eq!(
            world.probe.reports_of(&world.p_name),
            vec![SupervisionAction::RestartInitiated]
        );
    });
}

#[test]
fn graceful_stop_of_a_supervisor_child_restarts_it_after_its_busy_worker_ended() {
    common::run(4, async {
        let sub = named_worker(SubSetup {
            busy_ms: 300,
            ..SubSetup::default()
        });
        let world = start(one_restart(), setup(Shutdown::default(), sub)).await;
        world.sys.stop(&world.p_name).await.unwrap();
        world.wait_started(2).await;
        sleep(QUIET).await;
        assert!(world.top_running());
        world.assert_no_collision();
        world.probe.assert_before("w_dropped:1", "p_built:2");
    });
}

#[test]
fn one_worker_crash_does_not_storm_the_grandparent() {
    common::run(4, async {
        // `Top` is OneForAll over the supervisor child (window 1 s) and a
        // sibling; the child's worker (window 5 s) sits in a 3 s turn. The
        // sibling's crash stops the child, whose orderly stop outlives its
        // window, so it is aborted while its worker still holds the name.
        let sub = named_worker(SubSetup {
            busy_ms: 3000,
            w_shutdown: Some(Shutdown::Timeout(Duration::from_secs(5))),
            ..SubSetup::default()
        });
        let mut top_setup = setup(Shutdown::Timeout(Duration::from_secs(1)), sub);
        top_setup.q_name = Some(uname("q"));
        let q_name = top_setup.q_name.clone().unwrap();
        let world = start(SupervisionConfig::one_for_all(), top_setup).await;
        world
            .sys
            .get::<Plain>(&q_name)
            .unwrap()
            .notify(())
            .await
            .unwrap();
        world.wait_started(2).await;
        sleep(QUIET * 2).await;
        assert!(world.top_running(), "{:?}", world.probe.lines());
        world.assert_no_collision();
        world.assert_no_budget_exhaustion();
    });
}

#[test]
fn grandparent_survives_a_busy_named_grandchild_at_its_supervisor_s_kill() {
    // The busy and the idle worker, five runs each: the kill never reaches
    // the grandparent's budget and the names are always free.
    for busy_ms in [0, 2000] {
        for _ in 0..5 {
            common::run(4, async move {
                let sub = named_worker(SubSetup {
                    busy_ms,
                    ..SubSetup::default()
                });
                let world = start(one_restart(), setup(Shutdown::default(), sub)).await;
                world.sys.kill(&world.p_name).await.unwrap();
                world.wait_started(2).await;
                sleep(QUIET).await;
                assert!(
                    world.top_running(),
                    "busy {busy_ms}: {:?}",
                    world.probe.lines()
                );
                world.assert_no_collision();
                world.assert_no_budget_exhaustion();
            });
        }
    }
}

#[test]
fn anonymous_grandchildren_of_a_killed_supervisor_never_overlap_its_restart() {
    // Workers inside a turn when their supervisor dies end by the abort of
    // their grace; idle ones leave their loop on the `Kill` and finish by
    // themselves. Either way the old worker value is gone before the
    // successor's workers are built.
    for busy_ms in [400, 0] {
        common::run(4, async move {
            let sub = SubSetup {
                anon: 3,
                busy_ms,
                w_grace: Some(Duration::from_secs(2)),
                ..SubSetup::default()
            };
            let world = start(one_restart(), setup(Shutdown::default(), sub)).await;
            assert_eq!(world.expected_workers, 3);
            // A long drop: a successor built while any old value is still
            // being dropped shows as a fourth live worker, whatever the
            // scheduling delays.
            world.probe.drop_ms.store(300, Ordering::SeqCst);
            world.sys.kill(&world.p_name).await.unwrap();
            world.wait_started(2).await;
            sleep(QUIET).await;
            assert_eq!(
                world.probe.w_max.load(Ordering::SeqCst),
                3,
                "the live workers never exceed the configured count: {:?}",
                world.probe.lines()
            );
            assert_eq!(world.probe.w_built.load(Ordering::SeqCst), 6);
            world.assert_no_budget_exhaustion();
        });
    }
}

// ---------------------------------------------------------------------------
// The bound, and what happens when it passes
// ---------------------------------------------------------------------------

/// The worker's turn blocks its thread until the test ends it: the
/// restart's wait expires at its bound (the worker's kill grace plus the
/// settle time, plus the slack), and the grandparent stops with
/// `ChildUnresponsive` while the turn still runs.
async fn expiry_scenario(w_shutdown: Option<Shutdown>) {
    let sub = named_worker(SubSetup {
        block_ms: LONG_BLOCK_MS,
        w_shutdown,
        ..SubSetup::default()
    });
    let world = start(one_restart(), setup(Shutdown::default(), sub)).await;
    let unblocks = Unblocks(world.probe.clone());
    let started = Instant::now();
    world.sys.kill(&world.p_name).await.unwrap();
    within(BOUND, "the grandparent to stop", world.top.wait_stopped()).await;
    let elapsed = started.elapsed();
    assert!(
        world.probe.at("w_turn_ended").is_none(),
        "the verdict came while the turn ran: {:?}",
        world.probe.lines()
    );
    assert_at_the_bound(elapsed, "the successor wait");
    let reason = world.probe.top_stopped.lock().unwrap().clone().unwrap();
    assert!(
        reason.contains("ChildUnresponsive") && reason.contains(&world.p_name),
        "{reason}"
    );
    assert_eq!(world.probe.built_p(), 1, "no new incarnation was built");
    assert_eq!(world.probe.reports_of(&world.p_name).len(), 1);
    drop(unblocks);
}

#[test]
fn successor_wait_escalates_when_an_old_grandchild_never_yields() {
    // A turn that blocks its thread needs real parallelism.
    common::run_multi_thread_only(4, async { expiry_scenario(None).await });
}

#[test]
fn successor_wait_for_an_infinity_worker_is_bounded_by_its_kill_grace() {
    // A turn that blocks its thread freezes a current-thread runtime's timer,
    // so the bound could never pass there; it needs real parallelism.
    common::run_multi_thread_only(4, async { expiry_scenario(Some(Shutdown::Infinity)).await });
}

#[test]
fn successor_wait_covers_a_grandchild_s_long_kill_grace() {
    common::run(4, async {
        let sub = named_worker(SubSetup {
            busy_ms: 1000,
            w_grace: Some(Duration::from_millis(1500)),
            ..SubSetup::default()
        });
        let world = start(one_restart(), setup(Shutdown::default(), sub)).await;
        world.sys.kill(&world.p_name).await.unwrap();
        world.wait_started(2).await;
        sleep(QUIET).await;
        assert!(world.top_running(), "{:?}", world.probe.lines());
        let reason = world.probe.top_stopped.lock().unwrap().clone();
        assert!(
            reason.is_none(),
            "the grandparent never stopped: {reason:?}"
        );
        world.assert_no_collision();
        world.assert_no_budget_exhaustion();
        world.probe.assert_before("w_dropped:1", "p_built:2");
    });
}

#[test]
fn failed_attempt_subtree_is_waited_for_by_the_next_attempt() {
    common::run(4, async {
        // The second incarnation spawns the named worker (busy for a long
        // turn, long kill grace), then hangs past its start timeout. The
        // retry waits for that worker, though the incarnation never started.
        let sub = named_worker(SubSetup {
            busy_ms: 900,
            busy_all: true,
            w_grace: Some(Duration::from_millis(500)),
            hang_on_build: Some(2),
            ..SubSetup::default()
        });
        let mut top_setup = setup(Shutdown::default(), sub);
        top_setup.p_start_timeout = Some(Duration::from_millis(300));
        let world = start(
            SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(60)),
            top_setup,
        )
        .await;
        world.sys.kill(&world.p_name).await.unwrap();
        // The third incarnation is the first to finish starting after the
        // kill: the second one hangs and fails at its start timeout.
        world.wait_started(3).await;
        sleep(QUIET).await;
        assert!(world.top_running(), "{:?}", world.probe.lines());
        world.assert_no_collision();
        world.probe.assert_before("w_dropped:2", "p_built:3");
    });
}

#[test]
fn parent_stop_cancels_a_restart_attempt_that_waits_for_its_predecessor_subtree() {
    common::run(4, async {
        // The child's worker has a 5 s kill grace and sits in a turn the test
        // ends. The child is killed, so its restart attempt waits for that
        // worker. A graceful stop of the root cancels the attempt: the root
        // reaches `Stopped` while the worker's turn still runs, and the
        // factory ran once only. The child's own window is long, so the
        // root's teardown waits for the attempt's cancel and does not cut it
        // short.
        let sub = named_worker(SubSetup {
            held: true,
            w_grace: Some(Duration::from_secs(5)),
            ..SubSetup::default()
        });
        let world = start(one_restart(), setup(Shutdown::default(), sub)).await;
        // The kill finds the worker inside its held turn.
        let probe = world.probe.clone();
        wait_until(BOUND, "the worker's held turn", move || {
            probe.at("w_turn_started:1").is_some()
        })
        .await;
        world.sys.kill(&world.p_name).await.unwrap();
        world.wait_line("the kill to be reported", "report:").await;
        within(
            BOUND,
            "the root to stop",
            world.top.stop(StopReason::Graceful),
        )
        .await
        .unwrap();
        within(
            BOUND,
            "the root to publish Stopped",
            world.top.wait_stopped(),
        )
        .await;
        // Neither the worker's turn nor the worker itself has ended.
        assert!(
            world.probe.at("w_turn_ended").is_none() && world.probe.at("w_dropped").is_none(),
            "{:?}",
            world.probe.lines()
        );
        world.probe.release.notify_one();
        assert_eq!(world.probe.built_p(), 1, "{:?}", world.probe.lines());
        world.assert_no_collision();
        let reason = world.probe.top_stopped.lock().unwrap().clone().unwrap();
        assert!(!reason.contains("ChildUnresponsive"), "{reason}");
    });
}

#[test]
fn an_expired_successor_wait_delivers_the_pending_deaths_before_the_supervisor_stops() {
    // The worker's turn blocks its thread, which needs real parallelism.
    common::run_multi_thread_only(4, async {
        let sub = named_worker(SubSetup {
            block_ms: 3000,
            ..SubSetup::default()
        });
        let mut top_setup = setup(Shutdown::default(), sub);
        top_setup.q_name = Some(uname("q"));
        let world = start(one_restart(), top_setup).await;
        // The child is killed and its restart attempt starts waiting for the
        // blocked worker.
        world.sys.kill(&world.p_name).await.unwrap();
        world.wait_line("the kill to be reported", "report:").await;
        // One handler holds the root's loop past the attempt's bound while
        // the sibling is killed: when it returns, the restart plane's expired
        // outcome is taken before the sibling's death, which is still pending.
        world.send(TopMsg::KillQThenHold(800)).await;
        within(BOUND, "the root to stop", world.top.wait_stopped()).await;
        let q_name = world.q_name.clone().unwrap();
        let reports = world.probe.reports.lock().unwrap().clone();
        let of_q: Vec<_> = reports
            .iter()
            .filter(|(name, ..)| *name == q_name)
            .collect();
        assert_eq!(of_q.len(), 1, "{reports:?}");
        assert_eq!(of_q[0].2, SupervisionAction::SupervisorStopping);
        assert_eq!(of_q[0].3, ActorStatus::Stopping);
        assert_eq!(
            world.probe.q_built.load(Ordering::SeqCst),
            1,
            "the sibling is not restarted"
        );
        let reason = world.probe.top_stopped.lock().unwrap().clone().unwrap();
        assert!(
            reason.contains("ChildUnresponsive") && reason.contains(&world.p_name),
            "{reason}"
        );
        assert_eq!(world.probe.built_p(), 1);
        // The budget allows one restart, which the child's kill used: a second
        // charge for the sibling would have exhausted it.
        world.assert_no_budget_exhaustion();
    });
}

#[test]
fn a_restarted_supervisor_child_killed_again_waits_for_its_new_subtree() {
    // The second incarnation is the one a restart started and the supervisor
    // adopted: killing it again must also wait for its own workers, which the
    // ledger learns of only when it adopts the restart.
    common::run(4, async {
        let sub = named_worker(SubSetup {
            busy_ms: 300,
            busy_all: true,
            ..SubSetup::default()
        });
        let world = start(
            SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(60)),
            setup(Shutdown::default(), sub),
        )
        .await;
        world.sys.kill(&world.p_name).await.unwrap();
        world.wait_started(2).await;
        // Wait until the second incarnation's worker is inside its turn.
        let probe = world.probe.clone();
        wait_until(BOUND, "the second worker's turn", move || {
            probe.at("w_turn_started:2").is_some()
        })
        .await;
        world.sys.kill(&world.p_name).await.unwrap();
        world.wait_started(3).await;
        sleep(QUIET).await;
        assert!(world.top_running(), "{:?}", world.probe.lines());
        world.assert_no_collision();
        world.probe.assert_before("w_dropped:1", "p_built:2");
        world.probe.assert_before("w_dropped:2", "p_built:3");
    });
}

// ---------------------------------------------------------------------------
// terminate_child, delete_child and a same-id spawn_child
// ---------------------------------------------------------------------------

#[test]
fn terminate_delete_and_respawn_of_a_supervisor_child_never_meets_its_old_workers() {
    common::run(4, async {
        // Policy `Kill`, a named worker inside a 300 ms turn: the terminate
        // returns only after the worker ended, so the delete and the respawn
        // of the same handler find the name free and the budget untouched.
        let sub = named_worker(SubSetup {
            busy_ms: 300,
            ..SubSetup::default()
        });
        let world = start(one_restart(), setup(Shutdown::Kill, sub)).await;
        world.send(TopMsg::Cycle).await;
        let line = world.wait_line("the cycle to finish", "cycle ").await;
        assert!(
            line.contains("terminate:Ok(())")
                && line.contains("live_after_terminate=0")
                && line.contains("delete:Ok(())")
                && line.contains("spawn:Ok(())"),
            "{line}"
        );
        world.wait_started(2).await;
        sleep(QUIET).await;
        world.assert_no_collision();
        world.assert_no_budget_exhaustion();
        assert_eq!(
            world.probe.reports_of(&world.p_name),
            vec![SupervisionAction::Removed]
        );
    });
}

/// Five ways the child's spec disappears while its subtree still runs, and a
/// spawn under its id after that.
mod same_id_spawn_child_never_starts_beside_a_retired_subtree {
    use super::*;

    /// The child's worker: a long turn and a long kill grace, so it outlives
    /// the child's own end by a clear margin.
    fn long_lived_worker() -> SubSetup {
        named_worker(SubSetup {
            busy_ms: 800,
            w_grace: Some(Duration::from_millis(1500)),
            ..SubSetup::default()
        })
    }

    #[test]
    fn abandoned_terminate() {
        common::run(4, async {
            let mut sub = long_lived_worker();
            sub.veto_stop = true;
            let world = start(
                one_restart(),
                setup(Shutdown::Timeout(Duration::from_millis(200)), sub),
            )
            .await;
            world.send(TopMsg::TerminateAbandoned).await;
            // The supervisor handles the child's end after its window.
            world.wait_line("the end to be reported", "report:").await;
            world.send(TopMsg::DeleteSpawn).await;
            let line = world.wait_line("the respawn to return", "delete:").await;
            assert!(
                line.contains("delete:Ok(())") && line.contains("spawn:Ok(())"),
                "{line}"
            );
            world.wait_started(2).await;
            sleep(QUIET).await;
            world.assert_no_collision();
            world.assert_no_budget_exhaustion();
            world.probe.assert_before("w_dropped:1", "p_built:2");
        });
    }

    #[test]
    fn expired_terminate() {
        // The worker blocks its thread until the test ends its turn.
        common::run_multi_thread_only(4, async {
            let sub = named_worker(SubSetup {
                block_ms: LONG_BLOCK_MS,
                ..SubSetup::default()
            });
            let world = start(one_restart(), setup(Shutdown::Kill, sub)).await;
            let unblocks = Unblocks(world.probe.clone());
            world.send(TopMsg::TerminateP).await;
            let line = world
                .wait_line("the terminate to return", "terminate:")
                .await;
            assert!(
                line.contains("ChildUnresponsive") && line.contains("alive=false"),
                "{line}"
            );
            assert_at_the_bound(recorded(&line), "the terminate wait");
            world.send(TopMsg::DeleteSpawn).await;
            let line = world.wait_line("the spawn to return", "delete:").await;
            assert!(
                line.contains("delete:Ok(())") && line.contains("spawn:Err(ChildUnresponsive"),
                "{line}"
            );
            assert_at_the_bound(recorded(&line), "the spawn wait");
            assert!(
                world.probe.at("w_turn_ended").is_none(),
                "both verdicts came while the turn ran: {:?}",
                world.probe.lines()
            );
            assert_eq!(world.probe.built_p(), 1, "no new child was constructed");
            // After the worker ended, a spawn succeeds without a collision.
            drop(unblocks);
            world.wait_no_workers().await;
            world.probe.note("workers_ended");
            world.send(TopMsg::DeleteSpawn).await;
            let probe = world.probe.clone();
            wait_until(BOUND, "the second spawn to return", move || {
                probe
                    .lines()
                    .iter()
                    .filter(|l| l.starts_with("delete:"))
                    .count()
                    == 2
            })
            .await;
            let second = world
                .probe
                .lines()
                .into_iter()
                .filter(|l| l.starts_with("delete:"))
                .nth(1)
                .unwrap();
            assert!(second.contains("spawn:Ok(())"), "{second}");
            world.wait_started(2).await;
            world.assert_no_collision();
        });
    }

    #[test]
    fn temporary_pruned() {
        common::run(4, async {
            let mut top_setup = setup(Shutdown::default(), long_lived_worker());
            top_setup.p_restart = RestartType::Temporary;
            top_setup.respawn_on = Some(top_setup.p_name.clone());
            let world = start(one_restart(), top_setup).await;
            world.sys.kill(&world.p_name).await.unwrap();
            let line = world
                .wait_line("the respawn in the report to return", "respawn_in_report")
                .await;
            assert!(line.contains("spawn:Ok(())"), "{line}");
            world.wait_started(2).await;
            sleep(QUIET).await;
            world.assert_no_collision();
            world.probe.assert_before("w_dropped:1", "p_built:2");
        });
    }

    #[test]
    fn pool_pruned() {
        common::run(4, async {
            let mut top_setup = setup(Shutdown::default(), long_lived_worker());
            top_setup.p_restart = RestartType::Temporary;
            top_setup.respawn_on = Some(top_setup.p_name.clone());
            let world = start(
                SupervisionConfig::simple_one_for_one().max_restarts(1, Duration::from_secs(60)),
                top_setup,
            )
            .await;
            world.sys.kill(&world.p_name).await.unwrap();
            let line = world
                .wait_line("the respawn in the report to return", "respawn_in_report")
                .await;
            assert!(line.contains("spawn:Ok(())"), "{line}");
            world.wait_started(2).await;
            sleep(QUIET).await;
            world.assert_no_collision();
            world.probe.assert_before("w_dropped:1", "p_built:2");
        });
    }

    #[test]
    fn pool_bounced() {
        common::run(4, async {
            let world = start(
                SupervisionConfig::simple_one_for_one().max_restarts(1, Duration::from_secs(60)),
                setup(Shutdown::Kill, long_lived_worker()),
            )
            .await;
            world.send(TopMsg::StopSpawn).await;
            let line = world.wait_line("the spawn to return", "stop_spawn").await;
            assert!(
                line.contains("stop:Ok(Stopped)") && line.contains("spawn:Ok(())"),
                "{line}"
            );
            world.wait_started(2).await;
            sleep(QUIET).await;
            world.assert_no_collision();
            world.assert_no_budget_exhaustion();
            world.probe.assert_before("w_dropped:1", "p_built:2");
        });
    }
}

#[test]
fn terminate_child_of_a_temporary_child_waits_for_its_old_subtree() {
    common::run(4, async {
        // A `Temporary` child's spec is removed by the terminate itself, so
        // the wait reads the retired record of its subtree.
        let sub = named_worker(SubSetup {
            busy_ms: 800,
            w_grace: Some(Duration::from_millis(1500)),
            ..SubSetup::default()
        });
        let mut top_setup = setup(Shutdown::Kill, sub);
        top_setup.p_restart = RestartType::Temporary;
        let world = start(one_restart(), top_setup).await;
        world.send(TopMsg::TerminateP).await;
        let line = world
            .wait_line("the terminate to return", "terminate:")
            .await;
        assert!(
            line.contains("terminate:Ok(())") && line.contains("live=0"),
            "{line}"
        );
        world.probe.assert_before("w_dropped:1", "terminate:Ok(())");
        world.send(TopMsg::DeleteSpawn).await;
        let line = world.wait_line("the spawn to return", "delete:").await;
        assert!(line.contains("spawn:Ok(())"), "{line}");
        world.wait_started(2).await;
        world.assert_no_collision();
    });
}

/// A `terminate_child` of a child that is already down, whose old subtree
/// still runs.
mod terminate_child_of_a_child_already_down_waits_for_its_old_subtree {
    use super::*;

    fn down_child_setup(sub: SubSetup) -> TopSetup {
        // Transient, a window of 200 ms and a `pre_stop` that vetoes: the
        // `stop_child` ends at the window with a Kill.
        let mut top_setup = setup(Shutdown::Timeout(Duration::from_millis(200)), sub);
        top_setup.p_restart = RestartType::Transient;
        top_setup
    }

    #[test]
    fn waits_then_ok() {
        common::run(4, async {
            let sub = named_worker(SubSetup {
                busy_ms: 900,
                w_grace: Some(Duration::from_millis(1500)),
                veto_stop: true,
                ..SubSetup::default()
            });
            let world = start(one_restart(), down_child_setup(sub)).await;
            world.send(TopMsg::StopP).await;
            let line = world.wait_line("the stop to return", "stop_child:").await;
            assert!(line.contains("Ok(Stopped)"), "{line}");
            world.wait_line("the death to be reported", "report:").await;
            assert_eq!(
                world.probe.reports_of(&world.p_name),
                vec![SupervisionAction::Removed]
            );
            // The child reads down, its worker still runs.
            assert_eq!(world.probe.live(), 1, "{:?}", world.probe.lines());
            world.send(TopMsg::TerminateP).await;
            let line = world
                .wait_line("the terminate to return", "terminate:")
                .await;
            assert!(
                line.contains("terminate:Ok(())") && line.contains("live=0"),
                "{line}"
            );
            world.probe.assert_before("w_dropped:1", "terminate:Ok(())");
            world.send(TopMsg::Cycle).await;
            let line = world.wait_line("the cycle to finish", "cycle ").await;
            assert!(
                line.contains("delete:Ok(())") && line.contains("spawn:Ok(())"),
                "{line}"
            );
            world.wait_started(2).await;
            world.assert_no_collision();
        });
    }

    #[test]
    fn expires_with_the_child_still_down() {
        // The worker blocks its thread until the test ends its turn.
        common::run_multi_thread_only(4, async {
            let sub = named_worker(SubSetup {
                block_ms: LONG_BLOCK_MS,
                veto_stop: true,
                ..SubSetup::default()
            });
            let world = start(one_restart(), down_child_setup(sub)).await;
            let unblocks = Unblocks(world.probe.clone());
            world.send(TopMsg::StopP).await;
            world.wait_line("the stop to return", "stop_child:").await;
            world.wait_line("the death to be reported", "report:").await;
            world.send(TopMsg::TerminateP).await;
            let line = world
                .wait_line("the terminate to return", "terminate:")
                .await;
            assert!(
                line.contains("ChildUnresponsive") && line.contains("alive=false"),
                "{line}"
            );
            assert_at_the_bound(recorded(&line), "the terminate wait");
            assert!(
                world.probe.at("w_turn_ended").is_none(),
                "the verdict came while the turn ran: {:?}",
                world.probe.lines()
            );
            // After the worker ended, a third call is `Ok`.
            drop(unblocks);
            world.wait_no_workers().await;
            world.send(TopMsg::TerminateP).await;
            let probe = world.probe.clone();
            wait_until(BOUND, "the third call to return", move || {
                probe
                    .lines()
                    .iter()
                    .filter(|l| l.starts_with("terminate:"))
                    .count()
                    == 2
            })
            .await;
            let third = world
                .probe
                .lines()
                .into_iter()
                .filter(|l| l.starts_with("terminate:"))
                .nth(1)
                .unwrap();
            assert!(third.contains("terminate:Ok(())"), "{third}");
        });
    }
}

#[test]
fn spawn_child_on_a_retired_id_inside_a_final_delivery_is_refused_at_once() {
    common::run(4, async {
        // A `Temporary` child's removal retires its subtree (a worker in a
        // long turn). The root is then stopped while a sibling's death is
        // pending, so the sibling's report is a final delivery, and the root
        // spawns the retired id again from it: refused at once, with no wait
        // and no factory call.
        let sub = named_worker(SubSetup {
            busy_ms: 800,
            w_grace: Some(Duration::from_millis(1500)),
            ..SubSetup::default()
        });
        let mut top_setup = setup(Shutdown::Kill, sub);
        top_setup.p_restart = RestartType::Temporary;
        top_setup.q_name = Some(uname("q"));
        top_setup.respawn_on = top_setup.q_name.clone();
        let world = start(one_restart(), top_setup).await;
        world.sys.kill(&world.p_name).await.unwrap();
        world
            .wait_line("the removal to be reported", "report:")
            .await;
        let built = world.probe.built_p();
        // The root's handler holds the loop while the sibling is killed and
        // the stop is queued, until the test releases it: the loop then ends
        // and delivers the sibling's death as a final delivery.
        let release = ReleasesTheTop(world.probe.clone());
        world.send(TopMsg::KillQThenHoldUntilReleased).await;
        within(BOUND, "the sibling to stop", async {
            while world.probe.at("q_stopped").is_none() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        world.top.stop(StopReason::Graceful).await.unwrap();
        drop(release);
        within(BOUND, "the root to stop", world.top.wait_stopped()).await;
        let line = world
            .probe
            .starts_with("respawn_in_report")
            .unwrap_or_default();
        assert!(line.contains("spawn:Err(SupervisorStopping)"), "{line}");
        assert!(line.contains("built_delta=0"), "{line}");
        assert_eq!(world.probe.built_p(), built, "no factory call");
        let micros: u128 = line
            .split("micros=")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|n| n.parse().ok())
            .expect("a duration");
        assert!(
            micros < 100_000,
            "refused at once, not after the worker's turn: {line}"
        );
    });
}

/// Every wait for a previous incarnation's subtree that outlives its bound
/// gives that incarnation the Unresponsive verdict and logs it once, naming
/// the supervisor, the child and the incarnation; a second route reaching
/// the same incarnation's verdict logs nothing more.
#[cfg(feature = "tracing")]
mod each_subtree_verdict_logs_once {
    use super::*;

    /// The warnings naming the world's supervisor child (the child field is
    /// matched whole), after the loops that could still write one have
    /// settled.
    async fn warnings_of(world: &World, logged: &Mutex<Vec<String>>) -> Vec<String> {
        sleep(QUIET).await;
        let lines = logged.lock().unwrap().clone();
        let mine = warnings::naming(&lines, &world.p_name);
        for line in &mine {
            assert!(
                line.contains(&format!("supervisor={} ", world.top.id())),
                "the warning names the supervisor: {line}"
            );
            // P's first incarnation, whose subtree outlived the wait: P and
            // its worker draw the system's first two incarnations, in either
            // order, and any later incarnation (a restart attempt's) is drawn
            // after both.
            let incarnation: u64 = line
                .split(" incarnation=")
                .nth(1)
                .and_then(|rest| rest.split(' ').next())
                .and_then(|n| n.parse().ok())
                .expect("the warning names an incarnation");
            assert!(
                incarnation < 2,
                "the warning names P's first incarnation: {line}"
            );
            assert!(
                line.contains(
                    "child's subtree outlived the bound of the wait for it and is left behind unresponsive"
                ),
                "the warning is the subtree verdict's: {line}"
            );
        }
        mine
    }

    fn blocked_worker() -> SubSetup {
        named_worker(SubSetup {
            block_ms: 2500,
            ..SubSetup::default()
        })
    }

    #[test]
    fn a_restart_attempt_s_wait() {
        let logged = warnings::install();
        // A turn that blocks its thread needs real parallelism.
        common::run_multi_thread_only(4, async {
            let world = start(one_restart(), setup(Shutdown::default(), blocked_worker())).await;
            world.sys.kill(&world.p_name).await.unwrap();
            within(BOUND, "the grandparent to stop", world.top.wait_stopped()).await;
            let reason = world.probe.top_stopped.lock().unwrap().clone().unwrap();
            assert!(reason.contains("ChildUnresponsive"), "{reason}");
            let mine = warnings_of(&world, &logged).await;
            assert_eq!(mine.len(), 1, "{mine:?}");
        });
    }

    #[test]
    fn terminate_child_s_wait_then_spawn_child_s_wait_on_the_same_incarnation() {
        let logged = warnings::install();
        common::run_multi_thread_only(4, async {
            let world = start(one_restart(), setup(Shutdown::Kill, blocked_worker())).await;
            world.send(TopMsg::TerminateP).await;
            let line = world
                .wait_line("the terminate to return", "terminate:")
                .await;
            assert!(line.contains("ChildUnresponsive"), "{line}");
            let mine = warnings_of(&world, &logged).await;
            assert_eq!(mine.len(), 1, "terminate_child's verdict: {mine:?}");
            world.send(TopMsg::DeleteSpawn).await;
            let line = world.wait_line("the spawn to return", "delete:").await;
            assert!(line.contains("spawn:Err(ChildUnresponsive"), "{line}");
            let again = warnings_of(&world, &logged).await;
            assert_eq!(
                again, mine,
                "spawn_child reached the verdict the terminate already recorded"
            );
        });
    }

    #[test]
    fn spawn_child_s_wait() {
        let logged = warnings::install();
        common::run_multi_thread_only(4, async {
            let world = start(one_restart(), setup(Shutdown::Kill, blocked_worker())).await;
            // The caller gives up at once; the child's end is reported, and
            // its subtree, the blocked worker, still runs.
            world.send(TopMsg::TerminateAbandoned).await;
            world.wait_line("the end to be reported", "report:").await;
            assert!(
                warnings_of(&world, &logged).await.is_empty(),
                "no wait has expired yet"
            );
            world.send(TopMsg::DeleteSpawn).await;
            let line = world.wait_line("the spawn to return", "delete:").await;
            assert!(
                line.contains("delete:Ok(())") && line.contains("spawn:Err(ChildUnresponsive"),
                "{line}"
            );
            let mine = warnings_of(&world, &logged).await;
            assert_eq!(mine.len(), 1, "{mine:?}");
        });
    }
}
