#![allow(dead_code)]
//! Which children leave a spec behind when they stop, and what a same-name
//! `spawn_child` and `delete_child` answer while a spec is held.
//!
//! A supervisor refuses a `spawn_child` under a name whose spec it holds,
//! whatever the child is doing: it runs, it died and its death is not handled
//! yet, it is being stopped, it is being restarted, or it is down with its
//! spec kept until `delete_child` removes it. A `Temporary` child, and any
//! child of a `SimpleOneForOne` supervisor, leaves no spec once its end is
//! classified, so its name is free at once and `delete_child` has nothing to
//! remove.
//!
//! Every scenario runs its calls inside one handler of the supervisor, the
//! only place where they meet the states between two turns of its message
//! loop: while the handler runs, the loop handles no death and adopts no
//! restart.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, timeout, Instant};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorSystem, ChildEvent, RestartType,
    Shutdown, SpawnError, StopReason, SupervisionAction, SupervisionConfig, SupervisionError,
};

// ---------------------------------------------------------------------------
// Timing and names
// ---------------------------------------------------------------------------

/// The longest a wait for an outcome may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const BOUND: Duration = Duration::from_secs(20);

/// The longest a turn or an init held on a gate holds its thread when the
/// test never opens the gate.
const GATE_CAP: Duration = Duration::from_secs(30);

/// The kill grace of a leaf whose turn the tests hold: far past the turn, so
/// only the leaf's own end frees its name.
const LEAF_GRACE: Duration = Duration::from_millis(1500);

/// The turn a leaf holds in the case of a killed supervisor child: long
/// enough that a `terminate_child` that returns before the leaf ended is seen
/// on a host that stalls its threads.
const LEAF_TURN: Duration = Duration::from_millis(400);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("kept-spec-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

/// Awaits `fut` for at most [`BOUND`].
async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {BOUND:?} waiting for: {what}"),
    }
}

/// Polls `pred` every 5 ms until it holds, for at most [`BOUND`].
async fn wait_until(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + BOUND;
    loop {
        if pred() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {BOUND:?} waiting for: {what}"
        );
        sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// The child under test
// ---------------------------------------------------------------------------

/// What a test tells a child to do and what it observes of it, across the
/// child's restarts.
#[derive(Default)]
struct Knobs {
    /// Calls of the child's factory.
    builds: AtomicUsize,
    /// Incarnations that began `on_started`.
    starts: AtomicUsize,
    /// While set, `on_started` waits.
    hold_start: AtomicBool,
    /// While set, `pre_stop` refuses a stop.
    veto_stop: AtomicBool,
    /// Set: the next `on_stopped` fails, once.
    fail_stop: AtomicBool,
    /// Set when the held turn began.
    stuck_entered: AtomicBool,
    /// Set by the test to end the held turn.
    stuck_open: AtomicBool,
    /// Set when the held turn ended.
    stuck_returned: AtomicBool,
    /// While set, the supervisor's held turn waits.
    hold_turn: AtomicBool,
    /// Set when the supervisor's held turn began.
    turn_entered: AtomicBool,
}

/// Ends the holds a test put on a child or on the supervisor when it goes out
/// of scope, so a failed assertion cannot leave a turn or an init waiting.
struct Release(Arc<Knobs>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.hold_start.store(false, Ordering::SeqCst);
        self.0.veto_stop.store(false, Ordering::SeqCst);
        self.0.stuck_open.store(true, Ordering::SeqCst);
        self.0.hold_turn.store(false, Ordering::SeqCst);
    }
}

#[derive(Clone)]
enum ChildMsg {
    /// Fails the child with a panic.
    Crash,
    /// Stops the child cleanly, at its own request.
    Finish,
    /// Holds the child's thread until the test opens the gate.
    Stick,
}

struct Child(Arc<Knobs>);

impl Actor for Child {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Crash => panic!("the child crashes on command"),
            ChildMsg::Finish => {
                let _ = ctx.self_handle().stop(StopReason::Graceful).await;
                Ok(())
            }
            ChildMsg::Stick => {
                self.0.stuck_entered.store(true, Ordering::SeqCst);
                let cap = std::time::Instant::now() + GATE_CAP;
                while !self.0.stuck_open.load(Ordering::SeqCst) && std::time::Instant::now() < cap {
                    std::thread::sleep(Duration::from_millis(1));
                }
                self.0.stuck_returned.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.0.starts.fetch_add(1, Ordering::SeqCst);
        let cap = Instant::now() + GATE_CAP;
        while self.0.hold_start.load(Ordering::SeqCst) && Instant::now() < cap {
            sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        !self.0.veto_stop.load(Ordering::SeqCst)
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if self.0.fail_stop.swap(false, Ordering::SeqCst) {
            return Err(ActorError::User("the cleanup fails".to_string()));
        }
        Ok(())
    }
}

/// How a script spawns a child.
#[derive(Clone)]
struct Spec {
    name: String,
    restart: RestartType,
    shutdown: Option<Shutdown>,
    kill_grace: Option<Duration>,
    start_timeout: Option<Duration>,
    knobs: Arc<Knobs>,
}

impl Spec {
    fn new(name: &str, restart: RestartType, knobs: &Arc<Knobs>) -> Self {
        Self {
            name: name.to_string(),
            restart,
            shutdown: None,
            kill_grace: None,
            start_timeout: None,
            knobs: knobs.clone(),
        }
    }

    fn shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    fn kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = Some(grace);
        self
    }

    fn start_timeout(mut self, bound: Duration) -> Self {
        self.start_timeout = Some(bound);
        self
    }
}

// ---------------------------------------------------------------------------
// The supervisor and its scripts
// ---------------------------------------------------------------------------

/// One call of a script. A script runs inside one handler of the supervisor
/// and answers one line per call.
#[derive(Clone)]
enum Op {
    /// `spawn_child` under the spec's name: `spawned`, `duplicate` or the
    /// error text.
    Spawn(Spec),
    /// `terminate_child`: `terminated`, `unresponsive` or the error text.
    Terminate(String),
    /// `terminate_child` with the wait dropped at once: the stop is committed
    /// and goes on without the caller.
    TerminateAbandoned(String),
    /// `stop_child`, the outcome printed.
    StopChild(String),
    /// `delete_child`: `deleted`, `not found`, `running`, `restarting` or the
    /// error text.
    Delete(String),
    /// What `children()` says of the child: `alive=<bool> pending=<bool>`, or
    /// `no such child`.
    Read(String),
    /// Sends the child a message that fails it.
    Crash(String),
    /// Polls `children()` every 5 ms until the child is no longer alive: for
    /// a child whose task ended, until the supervisor reads its end.
    UntilNotAlive(String),
    /// Remembers the clock.
    Mark,
    /// Prints the time since the last `Mark`.
    Elapsed,
    /// Holds the supervisor's turn until the test releases it: the loop
    /// handles nothing meanwhile.
    HoldTurn(Arc<Knobs>),
}

type Events = Arc<Mutex<Vec<ChildEvent>>>;

/// The supervisor under test: it records what `on_child_stopped` is told, and
/// answers a death delivered while it stops by trying the spawn in `late`.
struct Sup {
    events: Events,
    late: Arc<Mutex<Option<Spec>>>,
    late_answers: Arc<Mutex<Vec<String>>>,
}

async fn spawn_line(ctx: &mut ActorContext<Sup>, spec: &Spec) -> String {
    let knobs = spec.knobs.clone();
    let mut builder = ctx
        .spawn_child(move || {
            knobs.builds.fetch_add(1, Ordering::SeqCst);
            Child(knobs.clone())
        })
        .named(spec.name.clone())
        .restart_type(spec.restart);
    if let Some(shutdown) = spec.shutdown {
        builder = builder.shutdown(shutdown);
    }
    if let Some(grace) = spec.kill_grace {
        builder = builder.kill_grace(grace);
    }
    if let Some(bound) = spec.start_timeout {
        builder = builder.start_timeout(bound);
    }
    match builder.await {
        Ok(_) => "spawned".to_string(),
        Err(SpawnError::DuplicateChild(_)) => "duplicate".to_string(),
        Err(SpawnError::SupervisorStopping) => "stopping".to_string(),
        Err(other) => format!("spawn: {other}"),
    }
}

impl Actor for Sup {
    type Message = Vec<Op>;
    type Response = Vec<String>;

    async fn handle(
        &mut self,
        ops: Vec<Op>,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Vec<String>> {
        let mut lines = Vec::with_capacity(ops.len());
        let mut mark = Instant::now();
        for op in ops {
            let line = match op {
                Op::Spawn(spec) => spawn_line(ctx, &spec).await,
                Op::Terminate(name) => match ctx.terminate_child(name).await {
                    Ok(()) => "terminated".to_string(),
                    Err(SupervisionError::ChildUnresponsive(_)) => "unresponsive".to_string(),
                    Err(other) => format!("terminate: {other}"),
                },
                Op::TerminateAbandoned(name) => {
                    let _ = timeout(Duration::ZERO, ctx.terminate_child(name)).await;
                    "abandoned".to_string()
                }
                Op::StopChild(name) => match ctx.stop_child(name).await {
                    Ok(outcome) => format!("stop: {outcome:?}"),
                    Err(other) => format!("stop: {other}"),
                },
                Op::Delete(name) => match ctx.delete_child(name) {
                    Ok(()) => "deleted".to_string(),
                    Err(SupervisionError::ChildNotFound(_)) => "not found".to_string(),
                    Err(SupervisionError::ChildRunning(_)) => "running".to_string(),
                    Err(SupervisionError::ChildRestarting(_)) => "restarting".to_string(),
                    Err(other) => format!("delete: {other}"),
                },
                Op::Read(name) => match ctx
                    .children()
                    .into_iter()
                    .find(|info| info.id.as_str() == name)
                {
                    Some(info) => {
                        format!("alive={} pending={}", info.is_alive, info.restart_pending)
                    }
                    None => "no such child".to_string(),
                },
                Op::Crash(name) => {
                    let child = ActorSystem::default()
                        .get::<Child>(&name)
                        .expect("the child is registered");
                    child
                        .notify(ChildMsg::Crash)
                        .await
                        .expect("the child takes the message");
                    "crash sent".to_string()
                }
                Op::UntilNotAlive(name) => {
                    let deadline = Instant::now() + BOUND;
                    loop {
                        let alive = ctx
                            .children()
                            .iter()
                            .any(|info| info.id.as_str() == name && info.is_alive);
                        if !alive {
                            break "not alive".to_string();
                        }
                        if Instant::now() >= deadline {
                            break "still alive at the deadline".to_string();
                        }
                        sleep(Duration::from_millis(5)).await;
                    }
                }
                Op::Mark => {
                    mark = Instant::now();
                    "marked".to_string()
                }
                Op::Elapsed => format!("elapsed={:?}", mark.elapsed()),
                Op::HoldTurn(knobs) => {
                    knobs.turn_entered.store(true, Ordering::SeqCst);
                    let cap = Instant::now() + GATE_CAP;
                    while knobs.hold_turn.load(Ordering::SeqCst) && Instant::now() < cap {
                        sleep(Duration::from_millis(5)).await;
                    }
                    "released".to_string()
                }
            };
            lines.push(line);
        }
        Ok(lines)
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().unwrap().push(event.clone());
        if event.action == SupervisionAction::SupervisorStopping {
            let late = self.late.lock().unwrap().clone();
            if let Some(spec) = late {
                let answer = spawn_line(ctx, &spec).await;
                self.late_answers.lock().unwrap().push(answer);
            }
        }
        Ok(())
    }
}

/// A running supervisor and what its `on_child_stopped` saw.
struct Rig {
    sup: ActorHandle<Sup>,
    events: Events,
    late: Arc<Mutex<Option<Spec>>>,
    late_answers: Arc<Mutex<Vec<String>>>,
}

impl Rig {
    async fn new(config: SupervisionConfig) -> Self {
        let events = Events::default();
        let late = Arc::new(Mutex::new(None));
        let late_answers = Arc::new(Mutex::new(Vec::new()));
        let sup = within("the supervisor to start", async {
            Sup {
                events: events.clone(),
                late: late.clone(),
                late_answers: late_answers.clone(),
            }
            .spawn()
            .named(uname("sup"))
            .with_supervision(config)
            .await
        })
        .await
        .expect("the supervisor starts");
        Self {
            sup,
            events,
            late,
            late_answers,
        }
    }

    /// Runs the calls in one handler of the supervisor.
    async fn script(&self, ops: Vec<Op>) -> Vec<String> {
        within("the supervisor to answer", self.sup.send(ops))
            .await
            .expect("the supervisor answers")
    }

    /// Reads the child once per handler turn until it reads `expected`, so
    /// the supervisor's loop runs between two reads.
    async fn until_read(&self, name: &str, expected: &str) {
        let deadline = Instant::now() + BOUND;
        loop {
            let line = self
                .script(vec![Op::Read(name.to_string())])
                .await
                .remove(0);
            if line == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {BOUND:?} waiting for `{name}` to read `{expected}`, last read `{line}`"
            );
            sleep(Duration::from_millis(5)).await;
        }
    }

    /// The actions `on_child_stopped` was told of the child, in order.
    fn reported(&self, name: &str) -> Vec<SupervisionAction> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.child_name.as_deref() == Some(name))
            .map(|event| event.action)
            .collect()
    }

    async fn stop(self) {
        within("the supervisor to stop", async {
            self.sup
                .stop(StopReason::Graceful)
                .await
                .expect("the stop is accepted");
            self.sup.wait_stopped().await;
        })
        .await;
    }
}

/// Sends the named child a message through the default system.
async fn tell(name: &str, msg: ChildMsg) {
    let child = ActorSystem::default()
        .get::<Child>(name)
        .expect("the child is registered");
    within("the child to take the message", child.notify(msg))
        .await
        .expect("the child takes the message");
}

/// Kills the named actor through the default system.
async fn kill(name: &str) {
    within("the kill to be accepted", ActorSystem::default().kill(name))
        .await
        .expect("the kill is accepted");
}

// ---------------------------------------------------------------------------
// A stopped child keeps its spec
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A removed spec leaves the name free
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A terminated supervisor child frees the names of its descendants
// ---------------------------------------------------------------------------

/// The names and counters of one tree: a `Top` supervisor, a `Mid`
/// supervisor child and the `Leaf` it spawns under a name of its own.
struct Tree {
    leaf: String,
    /// Whether `Mid` is stopped with `Kill` and its first leaf holds a turn.
    killed: bool,
    /// Incarnations of `Mid` that began `on_started`.
    starts: AtomicUsize,
    /// Spawns of the leaf that were refused.
    refused: AtomicUsize,
    /// Set when the first leaf began its held turn.
    entered: AtomicBool,
}

struct Leaf(Arc<Tree>);

impl Actor for Leaf {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.0.entered.store(true, Ordering::SeqCst);
        sleep(LEAF_TURN).await;
        Ok(())
    }
}

struct Mid(Arc<Tree>);

impl Actor for Mid {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let incarnation = self.0.starts.fetch_add(1, Ordering::SeqCst) + 1;
        let tree = self.0.clone();
        let spawned = ctx
            .spawn_child(move || Leaf(tree.clone()))
            .named(self.0.leaf.clone())
            .kill_grace(LEAF_GRACE)
            .await;
        match spawned {
            Ok(leaf) => {
                if self.0.killed && incarnation == 1 {
                    leaf.notify(()).await.expect("the leaf takes the message");
                }
                Ok(())
            }
            Err(err) => {
                self.0.refused.fetch_add(1, Ordering::SeqCst);
                Err(err.into())
            }
        }
    }
}

async fn spawn_mid(ctx: &mut ActorContext<Top>, tree: &Arc<Tree>, name: &str) -> String {
    let world = tree.clone();
    let mut builder = ctx
        .spawn_child(move || Mid(world.clone()))
        .named(name.to_string())
        .with_config(ActorConfig::default().with_supervision(SupervisionConfig::one_for_one()));
    if tree.killed {
        builder = builder
            .shutdown(Shutdown::Kill)
            .kill_grace(Duration::from_millis(20));
    }
    match builder.await {
        Ok(_) => "spawned".to_string(),
        Err(err) => format!("spawn: {err}"),
    }
}

struct Top;

impl Actor for Top {
    type Message = Arc<Tree>;
    type Response = Vec<String>;

    /// Spawns `Mid`, waits until its leaf is up, terminates `Mid`, reads
    /// whether the leaf's name is free, deletes `Mid` and spawns it again,
    /// all in this one handler.
    async fn handle(
        &mut self,
        tree: Arc<Tree>,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Vec<String>> {
        let system = ActorSystem::default();
        let mid = format!("{}-mid", tree.leaf);
        let mut lines = vec![spawn_mid(ctx, &tree, &mid).await];
        let deadline = Instant::now() + BOUND;
        let up = loop {
            let up = system.get::<Leaf>(&tree.leaf).is_some()
                && (!tree.killed || tree.entered.load(Ordering::SeqCst));
            if up || Instant::now() >= deadline {
                break up;
            }
            sleep(Duration::from_millis(5)).await;
        };
        lines.push(format!("leaf up={up}"));
        lines.push(match ctx.terminate_child(mid.clone()).await {
            Ok(()) => "terminated".to_string(),
            Err(err) => format!("terminate: {err}"),
        });
        lines.push(format!(
            "leaf name free={}",
            system.get::<Leaf>(&tree.leaf).is_none()
        ));
        lines.push(match ctx.delete_child(mid.clone()) {
            Ok(()) => "deleted".to_string(),
            Err(err) => format!("delete: {err}"),
        });
        lines.push(spawn_mid(ctx, &tree, &mid).await);
        Ok(lines)
    }
}

async fn descendants_names_round(killed: bool, round: usize) {
    let top = within("the top supervisor to start", async {
        Top.spawn().named(uname("top")).supervisor().await
    })
    .await
    .expect("the top supervisor starts");
    let tree = Arc::new(Tree {
        leaf: uname("leaf"),
        killed,
        starts: AtomicUsize::new(0),
        refused: AtomicUsize::new(0),
        entered: AtomicBool::new(false),
    });
    let lines = within("the top supervisor's cycle", top.send(tree.clone()))
        .await
        .expect("the top supervisor answers");
    assert_eq!(
        lines,
        [
            "spawned",
            "leaf up=true",
            "terminated",
            "leaf name free=true",
            "deleted",
            "spawned"
        ],
        "killed={killed}, round {round}"
    );
    // The respawned `Mid` spawns its leaf under the same name. A refused
    // spawn fails its init and its supervisor restarts it, after which the
    // name comes up all the same, so only the counter shows a refusal.
    wait_until("the second Mid to start", || {
        tree.starts.load(Ordering::SeqCst) >= 2
    })
    .await;
    wait_until("the second leaf's name", || {
        ActorSystem::default().get::<Leaf>(&tree.leaf).is_some()
    })
    .await;
    assert_eq!(
        tree.refused.load(Ordering::SeqCst),
        0,
        "no leaf spawn was refused: killed={killed}, round {round}"
    );
    within("the top supervisor to stop", async {
        top.stop(StopReason::Graceful)
            .await
            .expect("the stop is accepted");
        top.wait_stopped().await;
    })
    .await;
}

// ---------------------------------------------------------------------------
// A name stays held in every state of a child that is not stopped and kept
// ---------------------------------------------------------------------------

#[test]
fn a_crashed_temporary_child_keeps_its_name_until_its_death_is_handled() {
    // The spec of a `Temporary` child is removed when the supervisor handles
    // its end, not when its task ends: the name is held until then.
    common::run(4, async {
        let rig = Rig::new(SupervisionConfig::one_for_one()).await;
        let name = uname("temporary");
        let knobs = Arc::new(Knobs::default());
        let spec = Spec::new(&name, RestartType::Temporary, &knobs);
        let lines = rig
            .script(vec![
                Op::Spawn(spec.clone()),
                Op::Crash(name.clone()),
                Op::UntilNotAlive(name.clone()),
                Op::Spawn(spec.clone()),
            ])
            .await;
        assert_eq!(lines, ["spawned", "crash sent", "not alive", "duplicate"]);
        rig.until_read(&name, "no such child").await;
        let lines = rig.script(vec![Op::Spawn(spec)]).await;
        assert_eq!(lines, ["spawned"]);
        assert_eq!(rig.reported(&name), [SupervisionAction::Removed]);
        rig.stop().await;
    });
}
