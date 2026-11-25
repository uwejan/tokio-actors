#![allow(dead_code)]
//! An actor whose `pre_start` calls `ctx.ignore()` and returns `Ok(())` has
//! its start ignored, as Erlang/OTP's `init/1` returning `ignore`: no further
//! hook runs, the end is `Graceful`, and every reader of the start (a root
//! spawn, a supervised child, a restart, a strict start, a significant child)
//! answers it the same way.
//!
//! Every wait has a bound and orders by observation: the end is read from the
//! status plane (`wait_stopped`), a supervisor's view from a `SupervisorRef`
//! round trip, and a mailbox ping, which the supervisor serves after its
//! death plane, closes a row.

mod common;

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    AutoShutdown, ChildCounts, ChildEvent, DownReason, LinkExit, RestartStrategy, RestartType,
    SpawnError, StopReason, SupervisionAction, SupervisionConfig, SupervisionError, SupervisorRef,
    SupervisorRefError,
};
use tokio_actors::{ActorError, ActorHandle, ActorResult, ActorSystem};

/// A run index no run reaches: the actor never ignores.
const NEVER: usize = usize::MAX;

fn unique(base: &str) -> String {
    format!("ignore-{base}-{}", uuid::Uuid::new_v4())
}

/// How often each hook ran, shared by every incarnation built from one
/// factory, and the start of each `pre_start` for a test to wait on.
struct Counters {
    pre_start: AtomicUsize,
    on_started: AtomicUsize,
    handled: AtomicUsize,
    on_stopped: AtomicUsize,
    entered: Semaphore,
}

impl Counters {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pre_start: AtomicUsize::new(0),
            on_started: AtomicUsize::new(0),
            handled: AtomicUsize::new(0),
            on_stopped: AtomicUsize::new(0),
            entered: Semaphore::new(0),
        })
    }

    /// The hooks that ran, in the order `pre_start`, `on_started`, `handle`,
    /// `on_stopped`.
    fn hooks(&self) -> [usize; 4] {
        [
            self.pre_start.load(Ordering::SeqCst),
            self.on_started.load(Ordering::SeqCst),
            self.handled.load(Ordering::SeqCst),
            self.on_stopped.load(Ordering::SeqCst),
        ]
    }

    /// Returns once one more `pre_start` has begun than this call has seen.
    async fn next_start(&self) {
        timeout(Duration::from_secs(30), self.entered.acquire())
            .await
            .expect("a pre_start begins")
            .expect("the gate stays open")
            .forget();
    }
}

enum GhostMsg {
    /// Ends the incarnation with a failure.
    Crash,
}

/// An actor that ignores its start from the run numbered `ignore_from` on
/// (runs count from 0 across every incarnation of one `Counters`).
struct Ghost {
    counters: Arc<Counters>,
    ignore_from: usize,
    /// Linked in `pre_start`, before the start is ignored.
    link: Option<ActorHandle<Peer>>,
    /// The first `pre_start` waits here for a permit before it goes on.
    gate: Option<Arc<Semaphore>>,
}

impl Ghost {
    fn new(counters: &Arc<Counters>, ignore_from: usize) -> Self {
        Self {
            counters: counters.clone(),
            ignore_from,
            link: None,
            gate: None,
        }
    }
}

impl Actor for Ghost {
    type Message = GhostMsg;
    type Response = ();

    async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let run = self.counters.pre_start.fetch_add(1, Ordering::SeqCst);
        self.counters.entered.add_permits(1);
        if let Some(peer) = &self.link {
            ctx.link(peer).expect("the link is made");
        }
        if run == 0 {
            if let Some(gate) = &self.gate {
                gate.acquire().await.expect("the gate stays open").forget();
            }
        }
        if run >= self.ignore_from {
            ctx.ignore();
        }
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.counters.on_started.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, msg: GhostMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.counters.handled.fetch_add(1, Ordering::SeqCst);
        match msg {
            GhostMsg::Crash => Err(ActorError::user("told to crash")),
        }
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.counters.on_stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

enum PeerMsg {
    Exit(LinkExit),
}

/// Traps exits and forwards each one to the test.
struct Peer {
    exits: mpsc::UnboundedSender<LinkExit>,
}

impl Actor for Peer {
    type Message = PeerMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.trap_exit(PeerMsg::Exit);
        Ok(())
    }

    async fn handle(&mut self, msg: PeerMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let PeerMsg::Exit(exit) = msg;
        let _ = self.exits.send(exit);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// A root spawn
// ---------------------------------------------------------------------------

/// A root spawn of an actor that ignores its start answers
/// `SpawnError::Ignored` on every spawn path, whatever the options: only
/// `pre_start` ran, the name is free for the next spawn at once, and a
/// trapping actor linked in `pre_start` is told the end was `Graceful`. An
/// ignore read as a failed init (`Init`), as a cut start (`Killed`) or as a
/// start that went on (a later hook, a name held) fails here.
#[test]
fn an_ignored_root_start_runs_no_hook_and_answers_ignored() {
    #[derive(Clone, Copy, Debug)]
    enum Options {
        Plain,
        Named,
        StartTimeout,
    }

    for options in [Options::Plain, Options::Named, Options::StartTimeout] {
        common::run(2, async move {
            let row = format!("{options:?}");
            let sys = ActorSystem::create(unique("root-system")).unwrap();
            let (exits, mut peer_exits) = mpsc::unbounded_channel();
            let peer = Peer { exits }
                .spawn()
                .on_system(&sys)
                .await
                .expect("the peer starts");

            let counters = Counters::new();
            let name = matches!(options, Options::Named).then(|| unique("ghost"));
            let build = |counters: &Arc<Counters>, ignore_from: usize| {
                let mut ghost = Ghost::new(counters, ignore_from);
                ghost.link = Some(peer.clone());
                let mut spawn = ghost.spawn().on_system(&sys);
                if let Some(name) = &name {
                    spawn = spawn.named(name.clone());
                }
                if matches!(options, Options::StartTimeout) {
                    spawn = spawn.start_timeout(Duration::from_secs(20));
                }
                spawn
            };

            let started = timeout(Duration::from_secs(30), build(&counters, 0))
                .await
                .expect("the spawn answers");
            assert!(
                matches!(started, Err(SpawnError::Ignored)),
                "{row}: the spawn answers Ignored"
            );
            assert_eq!(counters.hooks(), [1, 0, 0, 0], "{row}: only pre_start ran");

            let exit = timeout(Duration::from_secs(30), peer_exits.recv())
                .await
                .expect("the trapping partner is told")
                .expect("the peer is alive");
            assert_eq!(
                exit.reason,
                DownReason::Stopped(StopReason::Graceful),
                "{row}: the end reads Graceful"
            );

            // The name, if any, is free at once; an unnamed spawn is the
            // control that nothing else was left behind.
            let again = Counters::new();
            let second = timeout(Duration::from_secs(30), build(&again, NEVER))
                .await
                .expect("the second spawn answers")
                .unwrap_or_else(|err| panic!("{row}: the second spawn failed: {err}"));
            assert_eq!(again.hooks()[..2], [1, 1], "{row}: the second one runs");
            second
                .stop(StopReason::Graceful)
                .await
                .expect("the second one stops");
        });
    }
}

// ---------------------------------------------------------------------------
// A supervisor and its children
// ---------------------------------------------------------------------------

enum Cmd {
    Spawn {
        name: String,
        restart: RestartType,
        awaited: bool,
        significant: bool,
    },
    /// A child built on its own counters, ignoring from run `ignore_from`.
    SpawnGhost {
        name: String,
        restart: RestartType,
        counters: Arc<Counters>,
        ignore_from: usize,
        gate: Option<Arc<Semaphore>>,
    },
    /// Parks the supervisor's handler until `release` fires, after telling
    /// the test it began.
    Hold {
        release: oneshot::Receiver<()>,
        entered: oneshot::Sender<()>,
    },
    Ping,
}

enum Reply {
    Spawned(Result<ActorHandle<Ghost>, SpawnError>),
    Pong,
}

/// A supervisor of `Ghost` children, built by the factory below, that
/// forwards every `on_child_stopped` to the test.
struct Boss {
    counters: Arc<Counters>,
    ignore_from: usize,
    events: mpsc::UnboundedSender<ChildEvent>,
    /// A permanent child spawned (not awaited) in `on_started`; its spawn's
    /// answer goes to the channel.
    at_start: Option<(String, mpsc::UnboundedSender<bool>)>,
}

impl Boss {
    async fn spawn_one(
        &self,
        ctx: &mut ActorContext<Self>,
        name: String,
        restart: RestartType,
        awaited: bool,
        significant: bool,
    ) -> Result<ActorHandle<Ghost>, SpawnError> {
        let counters = self.counters.clone();
        let ignore_from = self.ignore_from;
        let mut spawn = ctx
            .spawn_child(move || Ghost::new(&counters, ignore_from))
            .named(name)
            .restart_type(restart)
            .significant(significant);
        if awaited {
            spawn = spawn.await_init();
        }
        spawn.await
    }
}

impl Actor for Boss {
    type Message = Cmd;
    type Response = Reply;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some((name, answers)) = self.at_start.take() {
            let spawned = self
                .spawn_one(ctx, name, RestartType::Permanent, false, false)
                .await;
            let _ = answers.send(spawned.is_ok());
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        Ok(match msg {
            Cmd::Spawn {
                name,
                restart,
                awaited,
                significant,
            } => Reply::Spawned(
                self.spawn_one(ctx, name, restart, awaited, significant)
                    .await,
            ),
            Cmd::SpawnGhost {
                name,
                restart,
                counters,
                ignore_from,
                gate,
            } => {
                let spawned = ctx
                    .spawn_child(move || Ghost {
                        gate: gate.clone(),
                        ..Ghost::new(&counters, ignore_from)
                    })
                    .named(name)
                    .restart_type(restart)
                    .await;
                Reply::Spawned(spawned)
            }
            Cmd::Hold { release, entered } => {
                let _ = entered.send(());
                let _ = release.await;
                Reply::Pong
            }
            Cmd::Ping => Reply::Pong,
        })
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.events.send(event.clone());
        Ok(())
    }
}

struct Rig {
    sys: Arc<ActorSystem>,
    boss: ActorHandle<Boss>,
    sup: SupervisorRef<Boss>,
    counters: Arc<Counters>,
    events: mpsc::UnboundedReceiver<ChildEvent>,
}

impl Rig {
    async fn start(
        config: SupervisionConfig,
        ignore_from: usize,
        at_start: Option<(String, mpsc::UnboundedSender<bool>)>,
    ) -> Rig {
        let sys = ActorSystem::create(unique("system")).unwrap();
        let counters = Counters::new();
        let (events_tx, events) = mpsc::unbounded_channel();
        let boss = timeout(
            Duration::from_secs(30),
            Boss {
                counters: counters.clone(),
                ignore_from,
                events: events_tx,
                at_start,
            }
            .spawn()
            .on_system(&sys)
            .with_supervision(config),
        )
        .await
        .expect("the supervisor's start answers")
        .expect("the supervisor starts");
        let sup = SupervisorRef::of(&boss);
        Rig {
            sys,
            boss,
            sup,
            counters,
            events,
        }
    }

    /// Asks the supervisor to spawn a child through its own context.
    async fn spawn_in_handler(
        &self,
        name: &str,
        restart: RestartType,
        awaited: bool,
        significant: bool,
    ) -> Result<ActorHandle<Ghost>, SpawnError> {
        let reply = timeout(
            Duration::from_secs(30),
            self.boss.send(Cmd::Spawn {
                name: name.to_owned(),
                restart,
                awaited,
                significant,
            }),
        )
        .await
        .expect("the supervisor answers")
        .expect("the supervisor is alive");
        match reply {
            Reply::Spawned(spawned) => spawned,
            Reply::Pong => unreachable!("a spawn is answered with its outcome"),
        }
    }

    /// Asks the supervisor to spawn a `Ghost` on its own counters.
    async fn spawn_ghost(
        &self,
        name: &str,
        restart: RestartType,
        counters: &Arc<Counters>,
        ignore_from: usize,
        gate: Option<Arc<Semaphore>>,
    ) -> ActorHandle<Ghost> {
        let reply = timeout(
            Duration::from_secs(30),
            self.boss.send(Cmd::SpawnGhost {
                name: name.to_owned(),
                restart,
                counters: counters.clone(),
                ignore_from,
                gate,
            }),
        )
        .await
        .expect("the supervisor answers")
        .expect("the supervisor is alive");
        match reply {
            Reply::Spawned(spawned) => spawned.expect("the child is registered"),
            _ => unreachable!("a spawn is answered with its outcome"),
        }
    }

    /// Parks the supervisor's handler and returns the key that lets it go,
    /// once the handler has begun.
    async fn hold(&self) -> oneshot::Sender<()> {
        let (release, parked) = oneshot::channel();
        let (entered, held) = oneshot::channel();
        self.boss
            .notify(Cmd::Hold {
                release: parked,
                entered,
            })
            .await
            .expect("the hold is queued");
        timeout(Duration::from_secs(30), held)
            .await
            .expect("the supervisor reaches the hold")
            .expect("the hold began");
        release
    }

    /// The mailbox is served after the supervisor's death plane, so the
    /// answer means every end the supervisor was told of has been taken.
    async fn ping(&self) {
        let reply = timeout(Duration::from_secs(30), self.boss.send(Cmd::Ping))
            .await
            .expect("the supervisor answers")
            .expect("the supervisor is alive");
        assert!(matches!(reply, Reply::Pong));
    }

    /// Reads the supervisor's children until the expected state holds: a kept
    /// spec that is down and not pending a restart, or no spec at all.
    async fn settle(&self, kept: bool, row: &str) {
        timeout(Duration::from_secs(30), async {
            loop {
                let rows = self
                    .sup
                    .which_children()
                    .await
                    .unwrap_or_else(|err| panic!("{row}: which_children failed: {err}"));
                let done = if kept {
                    rows.len() == 1 && !rows[0].is_alive && !rows[0].restart_pending
                } else {
                    rows.is_empty()
                };
                if done {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{row}: the supervisor never reached the expected state"));
    }

    /// Reads the supervisor's children until each named child is in its
    /// expected state: `Some(alive)` is a spec of that liveness with no
    /// restart pending, `None` is no spec at all, and no other spec exists.
    async fn settle_rows(&self, expect: &[(&str, Option<bool>)], row: &str) {
        timeout(Duration::from_secs(30), async {
            loop {
                let rows = self
                    .sup
                    .which_children()
                    .await
                    .unwrap_or_else(|err| panic!("{row}: which_children failed: {err}"));
                let present = expect.iter().filter(|(_, state)| state.is_some()).count();
                let settled = rows.len() == present
                    && expect.iter().all(|(name, state)| {
                        let found = rows.iter().find(|info| info.id.as_str() == *name);
                        match state {
                            Some(alive) => {
                                found.is_some_and(|i| i.is_alive == *alive && !i.restart_pending)
                            }
                            None => found.is_none(),
                        }
                    });
                if settled {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{row}: the supervisor never reached the expected state"));
    }

    /// The supervisor's whole view of its one child (or of none).
    async fn assert_view(&self, id: &str, kept: bool, restart: RestartType, row: &str) {
        let children = self.sup.which_children().await.expect("which_children");
        let counts = self.sup.count_children().await.expect("count_children");
        let spec = self.sup.get_childspec(id).await;
        if kept {
            assert_eq!(children.len(), 1, "{row}: one row");
            let info = &children[0];
            assert_eq!(info.id.as_str(), id, "{row}");
            assert_eq!(info.restart_type, restart, "{row}");
            assert!(!info.is_alive, "{row}: down");
            assert!(!info.restart_pending, "{row}: no restart pending");
            assert_eq!(
                counts,
                ChildCounts {
                    specs: 1,
                    active: 0,
                    supervisors: 0,
                    workers: 1
                },
                "{row}"
            );
            let spec = spec.unwrap_or_else(|err| panic!("{row}: get_childspec failed: {err}"));
            assert_eq!(spec.restart_type, restart, "{row}");
            assert!(!spec.is_alive, "{row}");
        } else {
            assert!(children.is_empty(), "{row}: no row");
            assert_eq!(
                counts,
                ChildCounts {
                    specs: 0,
                    active: 0,
                    supervisors: 0,
                    workers: 0
                },
                "{row}"
            );
            match spec {
                Err(SupervisorRefError::Supervision(SupervisionError::ChildNotFound(missing))) => {
                    assert_eq!(missing.as_str(), id, "{row}");
                }
                other => panic!("{row}: get_childspec answered {other:?}"),
            }
        }
    }

    fn assert_no_report(&mut self, row: &str) {
        if let Ok(event) = self.events.try_recv() {
            panic!("{row}: on_child_stopped was called: {event:?}");
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Start {
    /// `spawn_child` in the supervisor's handler, not awaited.
    InHandler,
    /// `spawn_child(..).await_init()` in the supervisor's handler.
    InHandlerAwaited,
    /// `SupervisorRef::start_child` with `await_init`.
    ThroughRef,
}

/// A child that ignores its start is taken by the supervisor with no restart,
/// no charge against the budget (the budget allows none, so a charge would
/// end the supervisor), no `on_child_stopped` and no auto shutdown: its spec
/// is kept down, or removed for a `Temporary` child and under
/// `SimpleOneForOne`, and `which_children`, `count_children` and
/// `get_childspec` agree. An awaited start answers `Ignored` and has applied
/// that at once. For a kept spec, `restart_child` answers `Ok(())` and the
/// child is down again after its second ignore. A restart, a charge, a
/// report or a spec of the wrong kind fails here.
#[test]
fn an_ignored_child_is_kept_down_or_removed_without_a_charge_or_a_report() {
    let strategies = [RestartStrategy::OneForOne, RestartStrategy::SimpleOneForOne];
    let kinds = [
        RestartType::Permanent,
        RestartType::Transient,
        RestartType::Temporary,
    ];
    let starts = [Start::InHandler, Start::InHandlerAwaited, Start::ThroughRef];
    for strategy in strategies {
        for restart in kinds {
            for start in starts {
                common::run(2, async move {
                    let row = format!("{strategy:?} {restart:?} {start:?}");
                    let config = match strategy {
                        RestartStrategy::SimpleOneForOne => SupervisionConfig::simple_one_for_one(),
                        _ => SupervisionConfig::one_for_one(),
                    }
                    .max_restarts(0, Duration::from_secs(60));
                    let mut rig = Rig::start(config, 0, None).await;
                    let kept = matches!(strategy, RestartStrategy::OneForOne)
                        && !matches!(restart, RestartType::Temporary);
                    let name = unique("child");

                    match start {
                        Start::InHandler => {
                            let child = rig
                                .spawn_in_handler(&name, restart, false, false)
                                .await
                                .unwrap_or_else(|err| panic!("{row}: spawn failed: {err}"));
                            let reason = timeout(Duration::from_secs(30), child.wait_stopped())
                                .await
                                .expect("the child ends");
                            assert_eq!(reason, StopReason::Graceful, "{row}");
                        }
                        Start::InHandlerAwaited => {
                            let spawned = rig.spawn_in_handler(&name, restart, true, false).await;
                            assert!(
                                matches!(spawned, Err(SpawnError::Ignored)),
                                "{row}: await_init answers Ignored"
                            );
                        }
                        Start::ThroughRef => {
                            let counters = rig.counters.clone();
                            let configured = name.clone();
                            let started = timeout(
                                Duration::from_secs(30),
                                rig.sup.start_child(
                                    move || Ghost::new(&counters, 0),
                                    move |child| {
                                        child.named(configured).restart_type(restart).await_init()
                                    },
                                ),
                            )
                            .await
                            .expect("start_child answers");
                            assert!(
                                matches!(
                                    started,
                                    Err(SupervisorRefError::Spawn(SpawnError::Ignored))
                                ),
                                "{row}: start_child answers Spawn(Ignored)"
                            );
                        }
                    }
                    rig.counters.next_start().await;
                    rig.settle(kept, &row).await;
                    rig.ping().await;
                    rig.assert_view(&name, kept, restart, &row).await;
                    assert_eq!(rig.counters.hooks(), [1, 0, 0, 0], "{row}: only pre_start");
                    rig.assert_no_report(&row);

                    if kept {
                        timeout(
                            Duration::from_secs(30),
                            rig.sup.restart_child(name.as_str()),
                        )
                        .await
                        .expect("restart_child answers")
                        .unwrap_or_else(|err| panic!("{row}: restart_child failed: {err}"));
                        rig.counters.next_start().await;
                        rig.settle(true, &row).await;
                        rig.ping().await;
                        rig.assert_view(&name, true, restart, &row).await;
                        assert_eq!(
                            rig.counters.hooks(),
                            [2, 0, 0, 0],
                            "{row}: the second start ignored too"
                        );
                        rig.assert_no_report(&row);
                    }
                    // Still a live supervisor: it answers a call.
                    rig.sup
                        .count_children()
                        .await
                        .expect("the supervisor lives");
                });
            }
        }
    }
}

/// One group trigger and one ghost whose first start is ignored, with the
/// supervisor held while both end, in the order given. Returns the reports
/// the supervisor delivered, as `"<T or G> <reason> <action>"`.
async fn group_trial(
    strategy: RestartStrategy,
    ghost_restart: RestartType,
    ghost_first: bool,
    row: &str,
) -> Vec<String> {
    let config = match strategy {
        RestartStrategy::OneForAll => SupervisionConfig::one_for_all(),
        _ => SupervisionConfig::rest_for_one(),
    }
    .max_restarts(10, Duration::from_secs(60));
    let mut rig = Rig::start(config, NEVER, None).await;
    let (trigger_name, ghost_name) = (unique("trigger"), unique("ghost"));
    let (trigger_counters, ghost_counters) = (Counters::new(), Counters::new());
    let gate = Arc::new(Semaphore::new(0));
    let trigger = rig
        .spawn_ghost(
            &trigger_name,
            RestartType::Permanent,
            &trigger_counters,
            NEVER,
            None,
        )
        .await;
    let ghost = rig
        .spawn_ghost(
            &ghost_name,
            ghost_restart,
            &ghost_counters,
            0,
            Some(gate.clone()),
        )
        .await;
    trigger_counters.next_start().await;
    ghost_counters.next_start().await;
    let release = rig.hold().await;

    // The two ends happen while the supervisor takes neither.
    let end_ghost = async {
        gate.add_permits(1);
        let reason = timeout(Duration::from_secs(30), ghost.wait_stopped())
            .await
            .expect("the ghost ends");
        assert_eq!(reason, StopReason::Graceful, "{row}: the ghost's end");
    };
    let end_trigger = async {
        trigger
            .notify(GhostMsg::Crash)
            .await
            .expect("the crash is sent");
        timeout(Duration::from_secs(30), trigger.wait_stopped())
            .await
            .expect("the trigger ends");
    };
    if ghost_first {
        end_ghost.await;
        end_trigger.await;
    } else {
        end_trigger.await;
        end_ghost.await;
    }
    release.send(()).expect("the supervisor still holds");

    // The group restarts the trigger; a kept ghost is started again and
    // ignores again.
    trigger_counters.next_start().await;
    let kept = !matches!(ghost_restart, RestartType::Temporary);
    if kept {
        ghost_counters.next_start().await;
    }
    rig.settle_rows(
        &[
            (&trigger_name, Some(true)),
            (&ghost_name, kept.then_some(false)),
        ],
        row,
    )
    .await;
    rig.ping().await;
    rig.ping().await;
    let mut reports = Vec::new();
    while let Ok(event) = rig.events.try_recv() {
        let who = match event.child_id.as_str() {
            id if id == trigger_name => "T",
            id if id == ghost_name => "G",
            _ => "?",
        };
        reports.push(format!("{who} {:?} {:?}", event.reason, event.action));
    }
    reports
}

/// An ignored start does not disturb the supervisor in the three places it
/// meets one besides a first start. (a) A child that crashes once and whose
/// restart ignores: one `on_child_stopped` for the crash (`RestartInitiated`),
/// none for the ignored restart, and the child is down. (b) With a strict
/// start, an ignoring child spawned in `on_started` is not a failed start:
/// the supervisor's spawn answers `Ok`. (c) A significant child under
/// `AnySignificant` that ignores its start never shuts the supervisor down.
/// A second report, a failed strict start or an automatic shutdown fails
/// here.
#[test]
fn an_ignored_restart_and_an_ignored_strict_child_leave_the_supervisor_running() {
    // (a) crash on run 0, ignore on run 1.
    common::run(2, async {
        let config = SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(60));
        let mut rig = Rig::start(config, 1, None).await;
        let name = unique("restarted");
        let child = rig
            .spawn_in_handler(&name, RestartType::Permanent, false, false)
            .await
            .expect("the first start runs");
        rig.counters.next_start().await;
        let _ = timeout(Duration::from_secs(30), child.send(GhostMsg::Crash))
            .await
            .expect("the crash is answered");

        let event = timeout(Duration::from_secs(30), rig.events.recv())
            .await
            .expect("the crash is reported")
            .expect("the supervisor lives");
        assert_eq!(event.action, SupervisionAction::RestartInitiated);
        assert!(matches!(event.reason, StopReason::Failure(_)));

        rig.counters.next_start().await;
        rig.settle(true, "restart").await;
        rig.ping().await;
        rig.assert_view(&name, true, RestartType::Permanent, "restart")
            .await;
        // pre_start of both runs, on_started and the handler of the first,
        // and on_stopped of the first only.
        assert_eq!(
            rig.counters.hooks(),
            [2, 1, 1, 1],
            "the ignored run ran one hook"
        );
        rig.assert_no_report("restart");
    });

    // (d) an ignored restart inside a group restart: the chain goes on, the
    // ignoring member stays down with its spec, nothing is charged beyond the
    // one restart the first crash costs (the budget allows only that one), and
    // the reports are exactly those of the same group restart with no ignore:
    // the group's reports of the members it stopped, none for the ignored
    // start.
    for strategy in [RestartStrategy::OneForAll, RestartStrategy::RestForOne] {
        let mut reports = Vec::new();
        for middle_ignores in [false, true] {
            let out = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = out.clone();
            common::run(2, async move {
                let row = format!("{strategy:?} group, middle ignores: {middle_ignores}");
                let config = match strategy {
                    RestartStrategy::OneForAll => SupervisionConfig::one_for_all(),
                    _ => SupervisionConfig::rest_for_one(),
                }
                .max_restarts(1, Duration::from_secs(60));
                let mut rig = Rig::start(config, NEVER, None).await;
                let counters = [Counters::new(), Counters::new(), Counters::new()];
                let ignore_from = [NEVER, if middle_ignores { 1 } else { NEVER }, NEVER];
                let names = [unique("first"), unique("middle"), unique("last")];
                let mut first = None;
                for i in 0..3 {
                    let reply = timeout(
                        Duration::from_secs(30),
                        rig.boss.send(Cmd::SpawnGhost {
                            name: names[i].clone(),
                            restart: RestartType::Permanent,
                            counters: counters[i].clone(),
                            ignore_from: ignore_from[i],
                            gate: None,
                        }),
                    )
                    .await
                    .expect("the supervisor answers")
                    .expect("the supervisor is alive");
                    let Reply::Spawned(spawned) = reply else {
                        unreachable!("a spawn is answered with its outcome")
                    };
                    let handle = spawned.unwrap_or_else(|err| panic!("{row}: spawn failed: {err}"));
                    counters[i].next_start().await;
                    if i == 0 {
                        first = Some(handle);
                    }
                }
                let _ = timeout(
                    Duration::from_secs(30),
                    first.expect("spawned").send(GhostMsg::Crash),
                )
                .await
                .expect("the crash is answered");

                let event = timeout(Duration::from_secs(30), rig.events.recv())
                    .await
                    .expect("the crash is reported")
                    .expect("the supervisor lives");
                assert_eq!(event.child_id.as_str(), names[0], "{row}");
                assert_eq!(event.action, SupervisionAction::RestartInitiated, "{row}");
                assert!(matches!(event.reason, StopReason::Failure(_)), "{row}");

                // Every member's second pre_start has begun: the chain reached the
                // last one past the ignoring middle one.
                for c in &counters {
                    c.next_start().await;
                }
                rig.settle_rows(
                    &[
                        (&names[0], Some(true)),
                        (&names[1], Some(!middle_ignores)),
                        (&names[2], Some(true)),
                    ],
                    &row,
                )
                .await;
                rig.ping().await;

                // pre_start, on_started, handle, on_stopped of each member.
                assert_eq!(counters[0].hooks(), [2, 2, 1, 1], "{row}: first");
                let middle_started = if middle_ignores { 1 } else { 2 };
                assert_eq!(
                    counters[1].hooks(),
                    [2, middle_started, 0, 1],
                    "{row}: middle"
                );
                assert_eq!(counters[2].hooks(), [2, 2, 0, 1], "{row}: last");
                let counts = rig.sup.count_children().await.expect("count_children");
                let active = if middle_ignores { 2 } else { 3 };
                assert_eq!((counts.specs, counts.active), (3, active), "{row}");
                rig.ping().await;
                // The reports after the crash's, by member: only the group's stop
                // of a first incarnation, never the ignored start's own end.
                let mut seen = Vec::new();
                while let Ok(event) = rig.events.try_recv() {
                    let member = names
                        .iter()
                        .position(|name| event.child_id.as_str() == name)
                        .unwrap_or_else(|| {
                            panic!("{row}: a report of an unknown child: {event:?}")
                        });
                    assert!(
                        !matches!(event.reason, StopReason::Graceful),
                        "{row}: a report of the ignored start: {event:?}"
                    );
                    seen.push((member, event.reason, event.action));
                }
                *sink.lock().unwrap() = seen;
            });
            let seen = std::mem::take(&mut *out.lock().unwrap());
            reports.push(seen);
        }
        assert_eq!(
            reports[1], reports[0],
            "{strategy:?}: the reports of the group restart with an ignored member \
             are those of the same restart without one"
        );
    }

    // (e) an ignored first start that ends while the supervisor is busy, on
    // either side of a group trigger's end: whichever the supervisor takes
    // first, the ghost is reported on no route (not as a death, not as a
    // removed spec) and the two orders deliver the same reports.
    for (strategy, ghost_restart) in [
        (RestartStrategy::OneForAll, RestartType::Permanent),
        (RestartStrategy::RestForOne, RestartType::Permanent),
        (RestartStrategy::OneForAll, RestartType::Temporary),
    ] {
        common::run(2, async move {
            let row = format!("{strategy:?}, ghost {ghost_restart:?}");
            let mut by_order = Vec::new();
            for (order, ghost_first) in [("ghost first", true), ("trigger first", false)] {
                let row = format!("{row}, {order}");
                let reports = group_trial(strategy, ghost_restart, ghost_first, &row).await;
                assert!(
                    !reports.iter().any(|report| report.starts_with("G ")),
                    "{row}: the ignored start was reported: {reports:?}"
                );
                assert!(
                    reports.iter().any(|report| report.starts_with("T Failure")),
                    "{row}: the trigger's own report is missing: {reports:?}"
                );
                by_order.push(reports);
            }
            assert_eq!(
                by_order[0], by_order[1],
                "{row}: the reports depend on the order the ends were taken in"
            );
        });
    }

    // (f) a terminate_child that reaches the supervisor after the child
    // already ended ignored: it answers Ok, the spec stays down and nothing
    // is reported.
    common::run(2, async {
        let row = "terminate after an ignored end";
        let mut rig = Rig::start(
            SupervisionConfig::one_for_one().max_restarts(10, Duration::from_secs(60)),
            NEVER,
            None,
        )
        .await;
        let name = unique("ghost");
        let counters = Counters::new();
        let gate = Arc::new(Semaphore::new(0));
        let ghost = rig
            .spawn_ghost(
                &name,
                RestartType::Permanent,
                &counters,
                0,
                Some(gate.clone()),
            )
            .await;
        counters.next_start().await;
        let release = rig.hold().await;
        gate.add_permits(1);
        let reason = timeout(Duration::from_secs(30), ghost.wait_stopped())
            .await
            .expect("the ghost ends");
        assert_eq!(reason, StopReason::Graceful, "{row}");

        // The call is queued behind the held handler: its first poll sends
        // the request and waits for the answer.
        let sup = rig.sup.clone();
        let mut call = Box::pin(sup.terminate_child(name.as_str()));
        let first = std::future::poll_fn(|cx| std::task::Poll::Ready(call.as_mut().poll(cx))).await;
        assert!(
            first.is_pending(),
            "{row}: the call waits for the supervisor"
        );
        release.send(()).expect("the supervisor still holds");
        timeout(Duration::from_secs(30), call)
            .await
            .expect("the call is answered")
            .unwrap_or_else(|err| panic!("{row}: terminate_child failed: {err}"));

        rig.settle_rows(&[(&name, Some(false))], row).await;
        rig.ping().await;
        let spec = rig
            .sup
            .get_childspec(name.as_str())
            .await
            .unwrap_or_else(|err| panic!("{row}: get_childspec failed: {err}"));
        assert!(!spec.is_alive, "{row}: the spec is kept down");
        rig.assert_no_report(row);
    });

    // (b) strict start with an ignoring child spawned while starting.
    common::run(2, async {
        let name = unique("strict");
        let (answers, mut spawned) = mpsc::unbounded_channel();
        let config = SupervisionConfig::one_for_one()
            .max_restarts(0, Duration::from_secs(60))
            .with_strict_start();
        let mut rig = Rig::start(config, 0, Some((name.clone(), answers))).await;
        assert_eq!(
            timeout(Duration::from_secs(30), spawned.recv())
                .await
                .expect("the spawn answered"),
            Some(true),
            "the supervisor's own spawn_child answered Ok"
        );
        rig.counters.next_start().await;
        rig.settle(true, "strict").await;
        rig.ping().await;
        rig.assert_view(&name, true, RestartType::Permanent, "strict")
            .await;
        assert_eq!(rig.counters.hooks(), [1, 0, 0, 0], "strict: only pre_start");
        rig.assert_no_report("strict");
    });

    // (c) a significant child that ignores, under AnySignificant.
    common::run(2, async {
        let config = SupervisionConfig::one_for_one()
            .max_restarts(0, Duration::from_secs(60))
            .with_auto_shutdown(AutoShutdown::AnySignificant);
        let mut rig = Rig::start(config, 0, None).await;
        let name = unique("significant");
        let child = rig
            .spawn_in_handler(&name, RestartType::Transient, false, true)
            .await
            .expect("the spawn answers Ok");
        let reason = timeout(Duration::from_secs(30), child.wait_stopped())
            .await
            .expect("the child ends");
        assert_eq!(reason, StopReason::Graceful);
        rig.settle(true, "significant").await;
        rig.ping().await;
        rig.assert_view(&name, true, RestartType::Transient, "significant")
            .await;
        rig.assert_no_report("significant");
        assert_eq!(
            rig.counters.hooks(),
            [1, 0, 0, 0],
            "significant: only pre_start"
        );
    });
}
