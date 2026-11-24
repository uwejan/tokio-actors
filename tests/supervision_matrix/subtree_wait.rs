#![allow(dead_code)]
//! A restart, a `terminate_child` and a `spawn_child` under a reused id wait
//! until every actor the previous incarnation spawned has ended.
//!
//! The tree: a root `Top` supervises a supervisor child `Sub`, which spawns
//! its workers in `on_started`. A worker is named (a second one under the same
//! name is refused while the first lives) or anonymous (a count of live
//! instances shows an overlap). The first `Sub` incarnation puts its workers
//! inside a turn the test holds, so they are still alive when the `Sub` is
//! killed: an awaiting turn ends at the abort of the worker's kill grace or
//! when the test opens its gate, and a turn that blocks its thread ends only
//! when the test opens its gate.
//!
//! Every outcome a test awaits is a handshake on the probe, bounded only by a
//! hang detector, and the order of events is read from one timeline the
//! actors append to. A check that nothing more happened runs after a barrier:
//! the root is stopped gracefully and awaited, so a late event shows as an
//! extra report, an extra build or a reason other than `Graceful`.

use super::*;

use std::future::Future;
use std::sync::Condvar;
use std::task::Poll;

use tokio::sync::Notify;
use tokio_actors::{ActorStatus, SpawnError};

// ---------------------------------------------------------------------------
// Hang bounds and gates
// ---------------------------------------------------------------------------

/// Awaits `fut`; the bound only turns a real hang into a failure.
async fn bounded<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(Duration::from_secs(60), fut).await {
        Ok(value) => value,
        Err(_) => panic!("hung waiting for: {what}"),
    }
}

/// Polls `fut` once and drops it: a call abandoned right after its first
/// poll, which commits it.
async fn poll_once<F: Future>(fut: F) {
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(|cx| {
        let _ = fut.as_mut().poll(cx);
        Poll::Ready(())
    })
    .await;
}

/// A gate an awaiting task waits on until the test opens it. It stays open.
#[derive(Default)]
struct Gate {
    open: AtomicBool,
    opened: Notify,
}

impl Gate {
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.opened.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let opened = self.opened.notified();
            tokio::pin!(opened);
            opened.as_mut().enable();
            if self.open.load(Ordering::SeqCst) {
                return;
            }
            opened.await;
        }
    }
}

/// A gate a turn blocks its thread on until the test opens it. It stays
/// open; a thread never released ends its wait after a minute.
#[derive(Default)]
struct ThreadGate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl ThreadGate {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }

    fn block(&self) {
        let open = self.open.lock().unwrap();
        let _ = self
            .opened
            .wait_timeout_while(open, Duration::from_secs(60), |open| !*open)
            .unwrap();
    }
}

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
    /// Ends every held worker turn.
    release: Gate,
    /// Ends every worker turn that blocks its thread.
    unblock: ThreadGate,
    /// Ends the root's held handler or callback.
    top_gate: Gate,
    /// Errors of a worker spawn inside `Sub::on_started`.
    spawn_errs: Mutex<Vec<String>>,
    /// Events in the order they happened.
    timeline: Mutex<Vec<String>>,
    /// What the root's `on_child_stopped` was given: child, reason, action,
    /// and the root's status at that moment.
    reports: Mutex<Vec<(String, String, SupervisionAction, ActorStatus)>>,
    /// The reason the root stopped with.
    top_stopped: Mutex<Option<String>>,
    /// Woken after every change a test waits for.
    changed: Notify,
}

impl Probe {
    fn note(&self, line: impl Into<String>) {
        self.timeline.lock().unwrap().push(line.into());
        self.changed.notify_waiters();
    }

    /// Waits until `pred` holds. Every change a predicate reads wakes the
    /// waiter, which registers before it checks, so no change is missed.
    async fn wait_for(&self, what: &str, pred: impl Fn(&Probe) -> bool) {
        bounded(what, async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if pred(self) {
                    return;
                }
                changed.await;
            }
        })
        .await
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

    /// The timeline lines that start with `prefix`, in order.
    fn all(&self, prefix: &str) -> Vec<String> {
        self.timeline
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with(prefix))
            .cloned()
            .collect()
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
}

impl Worker {
    fn new(probe: Arc<Probe>) -> Self {
        let id = probe.w_built.fetch_add(1, Ordering::SeqCst) + 1;
        let live = probe.w_live.fetch_add(1, Ordering::SeqCst) + 1;
        probe.w_max.fetch_max(live, Ordering::SeqCst);
        probe.changed.notify_waiters();
        Self { probe, id }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // The value is counted live until its drop: a successor built before
        // this point shows as one live worker too many.
        self.probe.w_live.fetch_sub(1, Ordering::SeqCst);
        self.probe.note(format!("w_dropped:{}", self.id));
    }
}

#[derive(Clone)]
enum WMsg {
    /// A turn that awaits until the test opens the probe's `release`; an
    /// abort cuts it.
    Held,
    /// A turn that blocks its thread until the test opens the probe's
    /// `unblock`; no abort can cut it.
    Block,
}

impl Actor for Worker {
    type Message = WMsg;
    type Response = ();

    async fn handle(&mut self, msg: WMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.probe.note(format!("w_turn_started:{}", self.id));
        match msg {
            WMsg::Held => self.probe.release.wait().await,
            WMsg::Block => self.probe.unblock.block(),
        }
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
    /// The first incarnation's workers take a turn the test holds (an
    /// awaiting turn, or one that blocks the thread).
    held: bool,
    block: bool,
    /// Every incarnation's workers take that turn, not only the first's.
    busy_all: bool,
    /// `pre_stop` vetoes every request, or never returns.
    veto_stop: bool,
    hang_pre_stop: bool,
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
                if turn && self.setup.held {
                    let _ = handle.notify(WMsg::Held).await;
                }
                if turn && self.setup.block {
                    let _ = handle.notify(WMsg::Block).await;
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
            std::future::pending::<()>().await;
        }
        self.probe.note(format!("p_ready:{}", self.seq));
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.probe.note(format!("p_pre_stop:{}", self.seq));
        if self.setup.hang_pre_stop {
            std::future::pending::<()>().await;
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
    /// A `RestartInitiated` report of this child kills `q` from inside the
    /// callback, waits until it reads `Stopped`, then holds the callback
    /// until the test opens the probe's `top_gate`.
    hold_in_report_of: Option<String>,
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
    /// `terminate_child(P)`, logged before the call with the number of live
    /// workers, and after it with its answer, the number of live workers
    /// and P's entry.
    TerminateP,
    /// `terminate_child(P)` abandoned right after its first poll, which
    /// commits it.
    TerminateAbandoned,
    /// `delete_child(P)` and a same-id `spawn_child(P)`.
    DeleteSpawn,
    /// `terminate_child`, `delete_child` and a same-id spawn in one handler.
    Cycle,
    /// `stop_child(P)` and a same-id spawn in one handler.
    StopSpawn,
    /// Kills the plain child `q` from inside the handler, waits until it
    /// reads `Stopped`, then holds the handler until the test opens the
    /// probe's `top_gate`.
    KillQThenHoldUntilReleased,
}

/// Opens the probe's `top_gate` when dropped, so the root's held handler
/// ends however the test ends.
struct ReleasesTheTop(Arc<Probe>);

impl Drop for ReleasesTheTop {
    fn drop(&mut self) {
        self.0.top_gate.open();
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

    /// Kills `q`, waits until it reads `Stopped`, then holds until the test
    /// opens the probe's `top_gate`.
    async fn kill_q_then_hold(&self) {
        let sys = self.setup.sys.clone().expect("the system");
        let q_name = self.setup.q_name.clone().expect("a plain child q");
        let q = sys.get::<Plain>(&q_name).expect("q runs");
        sys.kill(&q_name).await.expect("kill q");
        bounded("q to read Stopped", q.wait_stopped()).await;
        self.probe.note("q_stopped");
        bounded("the test to release the root", self.probe.top_gate.wait()).await;
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
                self.probe.note("setup_done");
            }
            TopMsg::StopP => {
                let r = ctx.stop_child(p_name).await;
                self.probe.note(format!("stop_child:{r:?}"));
            }
            TopMsg::TerminateP => {
                self.probe
                    .note(format!("terminate_begin live={}", self.probe.live()));
                let r = ctx.terminate_child(p_name).await;
                self.probe.note(format!(
                    "terminate:{r:?} live={} {}",
                    self.probe.live(),
                    self.live_children(ctx)
                ));
            }
            TopMsg::TerminateAbandoned => {
                poll_once(ctx.terminate_child(p_name)).await;
                self.probe.note("terminate_abandoned");
            }
            TopMsg::DeleteSpawn => {
                let deleted = ctx.delete_child(p_name);
                let spawned = self.spawn_p(ctx).await;
                self.probe.note(format!(
                    "delete:{deleted:?} spawn:{spawned:?} live={}",
                    self.probe.live()
                ));
            }
            TopMsg::Cycle => {
                let terminated = ctx.terminate_child(p_name.clone()).await;
                let live = self.probe.live();
                let deleted = ctx.delete_child(p_name);
                let spawned = self.spawn_p(ctx).await;
                self.probe.note(format!(
                    "cycle terminate:{terminated:?} live_after_terminate={live} \
                     delete:{deleted:?} spawn:{spawned:?}"
                ));
            }
            TopMsg::StopSpawn => {
                let stopped = ctx.stop_child(p_name).await;
                let spawned = self.spawn_p(ctx).await;
                self.probe
                    .note(format!("stop_spawn stop:{stopped:?} spawn:{spawned:?}"));
            }
            TopMsg::KillQThenHoldUntilReleased => self.kill_q_then_hold().await,
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
            let spawned = self.spawn_p(ctx).await;
            self.probe.note(format!(
                "respawn_in_report spawn:{spawned:?} built_delta={} live={}",
                self.probe.built_p() - before,
                self.probe.live()
            ));
        }
        if self.setup.hold_in_report_of.as_deref() == Some(name.as_str())
            && ev.action == SupervisionAction::RestartInitiated
        {
            self.kill_q_then_hold().await;
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
    /// The incarnation number of P's first incarnation.
    #[cfg(feature = "tracing")]
    #[cfg(feature = "telemetry")]
    p_first: u64,
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
        hold_in_report_of: None,
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

/// A worker kill grace no test outlives: the worker's held turn ends only
/// when the test opens its gate, never by the abort.
const HELD_GRACE: Duration = Duration::from_secs(60);

/// Opens the probe's `unblock` when dropped, so a blocked turn ends however
/// the test ends.
struct Unblocks(Arc<Probe>);

impl Drop for Unblocks {
    fn drop(&mut self) {
        self.0.unblock.open();
    }
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
    probe
        .wait_for("the first workers to be built", move |p| {
            p.w_built.load(Ordering::SeqCst) >= expected_workers
                && p.p_started.load(Ordering::SeqCst) >= 1
        })
        .await;
    #[cfg(feature = "tracing")]
    #[cfg(feature = "telemetry")]
    let p_first = sys
        .snapshot()
        .iter()
        .find(|row| row.actor.id().as_str() == p_name)
        .map(|row| row.actor.incarnation())
        .expect("P is registered");
    World {
        sys,
        top,
        probe,
        p_name,
        q_name,
        expected_workers,
        #[cfg(feature = "tracing")]
        #[cfg(feature = "telemetry")]
        p_first,
    }
}

impl World {
    async fn send(&self, msg: TopMsg) {
        self.top.notify(msg).await.unwrap();
    }

    async fn wait_line(&self, what: &str, prefix: &'static str) -> String {
        self.probe
            .wait_for(what, move |p| p.starts_with(prefix).is_some())
            .await;
        self.probe.starts_with(prefix).unwrap()
    }

    /// Waits until `n` lines start with `prefix` and returns the last.
    async fn wait_nth_line(&self, what: &str, prefix: &'static str, n: usize) -> String {
        self.probe
            .wait_for(what, move |p| p.all(prefix).len() >= n)
            .await;
        self.probe.all(prefix).swap_remove(n - 1)
    }

    /// Waits until `n` workers are inside their first turn.
    async fn wait_turns(&self, n: usize) {
        self.probe
            .wait_for("the workers' turns", move |p| {
                p.all("w_turn_started:").len() >= n
            })
            .await;
    }

    async fn wait_started(&self, n: usize) {
        self.probe
            .wait_for("the supervisor child to start again", move |p| {
                p.p_started.load(Ordering::SeqCst) >= n
            })
            .await;
    }

    async fn wait_no_workers(&self) {
        self.probe
            .wait_for("every old worker to end", |p| p.live() == 0)
            .await;
    }

    /// Opens every held turn, stops the root gracefully and waits until it
    /// stopped: an event still pending at the root shows afterwards as a
    /// final report, and a root that stopped on its own as another reason.
    async fn barrier(&self) {
        self.probe.release.open();
        self.probe.unblock.open();
        let _ = self.top.stop(StopReason::Graceful).await;
        bounded("the root to stop", self.top.wait_stopped()).await;
        let reason = self.probe.top_stopped.lock().unwrap().clone();
        assert_eq!(
            reason.as_deref(),
            Some("Graceful"),
            "the root ran until the barrier: {:?}",
            self.probe.lines()
        );
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

    /// No report reached the root's final deliveries: nothing was pending
    /// at the barrier.
    fn assert_nothing_pending_at_the_barrier(&self) {
        assert!(
            self.probe
                .reports
                .lock()
                .unwrap()
                .iter()
                .all(|(_, _, action, _)| *action != SupervisionAction::SupervisorStopping),
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
    /// The same, with a window the child's `pre_stop`, which never returns,
    /// outlives, so the group's own ladder escalates to `Kill`.
    GroupTimeoutEscalated,
    /// The escalated path with a worker under `Kill` and a long grace, so it
    /// outlives the child by the whole grace: only a budget derived from
    /// that grace covers it.
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
        held: true,
        w_shutdown: long_grace.then_some(Shutdown::Kill),
        w_grace: long_grace.then_some(Duration::from_millis(500)),
        hang_pre_stop: escalated,
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
    // The worker is inside its first turn, which only the abort of its
    // grace ends.
    world.wait_turns(1).await;
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
    assert!(world.top_running(), "{path:?}: {:?}", world.probe.lines());
    world.barrier().await;
    world.assert_no_collision();
    world.assert_no_budget_exhaustion();
    world.assert_nothing_pending_at_the_barrier();
    assert_eq!(
        world.probe.p_started.load(Ordering::SeqCst),
        2,
        "{path:?}: {:?}",
        world.probe.lines()
    );
    assert_eq!(
        world.probe.built_p(),
        2,
        "{path:?}: {:?}",
        world.probe.lines()
    );
    world.probe.assert_before("w_dropped:1", "p_built:2");
}

// ---------------------------------------------------------------------------
// The bound, and what happens when it passes
// ---------------------------------------------------------------------------

/// The worker's turn blocks its thread until the test ends it: the
/// restart's wait expires at its bound, and the grandparent stops with
/// `ChildUnresponsive` while the turn still runs. The bound's value is the
/// rule table's; this scenario pins that the verdict arrives and that no
/// new incarnation is built beside the live worker.
async fn expiry_scenario(w_shutdown: Option<Shutdown>) {
    let sub = named_worker(SubSetup {
        block: true,
        w_shutdown,
        ..SubSetup::default()
    });
    let world = start(one_restart(), setup(Shutdown::default(), sub)).await;
    let unblocks = Unblocks(world.probe.clone());
    world.wait_turns(1).await;
    world.sys.kill(&world.p_name).await.unwrap();
    bounded("the grandparent to stop", world.top.wait_stopped()).await;
    assert!(
        world.probe.at("w_turn_ended").is_none(),
        "the verdict came while the turn ran: {:?}",
        world.probe.lines()
    );
    let reason = world.probe.top_stopped.lock().unwrap().clone().unwrap();
    assert!(
        reason.contains("ChildUnresponsive") && reason.contains(&world.p_name),
        "{reason}"
    );
    assert_eq!(world.probe.built_p(), 1, "no new incarnation was built");
    assert_eq!(world.probe.reports_of(&world.p_name).len(), 1);
    drop(unblocks);
}

// ---------------------------------------------------------------------------
// terminate_child, delete_child and a same-id spawn_child
// ---------------------------------------------------------------------------

/// Five ways the child's spec disappears while its subtree still runs, and a
/// spawn under its id after that.
mod same_id_spawn_child_never_starts_beside_a_retired_subtree {
    use super::*;

    /// The child's worker: a held turn and a long kill grace, so it outlives
    /// the child's own end by a clear margin.
    fn long_lived_worker() -> SubSetup {
        named_worker(SubSetup {
            held: true,
            w_grace: Some(Duration::from_millis(1500)),
            ..SubSetup::default()
        })
    }
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
}

/// Every wait for a previous incarnation's subtree that outlives its bound
/// gives that incarnation the Unresponsive verdict and logs it once, naming
/// the supervisor, the child and the incarnation; a second route reaching
/// the same incarnation's verdict logs nothing more.
#[cfg(feature = "tracing")]
#[cfg(feature = "telemetry")]
mod each_subtree_verdict_logs_once {
    use super::*;

    /// The warnings naming the world's supervisor child (the child field is
    /// matched whole). A verdict's warning is written before the wait that
    /// reached it returns, so a caller reads it after that return.
    fn warnings_of(world: &World, logged: &Mutex<Vec<String>>) -> Vec<String> {
        let lines = logged.lock().unwrap().clone();
        let mine = warnings::naming(&lines, &world.p_name);
        for line in &mine {
            assert!(
                line.contains(&format!("supervisor={} ", world.top.id())),
                "the warning names the supervisor: {line}"
            );
            // P's first incarnation, whose subtree outlived the wait; any
            // later incarnation (a restart attempt's) has a higher number.
            let incarnation: u64 = line
                .split(" incarnation=")
                .nth(1)
                .and_then(|rest| rest.split(' ').next())
                .and_then(|n| n.parse().ok())
                .expect("the warning names an incarnation");
            assert_eq!(
                incarnation, world.p_first,
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

    /// Stops the root gracefully, the worker still blocked, and waits until
    /// it stopped: no loop of the tree is left to write a later warning
    /// about the incarnation.
    async fn stop_the_root(world: &World) {
        let _ = world.top.stop(StopReason::Graceful).await;
        bounded("the root to stop", world.top.wait_stopped()).await;
    }

    fn blocked_worker() -> SubSetup {
        named_worker(SubSetup {
            block: true,
            ..SubSetup::default()
        })
    }
}
