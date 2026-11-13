#![allow(dead_code)]
//! `SupervisorRef`: the supervisor's child management over its system channel.
//!
//! The supervisor in every scenario is a command-driven actor. Children are
//! gated by the probe's semaphores so a test decides when a stop or a start
//! may finish, and the order of events is read from handshakes. The negative
//! windows (a call that must still be waiting) use a short real timeout.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{oneshot, Semaphore};
use tokio::time::{sleep, timeout, Instant};

use tokio_actors::telemetry::{EventKind, HookKind, KindSet, TelemetryEvent, TurnOutcome};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorSystem, ChildEvent, ChildInfo,
    RestartType, Shutdown, SpawnError, StopReason, SupervisionConfig, SupervisionError,
    SupervisorRef, SupervisorRefError,
};

mod common;

const BOUND: Duration = Duration::from_secs(20);
/// How long a call that must still be waiting is watched.
const QUIET: Duration = Duration::from_millis(250);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn tag() -> String {
    format!("-sref{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

async fn within<F: std::future::Future>(what: &str, fut: F) -> F::Output {
    match timeout(BOUND, fut).await {
        Ok(out) => out,
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

// ---------------------------------------------------------------------------
// The probe
// ---------------------------------------------------------------------------

struct Probe {
    /// What `on_child_stopped` received: `name|reason|action`.
    events: Mutex<Vec<String>>,
    log: Mutex<Vec<String>>,
    /// One permit for each gated `on_stopped` that started.
    entered: Semaphore,
    /// A gated `on_stopped` returns when it takes a permit.
    gate: Semaphore,
    /// A held handler returns when it takes a permit.
    hold: Semaphore,
    /// A held restart's `pre_start` returns when it takes a permit.
    start_gate: Semaphore,
    /// One permit for each held `pre_start` that started.
    start_entered: Semaphore,
    /// Children built, in order.
    built: AtomicUsize,
    /// Task ids the user closures of `start_child` ran on.
    closure_tasks: Mutex<Vec<tokio::task::Id>>,
    unblock: AtomicBool,
}

impl Probe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            entered: Semaphore::new(0),
            gate: Semaphore::new(0),
            hold: Semaphore::new(0),
            start_gate: Semaphore::new(0),
            start_entered: Semaphore::new(0),
            built: AtomicUsize::new(0),
            closure_tasks: Mutex::new(Vec::new()),
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

    async fn wait_entered(&self) {
        within("a gated on_stopped to start", self.entered.acquire())
            .await
            .expect("open")
            .forget();
    }

    async fn wait_start_entered(&self) {
        within("a held pre_start to start", self.start_entered.acquire())
            .await
            .expect("open")
            .forget();
    }

    async fn wait_line(&self, line: &str) {
        let deadline = Instant::now() + BOUND;
        while !self.lines().iter().any(|l| l == line) {
            assert!(
                Instant::now() < deadline,
                "no line {line:?}: {:?}",
                self.lines()
            );
            sleep(Duration::from_millis(2)).await;
        }
    }

    async fn wait_event(&self, prefix: &str) {
        let deadline = Instant::now() + BOUND;
        while !self.events().iter().any(|e| e.starts_with(prefix)) {
            assert!(
                Instant::now() < deadline,
                "no report {prefix}: {:?}",
                self.events()
            );
            sleep(Duration::from_millis(2)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// The child
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct NodeCfg {
    /// `on_stopped` waits for the gate.
    gated: bool,
    /// `pre_start` of every incarnation after the first waits for the start
    /// gate.
    hold_restart: bool,
    /// `pre_start` of every incarnation waits for the start gate.
    hold_start: bool,
    /// `on_started` spawns a leaf whose turn blocks its thread.
    grandchild: bool,
    /// `on_stopped` blocks its thread until the probe says so.
    block_stop: bool,
}

#[derive(Clone)]
enum Msg {
    Crash,
}

struct Node {
    probe: Arc<Probe>,
    cfg: NodeCfg,
    label: String,
    index: usize,
}

impl Node {
    fn new(probe: Arc<Probe>, cfg: NodeCfg, label: String, index: usize) -> Self {
        probe.built.fetch_add(1, Ordering::SeqCst);
        Self {
            probe,
            cfg,
            label,
            index,
        }
    }
}

impl Actor for Node {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Crash => Err(ActorError::user("crashed on command")),
        }
    }

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        // The first incarnation starts at once unless it is held; a restart
        // waits on the gate when held.
        if self.cfg.hold_start || (self.cfg.hold_restart && self.index > 0) {
            self.probe.start_entered.add_permits(1);
            self.probe
                .start_gate
                .acquire()
                .await
                .expect("open")
                .forget();
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
            self.probe.gate.acquire().await.expect("open").forget();
        }
        if self.cfg.block_stop {
            self.probe.entered.add_permits(1);
            let until = std::time::Instant::now() + Duration::from_secs(30);
            while !self.probe.unblock.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }
}

/// A worker that does nothing.
struct Plain;

impl Actor for Plain {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The supervisor
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Cmd {
    /// A handler that runs until the test lets it go.
    Hold,
    /// Records the mark and spawns a child named after it.
    Mark(u32),
    /// Calls every method of a reference to this supervisor from the handler.
    SelfCall,
    /// The id of the task the handler runs on.
    TaskId,
    Nothing,
}

#[derive(Debug)]
enum Reply {
    Text(String),
    Task(tokio::task::Id),
    Unit,
}

struct Boss {
    probe: Arc<Probe>,
    /// Calls the reference from `on_child_stopped`.
    self_call_on_report: bool,
    tag: String,
}

impl Actor for Boss {
    type Message = Cmd;
    type Response = Reply;

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        match msg {
            Cmd::Hold => {
                self.probe.note("hold_entered");
                self.probe.hold.acquire().await.expect("open").forget();
                self.probe.note("hold_left");
                Ok(Reply::Unit)
            }
            Cmd::Mark(n) => {
                self.probe.note(format!("mark:{n}"));
                ctx.spawn_child(|| Plain)
                    .named(format!("mark{n}{}", self.tag))
                    .await?;
                Ok(Reply::Unit)
            }
            Cmd::SelfCall => {
                let sup = SupervisorRef::of(&ctx.self_handle());
                Ok(Reply::Text(all_methods(&sup).await))
            }
            Cmd::TaskId => Ok(Reply::Task(tokio::task::id())),
            Cmd::Nothing => Ok(Reply::Unit),
        }
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let name = event
            .child_name
            .clone()
            .unwrap_or_else(|| "anonymous".to_string());
        let line = format!("{name}|{:?}|{:?}", event.reason, event.action);
        self.probe.note(format!("event:{line}"));
        self.probe.events.lock().unwrap().push(line);
        if self.self_call_on_report {
            let sup = SupervisorRef::of(&ctx.self_handle());
            let text = all_methods(&sup).await;
            self.probe.note(format!("self_call_in_report:{text}"));
        }
        Ok(())
    }
}

/// Calls every method of `sup` and reports each error.
async fn all_methods(sup: &SupervisorRef<Boss>) -> String {
    let mut out = Vec::new();
    out.push(format!(
        "start:{:?}",
        sup.start_child(|| Plain, |c| c).await.err()
    ));
    out.push(format!(
        "terminate:{:?}",
        sup.terminate_child("x").await.err()
    ));
    out.push(format!("restart:{:?}", sup.restart_child("x").await.err()));
    out.push(format!("delete:{:?}", sup.delete_child("x").await.err()));
    out.push(format!("which:{:?}", sup.which_children().await.err()));
    out.push(format!("count:{:?}", sup.count_children().await.err()));
    out.join(" ")
}

struct World {
    boss: ActorHandle<Boss>,
    sup: SupervisorRef<Boss>,
    probe: Arc<Probe>,
    tag: String,
    system: Arc<ActorSystem>,
    /// The hook and outcome of every turn the supervisor finished, in order.
    turns: Arc<Mutex<Vec<(HookKind, TurnOutcome)>>>,
}

impl Drop for World {
    fn drop(&mut self) {
        // Ends every held or gated turn however the test ends.
        self.probe.unblock.store(true, Ordering::SeqCst);
        self.probe.gate.add_permits(64);
        self.probe.hold.add_permits(64);
        self.probe.start_gate.add_permits(64);
    }
}

async fn world_with(config: ActorConfig, self_call_on_report: bool) -> World {
    let probe = Probe::new();
    let tag = tag();
    let system = ActorSystem::create(format!("supref{tag}")).expect("a fresh system name");
    let turns: Arc<Mutex<Vec<(HookKind, TurnOutcome)>>> = Arc::default();
    let sink = turns.clone();
    assert!(system.attach_telemetry(
        "turns",
        KindSet::of(&[EventKind::TurnFinished]),
        move |event| {
            if let TelemetryEvent::TurnFinished { hook, outcome, .. } = event {
                sink.lock().unwrap().push((*hook, *outcome));
            }
        }
    ));
    let boss = Boss {
        probe: probe.clone(),
        self_call_on_report,
        tag: tag.clone(),
    }
    .spawn()
    .named(format!("boss{tag}"))
    .with_config(config)
    .on_system(&system)
    .await
    .expect("the supervisor spawns");
    let sup = SupervisorRef::of(&boss);
    World {
        boss,
        sup,
        probe,
        tag,
        system,
        turns,
    }
}

async fn world() -> World {
    world_with(
        ActorConfig::default().with_supervision(SupervisionConfig::one_for_one()),
        false,
    )
    .await
}

impl World {
    fn name(&self, base: &str) -> String {
        format!("{base}{}", self.tag)
    }

    /// Starts a node through the reference.
    async fn start_node(
        &self,
        base: &str,
        restart: RestartType,
        cfg: NodeCfg,
    ) -> ActorHandle<Node> {
        let probe = self.probe.clone();
        let name = self.name(base);
        let label = name.clone();
        let built = Arc::new(AtomicUsize::new(0));
        self.sup
            .start_child(
                move || {
                    let nth = built.fetch_add(1, Ordering::SeqCst);
                    Node::new(probe.clone(), cfg.clone(), label.clone(), nth)
                },
                move |child| child.named(name).restart_type(restart),
            )
            .await
            .expect("the child starts")
    }

    /// Waits until the supervisor has finished `count` turns of `hook` that
    /// ended as `outcome`.
    async fn turns_of(&self, hook: HookKind, outcome: TurnOutcome, count: usize) {
        within("the turn events", async {
            while self
                .turns
                .lock()
                .unwrap()
                .iter()
                .filter(|turn| **turn == (hook, outcome))
                .count()
                < count
            {
                sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
    }

    async fn ask(&self, cmd: Cmd) -> Reply {
        within("the supervisor's answer", self.boss.send(cmd))
            .await
            .expect("the supervisor answers")
    }

    /// Waits until the named child is alive.
    async fn wait_alive(&self, base: &str) {
        let deadline = Instant::now() + BOUND;
        loop {
            if self.info(base).await.is_some_and(|c| c.is_alive) {
                return;
            }
            assert!(Instant::now() < deadline, "the child never came back");
            sleep(Duration::from_millis(5)).await;
        }
    }

    async fn info(&self, base: &str) -> Option<ChildInfo> {
        let name = self.name(base);
        self.sup
            .which_children()
            .await
            .expect("which_children")
            .into_iter()
            .find(|c| c.name.as_deref() == Some(name.as_str()))
    }
}

// ---------------------------------------------------------------------------
// terminate_child_answers_when_the_child_ended
// ---------------------------------------------------------------------------

#[test]
fn terminate_child_answers_when_the_child_ended() {
    common::run(4, async {
        for restart in [
            RestartType::Permanent,
            RestartType::Transient,
            RestartType::Temporary,
        ] {
            let w = world().await;
            let cfg = NodeCfg {
                gated: true,
                ..NodeCfg::default()
            };
            let child = w.start_node("c", restart, cfg).await;
            let id = child.id().clone();
            let call = {
                let sup = w.sup.clone();
                tokio::spawn(async move { sup.terminate_child(id).await })
            };
            // The stop is committed and the child's `on_stopped` is held.
            w.probe.wait_entered().await;
            let mut call = call;
            let early = timeout(QUIET, &mut call).await;
            assert!(
                early.is_err(),
                "{restart:?}: the answer came before the child ended"
            );
            // The supervisor serves other work while the child ends.
            assert_eq!(w.sup.count_children().await.expect("count").specs, 1);
            w.probe.gate.add_permits(1);
            within("the answer", call)
                .await
                .expect("the call task ends")
                .expect("the stop is answered");
            // The classification is finished before the answer goes out: the
            // spec is in the state `terminate_child` leaves it in.
            let id = child.id().clone();
            match restart {
                RestartType::Temporary => {
                    let again = w.sup.delete_child(id.clone()).await;
                    assert_eq!(
                        again,
                        Err(SupervisorRefError::Supervision(
                            SupervisionError::ChildNotFound(id)
                        )),
                        "{restart:?}: a temporary child's spec went with its end"
                    );
                }
                _ => {
                    w.sup.delete_child(id.clone()).await.unwrap_or_else(|err| {
                        panic!("{restart:?}: delete after the answer: {err}")
                    });
                    assert_eq!(w.sup.count_children().await.expect("count").specs, 0);
                }
            }
            w.probe.wait_event("c").await;
            assert_eq!(
                w.probe.events().len(),
                1,
                "{restart:?}: {:?}",
                w.probe.events()
            );
        }

        // The supervisor is busy in a handler when the child ends, so its death
        // arm cannot run. The answer goes out, and a delete made after it is
        // served behind the classification of the stop, whichever of the two
        // the supervisor reaches first once the handler returns.
        let w = world().await;
        let cfg = NodeCfg {
            gated: true,
            ..NodeCfg::default()
        };
        let child = w.start_node("busy", RestartType::Transient, cfg).await;
        let id = child.id().clone();
        let call = {
            let sup = w.sup.clone();
            let id = id.clone();
            tokio::spawn(async move { sup.terminate_child(id).await })
        };
        w.probe.wait_entered().await;
        let boss = w.boss.clone();
        let held = tokio::spawn(async move { boss.send(Cmd::Hold).await });
        w.probe.wait_line("hold_entered").await;
        w.probe.gate.add_permits(1);
        within("the answer", call)
            .await
            .expect("the call task ends")
            .expect("the stop is answered");
        let delete = {
            let sup = w.sup.clone();
            tokio::spawn(async move { sup.delete_child(id).await })
        };
        // Queued behind the held handler.
        let mut delete = delete;
        assert!(
            timeout(QUIET, &mut delete).await.is_err(),
            "the delete was served while the handler ran"
        );
        w.probe.hold.add_permits(1);
        within("the hold", held).await.unwrap().unwrap();
        within("the delete", delete)
            .await
            .expect("the delete task ends")
            .expect("a delete made after the answer finds the child stopped");
    });
}

// ---------------------------------------------------------------------------
// a_dropped_reply_still_completes_the_stop
// ---------------------------------------------------------------------------

#[test]
fn a_dropped_reply_still_completes_the_stop() {
    common::run(4, async {
        // Row 1: the caller drops its future with the request still queued
        // (the supervisor is busy in a handler); the stop happens anyway.
        let w = world().await;
        let child = w
            .start_node("a", RestartType::Transient, NodeCfg::default())
            .await;
        let boss = w.boss.clone();
        let held = tokio::spawn(async move { boss.send(Cmd::Hold).await });
        w.probe.wait_line("hold_entered").await;
        let queued = timeout(Duration::ZERO, w.sup.terminate_child(child.id().clone())).await;
        assert!(queued.is_err(), "the request was queued and not yet served");
        w.probe.hold.add_permits(1);
        within("the hold", held).await.unwrap().unwrap();
        w.probe.wait_event("a").await;

        let info = w.info("a").await.expect("a transient child's spec is kept");
        assert!(!info.is_alive && !info.restart_pending, "{info:?}");
        assert_eq!(w.probe.events().len(), 1, "{:?}", w.probe.events());

        // Row 2: the caller drops its future after the commit, with the
        // child still ending; the end is classified and reported once.
        let cfg = NodeCfg {
            gated: true,
            ..NodeCfg::default()
        };
        let child = w.start_node("b", RestartType::Transient, cfg).await;
        let call = {
            let sup = w.sup.clone();
            let id = child.id().clone();
            tokio::spawn(async move { sup.terminate_child(id).await })
        };
        w.probe.wait_entered().await;
        call.abort();
        let _ = call.await;
        w.probe.gate.add_permits(1);
        w.probe.wait_event("b").await;
        let info = w.info("b").await.expect("the spec is kept");
        assert!(!info.is_alive && !info.restart_pending, "{info:?}");
        w.sup
            .delete_child(child.id().clone())
            .await
            .expect("delete");
        // Nothing further is reported for either child.
        w.ask(Cmd::Nothing).await;
        assert_eq!(w.probe.events().len(), 2, "{:?}", w.probe.events());

        // The same through a bound: the caller is told `Timeout`, and the
        // stop happens anyway.
        let w = world().await;
        let child = w
            .start_node("c", RestartType::Transient, NodeCfg::default())
            .await;
        let boss = w.boss.clone();
        let held = tokio::spawn(async move { boss.send(Cmd::Hold).await });
        w.probe.wait_line("hold_entered").await;
        let answer = w
            .sup
            .clone()
            .timeout(Duration::from_millis(50))
            .terminate_child(child.id().clone())
            .await;
        assert_eq!(answer.err(), Some(SupervisorRefError::Timeout));
        w.probe.hold.add_permits(1);
        within("the hold", held).await.unwrap().unwrap();
        w.probe.wait_event("c").await;
        let info = w.info("c").await.expect("a transient child's spec is kept");
        assert!(!info.is_alive && !info.restart_pending, "{info:?}");
        w.sup
            .delete_child(child.id().clone())
            .await
            .expect("delete");
        assert_eq!(w.probe.events().len(), 1, "{:?}", w.probe.events());
    });
}

// ---------------------------------------------------------------------------
// a_dropped_start_child_future_still_starts_the_child
// ---------------------------------------------------------------------------

/// The caller drops its `start_child` future with the request still queued
/// (the supervisor is busy in a handler); the supervisor starts the child.
#[test]
fn a_dropped_start_child_future_still_starts_the_child() {
    common::run(4, async {
        let w = world().await;
        let boss = w.boss.clone();
        let held = tokio::spawn(async move { boss.send(Cmd::Hold).await });
        w.probe.wait_line("hold_entered").await;
        let name = w.name("late");
        let queued = timeout(
            Duration::ZERO,
            w.sup.start_child(|| Plain, move |child| child.named(name)),
        )
        .await;
        assert!(queued.is_err(), "the request was queued and not yet served");
        w.probe.hold.add_permits(1);
        within("the hold", held).await.unwrap().unwrap();
        w.wait_alive("late").await;

        // A reference with a bound answers `Timeout` for a request that waits
        // behind the busy supervisor; the request is still served.
        let w = world().await;
        let boss = w.boss.clone();
        let held = tokio::spawn(async move { boss.send(Cmd::Hold).await });
        w.probe.wait_line("hold_entered").await;
        let name = w.name("bounded");
        let bounded = w.sup.clone().timeout(Duration::from_millis(50));
        let answer = bounded
            .start_child(|| Plain, move |child| child.named(name))
            .await;
        assert_eq!(answer.err(), Some(SupervisorRefError::Timeout));
        w.probe.hold.add_permits(1);
        within("the hold", held).await.unwrap().unwrap();
        w.wait_alive("bounded").await;

        // A call made from a runtime without the time driver, through a
        // reference with a bound, is refused before anything is sent: the
        // supervisor starts no child. The supervisor runs on this runtime;
        // the calls run on a second runtime built without `enable_time`.
        // The same reference without a bound is served from that runtime.
        let w = world().await;
        let sup = w.sup.clone();
        let (name_bounded, name_plain) = (w.name("no-driver"), w.name("plain"));
        let (tx, rx) = oneshot::channel();
        let caller = std::thread::spawn(move || {
            let bare = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let rows = bare.block_on(async {
                let before = sup.count_children().await;
                let bound = Duration::from_secs(5);
                let start = sup
                    .clone()
                    .timeout(bound)
                    .start_child(|| Plain, move |child| child.named(name_bounded))
                    .await
                    .map(|_| ());
                let read = sup.clone().timeout(bound).count_children().await;
                let after = sup.count_children().await;
                let served = sup
                    .start_child(|| Plain, move |child| child.named(name_plain))
                    .await
                    .map(|child| child.id().clone());
                (before, start, read, after, served)
            });
            let _ = tx.send(rows);
        });
        let (before, start, read, after, served) = within("the bare runtime's calls", rx)
            .await
            .expect("the calls on the runtime without a time driver returned");
        caller.join().unwrap();
        assert_eq!(start, Err(SupervisorRefError::NoTimeDriver));
        assert_eq!(read.map(|_| ()), Err(SupervisorRefError::NoTimeDriver));
        assert_eq!(before, after, "the refused start reached no supervisor");
        assert_eq!(after.map(|c| c.specs), Ok(0));
        let served = served.expect("a call without a bound is served from that runtime");
        let info = w
            .info("plain")
            .await
            .expect("the served child is registered");
        assert_eq!(info.id, served);
        assert!(w.info("no-driver").await.is_none());
    });
}

// ---------------------------------------------------------------------------
// reads_answer_at_a_boundary_while_the_mailbox_is_full
// ---------------------------------------------------------------------------

#[test]
fn reads_answer_at_a_boundary_while_the_mailbox_is_full() {
    common::run(4, async {
        let config = ActorConfig::default()
            .with_supervision(SupervisionConfig::one_for_one())
            .with_mailbox_capacity(2);
        let w = world_with(config, false).await;
        let boss = w.boss.clone();
        let held = tokio::spawn(async move { boss.send(Cmd::Hold).await });
        w.probe.wait_line("hold_entered").await;
        // Two messages fill the mailbox; each would spawn a child when handled.
        w.boss.notify(Cmd::Mark(1)).await.unwrap();
        w.boss.notify(Cmd::Mark(2)).await.unwrap();
        // Both requests are queued (each future polled once, then kept)
        // before the held handler returns.
        let mut counts = Box::pin(w.sup.count_children());
        let mut which = Box::pin(w.sup.which_children());
        assert!(timeout(Duration::ZERO, &mut counts).await.is_err());
        assert!(timeout(Duration::ZERO, &mut which).await.is_err());
        w.probe.hold.add_permits(1);
        within("the hold", held).await.unwrap().unwrap();
        let counts = within("count_children", counts).await.unwrap();
        let which = within("which_children", which).await.unwrap();
        // Served at the boundary after the held turn: ahead of both queued
        // messages, so neither child exists yet.
        assert_eq!(counts.specs, 0, "{counts:?}: it queued behind the mailbox");
        assert!(which.is_empty(), "{which:?}");
        w.probe.wait_line("mark:2").await;
        assert_eq!(w.sup.count_children().await.unwrap().specs, 2);
    });
}

// ---------------------------------------------------------------------------
// a_self_call_is_refused
// ---------------------------------------------------------------------------

#[test]
fn a_self_call_is_refused() {
    common::run(4, async {
        let w = world_with(
            ActorConfig::default().with_supervision(SupervisionConfig::one_for_one()),
            true,
        )
        .await;
        // From a handler.
        let text = match timeout(Duration::from_secs(5), w.boss.send(Cmd::SelfCall)).await {
            Ok(Ok(Reply::Text(text))) => text,
            other => panic!("the handler's calls did not return: {other:?}"),
        };
        for method in ["start", "terminate", "restart", "delete", "which", "count"] {
            assert!(
                text.contains(&format!("{method}:Some(CallingSelf)")),
                "{method} from a handler: {text}"
            );
        }
        // From `on_child_stopped`: a child ends, and the report calls them.
        let child = w
            .start_node("a", RestartType::Transient, NodeCfg::default())
            .await;
        w.sup.terminate_child(child.id().clone()).await.unwrap();
        w.probe.wait_event("a").await;
        let line = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(line) = w
                    .probe
                    .lines()
                    .into_iter()
                    .find(|l| l.starts_with("self_call_in_report:"))
                {
                    break line;
                }
                assert!(
                    Instant::now() < deadline,
                    "the report's calls did not return"
                );
                sleep(Duration::from_millis(5)).await;
            }
        };
        for method in ["start", "terminate", "restart", "delete", "which", "count"] {
            assert!(
                line.contains(&format!("{method}:Some(CallingSelf)")),
                "{method} from on_child_stopped: {line}"
            );
        }
        // Nothing was sent: the ledger holds the one child and the supervisor
        // serves a call from outside.
        assert_eq!(w.sup.count_children().await.unwrap().specs, 1);
    });
}

// ---------------------------------------------------------------------------
// gone_after_the_supervisor_ended
// ---------------------------------------------------------------------------

#[test]
fn gone_after_the_supervisor_ended() {
    common::run(4, async {
        let w = world().await;
        let child = w
            .start_node("a", RestartType::Transient, NodeCfg::default())
            .await;
        let id = child.id().clone();
        w.boss.stop(StopReason::Graceful).await.unwrap();
        within("the supervisor to end", w.boss.wait_stopped()).await;
        let gone = Err(SupervisorRefError::Gone);
        assert_eq!(w.sup.terminate_child(id.clone()).await, gone, "terminate");
        assert_eq!(w.sup.restart_child(id.clone()).await, gone, "restart");
        assert_eq!(w.sup.delete_child(id).await, gone, "delete");
        assert_eq!(
            w.sup.which_children().await.map(|_| ()),
            gone,
            "which_children"
        );
        assert_eq!(
            w.sup.count_children().await.map(|_| ()),
            gone,
            "count_children"
        );
        let started = w.sup.start_child(|| Plain, |c| c).await.map(|_| ());
        assert_eq!(started, gone, "start_child");

        // A request queued when the supervisor ends is answered `Gone` too.
        let w = world().await;
        let boss = w.boss.clone();
        let held = tokio::spawn(async move { boss.send(Cmd::Hold).await });
        w.probe.wait_line("hold_entered").await;
        let pending = {
            let sup = w.sup.clone();
            tokio::spawn(async move { sup.count_children().await.map(|_| ()) })
        };
        let pending_which = {
            let sup = w.sup.clone();
            tokio::spawn(async move { sup.which_children().await.map(|_| ()) })
        };
        w.boss.stop(StopReason::Graceful).await.unwrap();
        w.probe.hold.add_permits(1);
        let _ = within("the held handler", held).await;
        within("the supervisor to end", w.boss.wait_stopped()).await;
        assert_eq!(
            within("the queued count", pending).await.unwrap(),
            gone,
            "count_children queued at the end"
        );
        assert_eq!(
            within("the queued which", pending_which).await.unwrap(),
            gone,
            "which_children queued at the end"
        );
    });
}

// ---------------------------------------------------------------------------
// a_reference_names_one_incarnation
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// start_child_registers_with_the_configured_options
// ---------------------------------------------------------------------------

#[test]
fn start_child_registers_with_the_configured_options() {
    common::run(4, async {
        let w = world().await;

        // A closure that calls `await_init` makes the call wait for the
        // child's initialization: with the child parked in `pre_start` the
        // call is still pending, and it returns the handle when the start
        // finishes.
        let name = w.name("held");
        let held_cfg = NodeCfg {
            hold_start: true,
            ..NodeCfg::default()
        };
        let mut call = {
            let sup = w.sup.clone();
            let probe = w.probe.clone();
            let label = name.clone();
            tokio::spawn(async move {
                sup.start_child(
                    move || Node::new(probe.clone(), held_cfg.clone(), label.clone(), 0),
                    move |child| child.named(name).await_init(),
                )
                .await
            })
        };
        w.probe.wait_start_entered().await;
        assert!(
            timeout(QUIET, &mut call).await.is_err(),
            "the call waits for the child's initialization"
        );
        w.probe.start_gate.add_permits(1);
        let held = within("the awaited start", call)
            .await
            .unwrap()
            .expect("the child starts once its initialization finishes");
        let info = w.info("held").await.expect("the child is registered");
        assert_eq!(info.id, *held.id());
        assert!(info.is_alive, "{info:?}");

        let boss_task = match w.ask(Cmd::TaskId).await {
            Reply::Task(id) => id,
            other => panic!("expected a task id, got {other:?}"),
        };
        let probe = w.probe.clone();
        let factory_probe = w.probe.clone();
        let name = w.name("opts");
        let child = w
            .sup
            .start_child(
                move || {
                    factory_probe
                        .closure_tasks
                        .lock()
                        .unwrap()
                        .push(tokio::task::id());
                    Plain
                },
                move |child| {
                    probe.closure_tasks.lock().unwrap().push(tokio::task::id());
                    child
                        .named(name)
                        .restart_type(RestartType::Transient)
                        .shutdown(Shutdown::Timeout(Duration::from_secs(7)))
                        .kill_grace(Duration::from_millis(300))
                },
            )
            .await
            .expect("the child starts");
        let info = w.info("opts").await.expect("the child is registered");
        assert_eq!(info.id, *child.id());
        assert_eq!(info.restart_type, RestartType::Transient);
        assert_eq!(info.shutdown, Shutdown::Timeout(Duration::from_secs(7)));
        assert!(info.is_alive && !info.is_supervisor, "{info:?}");
        // The factory and the closure ran once each, on the supervisor's own
        // task, between its callbacks.
        let tasks = w.probe.closure_tasks.lock().unwrap().clone();
        assert_eq!(tasks, vec![boss_task, boss_task], "factory and closure");
        // Its turn is a hook turn of the system kind that returned.
        w.turns_of(HookKind::Sys, TurnOutcome::Returned, 1).await;

        // A second child of the same name maps `DuplicateChild` to `Spawn`.
        let name = w.name("opts");
        let again = w
            .sup
            .start_child(|| Plain, move |child| child.named(name))
            .await
            .map(|_| ());
        assert_eq!(
            again,
            Err(SupervisorRefError::Spawn(SpawnError::DuplicateChild(
                w.name("opts").as_str().into()
            )))
        );
    });
}

// ---------------------------------------------------------------------------
// count_children_counts_workers_supervisors_and_a_restarting_child
// ---------------------------------------------------------------------------

#[test]
fn count_children_counts_workers_supervisors_and_a_restarting_child() {
    common::run(4, async {
        let w = world().await;
        let plain = NodeCfg::default();
        w.start_node("w1", RestartType::Permanent, plain.clone())
            .await;
        let w2 = w
            .start_node("w2", RestartType::Transient, plain.clone())
            .await;
        // A supervisor child.
        let probe = w.probe.clone();
        let name = w.name("sub");
        let label = name.clone();
        w.sup
            .start_child(
                move || Node::new(probe.clone(), NodeCfg::default(), label.clone(), 0),
                move |child| {
                    child
                        .named(name)
                        .with_config(ActorConfig::default().supervisor())
                },
            )
            .await
            .expect("the supervisor child starts");
        // A child whose restart is held in flight.
        let held = NodeCfg {
            hold_restart: true,
            ..NodeCfg::default()
        };
        let crasher = w.start_node("crasher", RestartType::Permanent, held).await;
        crasher.notify(Msg::Crash).await.unwrap();
        w.probe.wait_start_entered().await;
        // A stopped child whose spec is kept.
        w.sup.terminate_child(w2.id().clone()).await.unwrap();

        let counts = w.sup.count_children().await.expect("count");
        assert_eq!(
            (
                counts.specs,
                counts.active,
                counts.supervisors,
                counts.workers
            ),
            (4, 2, 1, 3),
            "{counts:?}: neither the restarting child nor the stopped one is active"
        );
        let which = w.sup.which_children().await.expect("which");
        assert_eq!(which.len(), 4);
        let by = |base: &str| {
            let name = w.name(base);
            which
                .iter()
                .find(|c| c.name.as_deref() == Some(name.as_str()))
                .cloned()
                .unwrap_or_else(|| panic!("no child {base}: {which:?}"))
        };
        assert!(by("sub").is_supervisor && !by("w1").is_supervisor);
        assert!(by("crasher").restart_pending && !by("crasher").is_alive);
        assert!(!by("w2").is_alive && !by("w2").restart_pending);

        // Once the restart finishes the child is active again.
        w.probe.start_gate.add_permits(1);
        w.wait_alive("crasher").await;
        let counts = w.sup.count_children().await.expect("count");
        assert_eq!(
            counts.active, 3,
            "{counts:?}: the restarted child is active"
        );
    });
}

// ---------------------------------------------------------------------------
// restart_and_delete_errors_map_to_supervision
// ---------------------------------------------------------------------------

#[test]
fn restart_and_delete_errors_map_to_supervision() {
    common::run(4, async {
        let w = world().await;
        let child = w
            .start_node("a", RestartType::Transient, NodeCfg::default())
            .await;
        let id = child.id().clone();
        let missing = tokio_actors::ActorId::from("nobody");
        let sup = &w.sup;
        let kind = |r: Result<(), SupervisorRefError>| match r {
            Ok(()) => "ok".to_string(),
            Err(SupervisorRefError::Supervision(SupervisionError::ChildRunning(_))) => {
                "running".into()
            }
            Err(SupervisorRefError::Supervision(SupervisionError::ChildNotFound(_))) => {
                "not_found".into()
            }
            Err(other) => format!("{other:?}"),
        };
        // A running child is refused by both, and nothing changes.
        assert_eq!(kind(sup.restart_child(id.clone()).await), "running");
        assert_eq!(kind(sup.delete_child(id.clone()).await), "running");
        assert_eq!(sup.count_children().await.unwrap().specs, 1);
        assert!(
            child.is_alive(),
            "a refused delete leaves the child running"
        );
        // An unknown child is not found.
        assert_eq!(kind(sup.restart_child(missing.clone()).await), "not_found");
        assert_eq!(kind(sup.delete_child(missing.clone()).await), "not_found");
        assert_eq!(kind(sup.terminate_child(missing).await), "not_found");
        // A stopped child restarts, and then deletes only once stopped again.
        sup.terminate_child(id.clone()).await.unwrap();
        sup.restart_child(id.clone()).await.expect("restart");
        w.wait_alive("a").await;
        sup.terminate_child(id.clone()).await.unwrap();
        assert_eq!(kind(sup.delete_child(id.clone()).await), "ok");
        assert_eq!(kind(sup.delete_child(id).await), "not_found");

        // An actor that is no supervisor answers `NotASupervisor`.
        let plain = Plain
            .spawn()
            .named(format!("plain{}", w.tag))
            .await
            .unwrap();
        let sup = SupervisorRef::of(&plain);
        assert_eq!(
            sup.count_children().await,
            Err(SupervisorRefError::Supervision(
                SupervisionError::NotASupervisor
            ))
        );
        assert_eq!(
            sup.which_children().await.map(|_| ()),
            Err(SupervisorRefError::Supervision(
                SupervisionError::NotASupervisor
            ))
        );
        assert_eq!(
            sup.restart_child("x").await,
            Err(SupervisorRefError::Supervision(
                SupervisionError::NotASupervisor
            ))
        );
        assert_eq!(
            sup.delete_child("x").await,
            Err(SupervisorRefError::Supervision(
                SupervisionError::NotASupervisor
            ))
        );
        assert_eq!(
            sup.start_child(|| Plain, |c| c).await.map(|_| ()),
            Err(SupervisorRefError::Spawn(SpawnError::NotASupervisor))
        );
    });
}

// ---------------------------------------------------------------------------
// a_configure_panic_fails_the_supervisor
// ---------------------------------------------------------------------------

/// A waker that panics in its wake while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: AtomicUsize,
}

impl std::task::Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("the caller's waker panics");
        }
    }
}

/// The factory and the configure closure run on the supervisor's task. A panic
/// of either is a panic of that task: the caller reads `Gone`, and the
/// supervisor ends with it as with a handler's. Row 2 of each site: the
/// caller's waker panics when the unwind drops the caller's reply, which must
/// not be a second panic in the unwind. The scenario runs in a child process
/// of this test binary, since that second panic would end the process.
#[test]
fn a_configure_panic_fails_the_supervisor() {
    if !common::in_a_child_process("a_configure_panic_fails_the_supervisor") {
        return;
    }
    // The panic waits for the test, so the caller is parked on its reply when
    // the unwind drops it: a thread of its own for the supervisor's task.
    common::run_multi_thread_only(4, async {
        for site in ["configure", "factory"] {
            for loud_caller in [false, true] {
                let row = format!("{site} loud={loud_caller}");
                let w = world().await;
                let release = Arc::new(AtomicBool::new(false));
                let entered = Arc::new(AtomicBool::new(false));
                let hold = {
                    let (release, entered) = (release.clone(), entered.clone());
                    move || {
                        entered.store(true, Ordering::SeqCst);
                        let until = std::time::Instant::now() + BOUND;
                        while !release.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                };
                let loud = Arc::new(LoudWake::default());
                let waker = std::task::Waker::from(loud.clone());
                let sup = w.sup.clone();
                let mut call: std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>> =
                    if site == "configure" {
                        Box::pin(async move {
                            sup.start_child(
                                || Plain,
                                move |_child| {
                                    hold();
                                    panic!("configure panicked")
                                },
                            )
                            .await
                            .map(|_| ())
                        })
                    } else {
                        Box::pin(async move {
                            sup.start_child(
                                move || -> Plain {
                                    hold();
                                    panic!("factory panicked")
                                },
                                |child| child,
                            )
                            .await
                            .map(|_| ())
                        })
                    };
                let result = if loud_caller {
                    // Poll with the caller's waker until the panic is about to
                    // happen; one more poll parks the caller on its reply.
                    let mut cx = std::task::Context::from_waker(&waker);
                    while !entered.load(Ordering::SeqCst) {
                        assert!(
                            std::future::Future::poll(call.as_mut(), &mut cx).is_pending(),
                            "{row}: the call ended before the panic"
                        );
                        sleep(Duration::from_millis(1)).await;
                    }
                    assert!(
                        std::future::Future::poll(call.as_mut(), &mut cx).is_pending(),
                        "{row}"
                    );
                    loud.armed.store(true, Ordering::SeqCst);
                    let woken = loud.woken.load(Ordering::SeqCst);
                    release.store(true, Ordering::SeqCst);
                    within("the caller's waker to be woken", async {
                        while loud.woken.load(Ordering::SeqCst) == woken {
                            sleep(Duration::from_millis(1)).await;
                        }
                    })
                    .await;
                    loud.armed.store(false, Ordering::SeqCst);
                    match std::future::Future::poll(call.as_mut(), &mut cx) {
                        std::task::Poll::Ready(result) => result,
                        std::task::Poll::Pending => panic!("{row}: the answer never came"),
                    }
                } else {
                    release.store(true, Ordering::SeqCst);
                    within("the call", call).await
                };
                assert_eq!(result, Err(SupervisorRefError::Gone), "{row}");
                let reason = within("the supervisor to end", w.boss.wait_stopped()).await;
                match reason {
                    StopReason::Failure(ActorError::Panic(message)) => {
                        assert!(message.contains(&format!("{site} panicked")), "{message}");
                    }
                    other => panic!("{row}: expected a panic failure, got {other:?}"),
                }
                // The closure's turn is a hook turn of the system kind that
                // ended as a panic.
                w.turns_of(HookKind::Sys, TurnOutcome::Panicked, 1).await;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// the_stop_is_answered_ok_when_the_supervisor_ends_after_the_child_did
// ---------------------------------------------------------------------------

/// The child's end was seen by the helper, and the supervisor ends before it
/// takes the request that finishes the classification: the stop completed, so
/// the answer is `Ok`, not `Gone`.
#[test]
fn the_stop_is_answered_ok_when_the_supervisor_ends_after_the_child_did() {
    common::run(4, async {
        let w = world().await;
        let cfg = NodeCfg {
            gated: true,
            ..NodeCfg::default()
        };
        let child = w.start_node("c", RestartType::Transient, cfg).await;
        let id = child.id().clone();
        let call = {
            let sup = w.sup.clone();
            tokio::spawn(async move { sup.terminate_child(id).await })
        };
        // The child's `on_stopped` is held, so the helper waits for its end.
        w.probe.wait_entered().await;
        // The supervisor is asked to stop; its plane closes when its loop
        // leaves, and the child's stop is still held.
        let boss = w.boss.clone();
        let stopping = tokio::spawn(async move { boss.stop(StopReason::Graceful).await });
        within("the stop request", stopping).await.unwrap().unwrap();
        within("the supervisor's plane to close", async {
            while w.sup.count_children().await.map(|_| ()) != Err(SupervisorRefError::Gone) {
                sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
        assert!(
            !call.is_finished(),
            "the answer came before the child ended"
        );
        w.probe.gate.add_permits(1);
        let answer = within("the answer", call)
            .await
            .expect("the call task ends");
        assert_eq!(answer, Ok(()), "the child ended before the supervisor did");
    });
}

// ---------------------------------------------------------------------------
// a_stuck_childs_verdict_goes_to_the_caller_or_ends_the_supervisor
// ---------------------------------------------------------------------------

/// The child's `on_stopped` blocks its thread, so its whole stop ladder runs
/// out and it carries the Unresponsive verdict. Row 1: the caller waits, is
/// told, and decides; the supervisor goes on. Row 2: the caller dropped its
/// future after the stop was committed, so nobody can be told and the
/// supervisor ends with the verdict. The turn needs a thread of its own.
#[test]
fn a_stuck_childs_verdict_goes_to_the_caller_or_ends_the_supervisor() {
    common::run_multi_thread_only(4, async {
        for caller_stays in [true, false] {
            let row = format!("caller_stays={caller_stays}");
            let w = world().await;
            let probe = w.probe.clone();
            let name = w.name("stuck");
            let label = name.clone();
            let cfg = NodeCfg {
                block_stop: true,
                ..NodeCfg::default()
            };
            let child = w
                .sup
                .start_child(
                    move || Node::new(probe.clone(), cfg.clone(), label.clone(), 0),
                    move |child| {
                        child
                            .named(name)
                            .restart_type(RestartType::Transient)
                            .shutdown(Shutdown::Timeout(Duration::from_millis(50)))
                            .kill_grace(Duration::from_millis(100))
                    },
                )
                .await
                .expect("the child starts");
            let id = child.id().clone();
            let call = {
                let sup = w.sup.clone();
                let id = id.clone();
                tokio::spawn(async move { sup.terminate_child(id).await })
            };
            // The stop is committed and the child's hook holds its thread.
            w.probe.wait_entered().await;
            if caller_stays {
                let answer = within("the answer", call)
                    .await
                    .expect("the call task ends");
                assert_eq!(
                    answer,
                    Err(SupervisorRefError::Supervision(
                        SupervisionError::ChildUnresponsive(id)
                    )),
                    "{row}"
                );
                assert!(
                    timeout(Duration::from_millis(500), w.boss.wait_stopped())
                        .await
                        .is_err(),
                    "{row}: the supervisor ended over a verdict its caller was told"
                );
                assert!(w.sup.count_children().await.is_ok(), "{row}");
            } else {
                call.abort();
                let _ = call.await;
                let reason = within("the supervisor to end", w.boss.wait_stopped()).await;
                match reason {
                    StopReason::Failure(ActorError::Supervision(
                        SupervisionError::ChildUnresponsive(stuck),
                    )) => assert_eq!(stuck, id, "{row}"),
                    other => panic!("{row}: expected ChildUnresponsive, got {other:?}"),
                }
            }
        }
    });
}
