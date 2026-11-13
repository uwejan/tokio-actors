#![allow(dead_code)]
//! The non-blocking manual stops: `begin_stop_child` and `begin_terminate_child`
//! commit what `stop_child` and `terminate_child` commit and return at once.
//!
//! Every scenario runs through a command-driven supervisor whose callbacks
//! carry the call's answer back as text. A gated child ends only when the
//! test opens its gate, and the order of events is read from handshakes (the
//! child's own `on_stopped` entry, the supervisor's reports), never from
//! sleeps. The only waits on the clock are the negative windows of a
//! scenario that must still be blocked, and the quiet window that ends a
//! comparison.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorId, ActorResult, ChildEvent, RestartType, Shutdown,
    StopChildOutcome, StopReason, SupervisionConfig, SupervisionError,
};

mod common;

const BOUND: Duration = Duration::from_secs(20);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn tag() -> String {
    format!("-soft{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

async fn within<F: std::future::Future>(what: &str, fut: F) -> F::Output {
    match timeout(BOUND, fut).await {
        Ok(out) => out,
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

// ---------------------------------------------------------------------------
// The probe the actors share
// ---------------------------------------------------------------------------

struct Probe {
    /// What `on_child_stopped` received: `name|reason|action`.
    events: Mutex<Vec<String>>,
    /// Lines the supervisor and the children append, in order.
    log: Mutex<Vec<String>>,
    /// One permit for each gated `on_stopped` that started.
    entered: Semaphore,
    /// A gated `on_stopped` returns when it takes a permit.
    gate: Semaphore,
    /// Ends every blocked turn.
    unblock: AtomicBool,
}

impl Probe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            entered: Semaphore::new(0),
            gate: Semaphore::new(0),
            unblock: AtomicBool::new(false),
        })
    }

    fn note(&self, line: impl Into<String>) {
        self.log.lock().unwrap().push(line.into());
    }

    fn lines(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    /// Waits for one gated `on_stopped` to have started.
    async fn wait_entered(&self) {
        within("a gated on_stopped to start", self.entered.acquire())
            .await
            .expect("the entered semaphore stays open")
            .forget();
    }

    /// Lets `n` gated `on_stopped` calls return.
    fn open(&self, n: usize) {
        self.gate.add_permits(n);
    }
}

// ---------------------------------------------------------------------------
// The child
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct NodeCfg {
    /// `on_stopped` waits for the gate.
    gated: bool,
    /// `on_stopped` returns an error.
    fail_stop: bool,
    /// `on_started` spawns a leaf whose turn blocks its thread.
    grandchild: bool,
}

#[derive(Clone)]
enum Msg {
    Crash,
    /// Blocks the thread until the probe says so.
    Block,
}

struct Node {
    probe: Arc<Probe>,
    cfg: NodeCfg,
    label: String,
}

impl Node {
    fn new(probe: Arc<Probe>, cfg: NodeCfg, label: String) -> Self {
        Self { probe, cfg, label }
    }
}

impl Actor for Node {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Crash => Err(ActorError::user("crashed on command")),
            Msg::Block => {
                self.probe.note(format!("blocked:{}", self.label));
                let until = std::time::Instant::now() + Duration::from_secs(30);
                while !self.probe.unblock.load(Ordering::SeqCst)
                    && std::time::Instant::now() < until
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                self.probe.note(format!("leaf_unblocked:{}", self.label));
                Ok(())
            }
        }
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.cfg.grandchild {
            let probe = self.probe.clone();
            let leaf_label = format!("{}-leaf", self.label);
            let name = leaf_label.clone();
            let leaf = ctx
                .spawn_child(move || Node::new(probe.clone(), NodeCfg::default(), name.clone()))
                .named(leaf_label)
                .shutdown(Shutdown::Timeout(Duration::from_millis(50)))
                .kill_grace(Duration::from_secs(25))
                .await?;
            leaf.notify(Msg::Block)
                .await
                .map_err(|err| ActorError::user(err.to_string()))?;
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if self.cfg.gated {
            self.probe.entered.add_permits(1);
            self.probe
                .gate
                .acquire()
                .await
                .expect("the gate stays open")
                .forget();
        }
        if self.cfg.fail_stop {
            return Err(ActorError::user("stop failed"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The supervisor
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Stop,
    Terminate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// `begin_stop_child` / `begin_terminate_child`.
    Begin,
    /// `stop_child` / `terminate_child`, awaited.
    Awaited,
}

#[derive(Clone)]
struct Spec {
    label: String,
    restart: RestartType,
    cfg: NodeCfg,
    named: bool,
    supervisor: bool,
    shutdown: Shutdown,
    kill_grace: Option<Duration>,
}

impl Spec {
    fn new(label: &str, restart: RestartType) -> Self {
        Self {
            label: label.to_string(),
            restart,
            cfg: NodeCfg::default(),
            named: true,
            supervisor: false,
            shutdown: Shutdown::Timeout(Duration::from_secs(5)),
            kill_grace: None,
        }
    }

    fn gated(mut self) -> Self {
        self.cfg.gated = true;
        self
    }
}

#[derive(Clone)]
enum Cmd {
    Spawn(Spec),
    /// The call under test, made in one handler.
    Do {
        label: String,
        op: Op,
        mode: Mode,
    },
    /// Ends the child inside the handler, then makes the call.
    AfterEnd {
        label: String,
        op: Op,
        mode: Mode,
    },
    /// The begun call, then `delete_child` of the same child, in one
    /// handler: nothing the supervisor could handle sits between them.
    BeginThenDelete {
        label: String,
        op: Op,
    },
    /// The call, then the same call again in the same handler.
    Twice {
        label: String,
        op: Op,
    },
    /// The awaited call, dropped right after its commit.
    Dropped {
        label: String,
        op: Op,
    },
    Delete(String),
    /// Starts the child's blocking turn.
    Block(String),
    /// A same-name spawn after a delete, timed against the timeline.
    Respawn(Spec),
    Crash(String),
    Roster,
    Barrier,
}

#[derive(Debug)]
enum Reply {
    Text(String),
    Roster(Vec<(String, bool, bool)>),
    Unit,
}

struct Boss {
    probe: Arc<Probe>,
    handles: HashMap<String, ActorHandle<Node>>,
}

#[derive(Debug)]
struct Deleted;

fn show<T: std::fmt::Debug, E: std::fmt::Display>(result: Result<T, E>) -> String {
    match result {
        Ok(value) => format!("{value:?}"),
        Err(err) => format!("{err}"),
    }
}

impl Boss {
    fn id(&self, label: &str) -> ActorId {
        self.handles
            .get(label)
            .unwrap_or_else(|| panic!("no child {label}"))
            .id()
            .clone()
    }

    async fn call(&self, ctx: &mut ActorContext<Self>, label: &str, op: Op, mode: Mode) -> String {
        let id = self.id(label);
        // Everything up to the commit runs in this poll, ahead of anything
        // the supervisor could handle next.
        self.probe.note("call_started");
        match (op, mode) {
            (Op::Stop, Mode::Begin) => show(ctx.begin_stop_child(id)),
            (Op::Stop, Mode::Awaited) => show(ctx.stop_child(id).await),
            (Op::Terminate, Mode::Begin) => show(
                ctx.begin_terminate_child(id)
                    .map(|()| StopChildOutcome::Stopped),
            ),
            (Op::Terminate, Mode::Awaited) => show(
                ctx.terminate_child(id)
                    .await
                    .map(|()| StopChildOutcome::Stopped),
            ),
        }
    }

    async fn spawn(&mut self, ctx: &mut ActorContext<Self>, spec: Spec) -> Result<(), String> {
        let probe = self.probe.clone();
        let cfg = spec.cfg.clone();
        let label = spec.label.clone();
        let mut builder = ctx
            .spawn_child(move || Node::new(probe.clone(), cfg.clone(), label.clone()))
            .restart_type(spec.restart)
            .shutdown(spec.shutdown);
        if spec.named {
            builder = builder.named(spec.label.clone());
        }
        if spec.supervisor {
            builder = builder.with_config(ActorConfig::default().supervisor());
        }
        if let Some(grace) = spec.kill_grace {
            builder = builder.kill_grace(grace);
        }
        match builder.await {
            Ok(handle) => {
                self.handles.insert(spec.label, handle);
                Ok(())
            }
            Err(err) => Err(format!("{err}")),
        }
    }
}

impl Actor for Boss {
    type Message = Cmd;
    type Response = Reply;

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        match msg {
            Cmd::Spawn(spec) => Ok(Reply::Text(format!("{:?}", self.spawn(ctx, spec).await))),
            Cmd::Do { label, op, mode } => Ok(Reply::Text(self.call(ctx, &label, op, mode).await)),
            Cmd::AfterEnd { label, op, mode } => {
                let handle = self.handles[&label].clone();
                handle.stop(StopReason::Graceful).await.expect("stop sent");
                within("the child to end", handle.wait_stopped()).await;
                // The runner has recorded the end: the ledger reads the child
                // as not alive while its death is still unhandled, which is
                // the state a call that classifies at once meets.
                let id = self.id(&label);
                within("the runner to record the end", async {
                    while ctx.children().iter().any(|c| c.id == id && c.is_alive) {
                        sleep(Duration::from_millis(1)).await;
                    }
                })
                .await;
                Ok(Reply::Text(self.call(ctx, &label, op, mode).await))
            }
            Cmd::BeginThenDelete { label, op } => {
                let first = self.call(ctx, &label, op, Mode::Begin).await;
                let id = self.id(&label);
                let second = show(ctx.delete_child(id).map(|()| Deleted));
                Ok(Reply::Text(format!("{first}/{second}")))
            }
            Cmd::Twice { label, op } => {
                let first = self.call(ctx, &label, op, Mode::Begin).await;
                let second = self.call(ctx, &label, op, Mode::Begin).await;
                Ok(Reply::Text(format!("{first}/{second}")))
            }
            Cmd::Dropped { label, op } => {
                let id = self.id(&label);
                let text = match op {
                    Op::Stop => {
                        let dropped = timeout(Duration::ZERO, ctx.stop_child(id)).await;
                        format!("dropped={}", dropped.is_err())
                    }
                    Op::Terminate => {
                        let dropped = timeout(Duration::ZERO, ctx.terminate_child(id)).await;
                        format!("dropped={}", dropped.is_err())
                    }
                };
                Ok(Reply::Text(text))
            }
            Cmd::Block(label) => {
                let handle = self.handles[&label].clone();
                handle.notify(Msg::Block).await.expect("block sent");
                Ok(Reply::Unit)
            }
            Cmd::Delete(label) => {
                let id = self.id(&label);
                Ok(Reply::Text(show(ctx.delete_child(id).map(|()| Deleted))))
            }
            Cmd::Respawn(spec) => {
                self.probe.note("respawn_started");
                let result = self.spawn(ctx, spec).await;
                self.probe.note(format!("respawned:{result:?}"));
                Ok(Reply::Text(format!("{result:?}")))
            }
            Cmd::Crash(label) => {
                let handle = self.handles[&label].clone();
                handle.notify(Msg::Crash).await.expect("crash sent");
                Ok(Reply::Unit)
            }
            Cmd::Roster => {
                let mut roster: Vec<_> = ctx
                    .children()
                    .into_iter()
                    .map(|c| {
                        (
                            c.name.unwrap_or_else(|| c.id.to_string()),
                            c.is_alive,
                            c.restart_pending,
                        )
                    })
                    .collect();
                roster.sort();
                Ok(Reply::Roster(roster))
            }
            Cmd::Barrier => Ok(Reply::Unit),
        }
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let name = event
            .child_name
            .clone()
            .unwrap_or_else(|| "anonymous".to_string());
        let line = format!("{name}|{:?}|{:?}", event.reason, event.action);
        self.probe.note(format!("event:{line}"));
        self.probe.events.lock().unwrap().push(line);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The world a scenario runs in
// ---------------------------------------------------------------------------

struct World {
    boss: ActorHandle<Boss>,
    probe: Arc<Probe>,
    tag: String,
}

impl Drop for World {
    fn drop(&mut self) {
        // Ends every blocked or gated turn however the test ends.
        self.probe.unblock.store(true, Ordering::SeqCst);
        self.probe.gate.add_permits(64);
    }
}

async fn world(config: SupervisionConfig) -> World {
    let probe = Probe::new();
    let tag = tag();
    let boss = Boss {
        probe: probe.clone(),
        handles: HashMap::new(),
    }
    .spawn()
    .named(format!("boss{tag}"))
    .with_supervision(config)
    .await
    .expect("the supervisor spawns");
    World { boss, probe, tag }
}

impl World {
    fn label(&self, base: &str) -> String {
        format!("{base}{}", self.tag)
    }

    async fn ask(&self, cmd: Cmd) -> Reply {
        within("the supervisor's answer", self.boss.send(cmd))
            .await
            .expect("the supervisor answers")
    }

    async fn text(&self, cmd: Cmd) -> String {
        match self.ask(cmd).await {
            Reply::Text(text) => text,
            other => panic!("expected text, got {other:?}"),
        }
    }

    async fn roster(&self) -> Vec<(String, bool, bool)> {
        match self.ask(Cmd::Roster).await {
            Reply::Roster(roster) => roster,
            other => panic!("expected a roster, got {other:?}"),
        }
    }

    /// How many manual calls the supervisor started.
    fn started(&self) -> usize {
        self.probe
            .lines()
            .iter()
            .filter(|l| *l == "call_started")
            .count()
    }

    async fn barrier(&self) {
        let _ = self.ask(Cmd::Barrier).await;
    }

    async fn spawn(&self, spec: Spec) {
        let text = self.text(Cmd::Spawn(spec)).await;
        assert_eq!(text, "Ok(())", "the spawn");
    }

    /// Waits until the roster and the reports stop changing for a quiet
    /// window, and returns them with the tag removed.
    async fn settled(&self) -> (Vec<(String, bool, bool)>, Vec<String>) {
        let deadline = Instant::now() + BOUND;
        let mut last = (self.roster().await, self.probe.events());
        let mut same = 0;
        while same < 8 {
            assert!(Instant::now() < deadline, "the supervisor never settled");
            sleep(Duration::from_millis(10)).await;
            let now = (self.roster().await, self.probe.events());
            if now == last {
                same += 1;
            } else {
                same = 0;
                last = now;
            }
        }
        let tag = self.tag.clone();
        let roster = last
            .0
            .into_iter()
            .map(|(name, alive, pending)| (name.replace(&tag, ""), alive, pending))
            .collect();
        let events = last.1.into_iter().map(|e| e.replace(&tag, "")).collect();
        (roster, events)
    }

    /// Waits for the report with this prefix.
    async fn wait_event(&self, prefix: &str) {
        let deadline = Instant::now() + BOUND;
        loop {
            if self
                .probe
                .events()
                .iter()
                .any(|e| e.replace(&self.tag, "").starts_with(prefix))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no report {prefix}: {:?}",
                self.probe.events()
            );
            sleep(Duration::from_millis(5)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// begin_commits_what_the_awaited_stop_commits
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum State {
    RunningPermanent,
    RunningTransient,
    RunningTemporary,
    /// The child's task ended inside the handler, before the call.
    EndedBeforeTheCall,
    /// A stop of the child is in progress (gated), begun earlier.
    StopInProgress,
    /// A permanent member of a pending group restart, its stop gated.
    GroupMember,
    /// Terminated earlier; the spec is kept and the child is down.
    DownKept,
    /// A child of a pool (`SimpleOneForOne`).
    PoolChild,
    /// A stop of the child ends with an error from `on_stopped`.
    StopMeetsACrash,
}

const STATES: [State; 9] = [
    State::RunningPermanent,
    State::RunningTransient,
    State::RunningTemporary,
    State::EndedBeforeTheCall,
    State::StopInProgress,
    State::GroupMember,
    State::DownKept,
    State::PoolChild,
    State::StopMeetsACrash,
];

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    answer: String,
    roster: Vec<(String, bool, bool)>,
    events: Vec<String>,
}

/// Runs the call under test in `mode` against a child in `state`, and returns
/// what the supervisor answered, its ledger and its reports once it settled.
async fn observe(state: State, op: Op, mode: Mode) -> Observation {
    let config = match state {
        State::GroupMember => SupervisionConfig::one_for_all(),
        State::PoolChild => SupervisionConfig::simple_one_for_one(),
        _ => SupervisionConfig::one_for_one(),
    };
    let w = world(config).await;
    let c = w.label("c");
    let mut gated_stops = 0;
    let mut after_end = false;
    match state {
        State::RunningPermanent => w.spawn(Spec::new(&c, RestartType::Permanent)).await,
        State::RunningTransient => w.spawn(Spec::new(&c, RestartType::Transient)).await,
        State::RunningTemporary => w.spawn(Spec::new(&c, RestartType::Temporary)).await,
        State::EndedBeforeTheCall => {
            w.spawn(Spec::new(&c, RestartType::Permanent)).await;
            after_end = true;
        }
        State::StopInProgress => {
            w.spawn(Spec::new(&c, RestartType::Permanent).gated()).await;
            let begun = w
                .text(Cmd::Do {
                    label: c.clone(),
                    op: Op::Stop,
                    mode: Mode::Begin,
                })
                .await;
            assert_eq!(begun, "Stopped", "the stop in progress");
            w.probe.wait_entered().await;
            gated_stops = 1;
        }
        State::GroupMember => {
            let b = w.label("b");
            w.spawn(Spec::new(&c, RestartType::Permanent).gated()).await;
            w.spawn(Spec::new(&b, RestartType::Permanent)).await;
            let _ = w.ask(Cmd::Crash(b)).await;
            w.probe.wait_entered().await;
            gated_stops = 1;
        }
        State::DownKept => {
            w.spawn(Spec::new(&c, RestartType::Transient)).await;
            let first = w
                .text(Cmd::Do {
                    label: c.clone(),
                    op: Op::Terminate,
                    mode: Mode::Awaited,
                })
                .await;
            assert_eq!(first, "Stopped", "the earlier terminate");
            w.settled().await;
        }
        State::PoolChild => {
            let mut spec = Spec::new(&c, RestartType::Permanent);
            spec.named = false;
            w.spawn(spec).await;
        }
        State::StopMeetsACrash => {
            let mut spec = Spec::new(&c, RestartType::Permanent);
            spec.cfg.fail_stop = true;
            w.spawn(spec).await;
        }
    }
    let command = if after_end {
        Cmd::AfterEnd {
            label: c.clone(),
            op,
            mode,
        }
    } else {
        Cmd::Do {
            label: c.clone(),
            op,
            mode,
        }
    };
    let started = w.started();
    let call = {
        let boss = w.boss.clone();
        tokio::spawn(async move { boss.send(command).await })
    };
    // A begun call returns before any gate opens; an awaited call on a gated
    // stop returns only after it. The gate opens once the call is issued: it
    // commits in the poll that notes its start.
    within("the call to start", async {
        while w.started() == started {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    if mode == Mode::Begin {
        within("a begun call to return", async {
            while !call.is_finished() {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
    }
    if gated_stops > 0 {
        w.probe.open(gated_stops);
    }
    let answer = match within("the call's answer", call)
        .await
        .expect("the call task ends")
        .expect("the supervisor answers")
    {
        Reply::Text(text) => text.replace(&w.tag, ""),
        other => panic!("expected text, got {other:?}"),
    };
    let (roster, events) = w.settled().await;
    Observation {
        answer,
        roster,
        events,
    }
}

#[test]
fn begin_commits_what_the_awaited_stop_commits() {
    common::run(4, async {
        for state in STATES {
            for op in [Op::Stop, Op::Terminate] {
                let begun = observe(state, op, Mode::Begin).await;
                let awaited = observe(state, op, Mode::Awaited).await;
                assert_eq!(begun, awaited, "{state:?} {op:?}");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// returns_before_the_child_ends
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// the_slot_stays_taken_until_the_subtree_ended
// ---------------------------------------------------------------------------

#[test]
fn the_slot_stays_taken_until_the_subtree_ended() {
    // The late actor is a leaf whose turn blocks its thread, so this scenario
    // needs real parallelism on both flavors' runs.
    common::run_multi_thread_only(4, async {
        let w = world(SupervisionConfig::one_for_one()).await;
        let sub_label = w.label("sub");
        let mut sub = Spec::new(&sub_label, RestartType::Transient);
        sub.cfg.grandchild = true;
        sub.supervisor = true;
        sub.shutdown = Shutdown::Timeout(Duration::from_millis(100));
        sub.kill_grace = Some(Duration::from_millis(100));
        w.spawn(sub.clone()).await;

        // The supervised child starts on its own: the spawn answers before
        // its start hook ran. The stop is ordered only once the leaf reports
        // that its blocking turn began, so the stop always meets a leaf that
        // abort cannot reach.
        let leaf_line = format!("blocked:{sub_label}-leaf");
        within("the leaf to begin its blocking turn", async {
            while !w.probe.lines().contains(&leaf_line) {
                sleep(Duration::from_millis(2)).await;
            }
        })
        .await;

        // Row 1: while the child's own stop is in progress the spec is kept,
        // so the id is taken both ways. The delete is made in the handler
        // that begins the stop, so it meets the stop in progress however long
        // the host takes to run the supervisor's next turn.
        let both = w
            .text(Cmd::BeginThenDelete {
                label: sub_label.clone(),
                op: Op::Terminate,
            })
            .await;
        let (begun, refused) = both.split_once('/').expect("two answers");
        assert_eq!(begun, "Stopped");
        assert!(
            refused.contains("restarting") || refused.contains("running"),
            "delete while the stop is in progress: {refused}"
        );

        // The child ends (its window expires and it is aborted) while its
        // leaf is still blocked in a turn that abort cannot reach.
        w.wait_event("sub|").await;
        assert!(
            !w.probe
                .lines()
                .iter()
                .any(|l| l.starts_with("leaf_unblocked")),
            "the leaf is still blocked: {:?}",
            w.probe.lines()
        );

        // Row 2: the child is down and its spec can go, but the id stays
        // taken: a same-id spawn waits for the leaf.
        let deleted = w.text(Cmd::Delete(sub_label.clone())).await;
        assert_eq!(deleted, "Deleted", "delete once the child ended");
        let respawn = {
            let boss = w.boss.clone();
            let spec = sub.clone();
            tokio::spawn(async move { boss.send(Cmd::Respawn(spec)).await })
        };
        within("the respawn to start", async {
            while !w.probe.lines().iter().any(|l| l == "respawn_started") {
                sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
        // The negative window: the spawn does not complete while the leaf runs.
        let mut respawn = respawn;
        let early = timeout(Duration::from_millis(300), &mut respawn).await;
        assert!(
            early.is_err(),
            "a same-id spawn went through while the old subtree ran: {:?}",
            w.probe.lines()
        );
        w.probe.note("test_unblocks_the_leaf");
        w.probe.unblock.store(true, Ordering::SeqCst);
        let result = within("the respawn to finish", respawn)
            .await
            .expect("the respawn task ends")
            .expect("the supervisor answers");
        match result {
            Reply::Text(text) => assert_eq!(text, "Ok(())", "the respawn"),
            other => panic!("expected text, got {other:?}"),
        }
        let lines = w.probe.lines();
        let at = |what: &str| lines.iter().position(|l| l.starts_with(what));
        assert!(
            at("test_unblocks_the_leaf") < at("respawned:"),
            "the respawn came first: {lines:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// the_end_is_reported_once
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// a_begun_terminate_of_a_dropped_future_equals_the_awaited_one
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// a_verdict_no_caller_waits_for_ends_the_supervisor
// ---------------------------------------------------------------------------

#[test]
fn a_verdict_no_caller_waits_for_ends_the_supervisor() {
    // The child's turn blocks its thread, so its whole stop ladder runs out
    // and it carries the Unresponsive verdict while it still runs. A stop no
    // caller waits for (begun, or awaited and dropped after the commit) ends
    // the supervisor with the verdict, as the group barrier does; an awaited
    // stop hands the verdict to its caller, who decides, and the supervisor
    // goes on. The turn needs a thread of its own on both flavors' runs.
    //
    // (op, restart type, mode, dropped after the commit, supervisor ends)
    let rows = [
        (
            Op::Terminate,
            RestartType::Transient,
            Mode::Begin,
            false,
            true,
        ),
        (Op::Stop, RestartType::Permanent, Mode::Begin, false, true),
        (
            Op::Terminate,
            RestartType::Transient,
            Mode::Awaited,
            true,
            true,
        ),
        (Op::Stop, RestartType::Permanent, Mode::Awaited, true, true),
        (
            Op::Terminate,
            RestartType::Transient,
            Mode::Awaited,
            false,
            false,
        ),
        (
            Op::Stop,
            RestartType::Permanent,
            Mode::Awaited,
            false,
            false,
        ),
    ];
    common::run_multi_thread_only(4, async move {
        for (op, restart, mode, dropped, ends) in rows {
            let row = format!("{op:?} {restart:?} {mode:?} dropped={dropped}");
            let w = world(SupervisionConfig::one_for_one()).await;
            let c = w.label("c");
            let mut spec = Spec::new(&c, restart);
            spec.shutdown = Shutdown::Timeout(Duration::from_millis(50));
            spec.kill_grace = Some(Duration::from_millis(100));
            w.spawn(spec).await;
            let _ = w.ask(Cmd::Block(c.clone())).await;
            let blocked = format!("blocked:{c}");
            within("the child's turn to start", async {
                while !w.probe.lines().contains(&blocked) {
                    sleep(Duration::from_millis(2)).await;
                }
            })
            .await;
            let answer = if dropped {
                w.text(Cmd::Dropped {
                    label: c.clone(),
                    op,
                })
                .await
            } else {
                w.text(Cmd::Do {
                    label: c.clone(),
                    op,
                    mode,
                })
                .await
            };
            if ends {
                let reason = within("the supervisor to end", w.boss.wait_stopped()).await;
                match reason {
                    StopReason::Failure(ActorError::Supervision(
                        SupervisionError::ChildUnresponsive(id),
                    )) => assert_eq!(id, ActorId::from(c.as_str()), "{row}"),
                    other => panic!("{row}: expected ChildUnresponsive, got {other:?}"),
                }
            } else {
                // The caller was told and decided: the answer is the error,
                // and the supervisor goes on beside the child that still runs.
                assert!(answer.contains("did not end in time"), "{row}: {answer}");
                w.barrier().await;
                assert!(
                    timeout(Duration::from_millis(500), w.boss.wait_stopped())
                        .await
                        .is_err(),
                    "{row}: the supervisor ended over a verdict its caller decided on"
                );
            }
        }
    });
}
