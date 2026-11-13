#![allow(dead_code)]
//! `auto_shutdown` and `significant` children, as Erlang/OTP has them.
//!
//! A supervisor shuts itself down when a significant child ends in a way it
//! did not order and does not restart: a `Transient` child's normal exit or a
//! `Temporary` child's exit of any kind. Its own end is `ParentRequest`, after
//! the significant child's event reaches `on_child_stopped` as
//! `SupervisionAction::AutoShutdown` and the deaths still pending reach it as
//! `SupervisorStopping`. A stop the supervisor ordered never counts.
//!
//! Every wait is ordered by a handshake or a gate and bounded, so that a hang
//! fails the test.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, Notify, Semaphore};
use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorId, ActorResult, ActorSystem, AutoShutdown, ChildEvent,
    RestartType, SpawnError, StopReason, SupervisionAction, SupervisionConfig,
};

/// Bounds every wait, so a hang fails the test.
const BOUND: Duration = Duration::from_secs(20);

type Log = Arc<Mutex<Vec<String>>>;

fn note(log: &Log, line: impl Into<String>) {
    log.lock().unwrap().push(line.into());
}

fn lines(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

fn unique(base: &str) -> String {
    format!("{base}-{}", uuid::Uuid::new_v4())
}

enum Msg {
    /// The handler fails: the worker ends with `Failure`.
    Boom,
    Ping,
}

/// A worker that fails on `Boom`. The initialization of every incarnation
/// after the first waits at `restart_gate` when there is one.
struct Worker {
    incarnation: usize,
    restart_gate: Option<Arc<Semaphore>>,
    /// `on_stopped` waits for a permit when there is a gate.
    stop_gate: Option<Arc<Semaphore>>,
}

impl Actor for Worker {
    type Message = Msg;
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.incarnation > 0 {
            if let Some(gate) = &self.restart_gate {
                gate.acquire().await.expect("the gate stays open").forget();
            }
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Boom => Err(ActorError::user("boom")),
            Msg::Ping => Ok(()),
        }
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if let Some(gate) = &self.stop_gate {
            gate.acquire().await.expect("the gate stays open").forget();
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Kid {
    name: &'static str,
    restart: RestartType,
    significant: bool,
    restart_gate: Option<Arc<Semaphore>>,
    stop_gate: Option<Arc<Semaphore>>,
}

impl Kid {
    fn new(name: &'static str, restart: RestartType, significant: bool) -> Self {
        Kid {
            name,
            restart,
            significant,
            restart_gate: None,
            stop_gate: None,
        }
    }
}

enum Cmd {
    Ping,
    Terminate(&'static str),
    StopChild(&'static str),
    /// The non-blocking forms, answered when the call returns.
    BeginTerminate(&'static str),
    BeginStop(&'static str),
    Restart(&'static str),
    /// The names of the children whose restart is pending.
    Pending,
    /// Spawns one more child and answers `Spawned`.
    Spawn(Kid),
    Alive,
}

#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Pong,
    Done(Result<(), String>),
    Spawned(Result<(), SpawnError>),
    Names(Vec<String>),
}

/// A pause inside the delivery of one child's event: the boss tells the test
/// the event arrived and waits to be released.
#[derive(Clone)]
struct Hold {
    child: &'static str,
    seen: Arc<Notify>,
    release: Arc<Notify>,
}

struct Boss {
    kids: Vec<Kid>,
    builds: Arc<Mutex<HashMap<&'static str, Arc<AtomicUsize>>>>,
    log: Log,
    events: mpsc::UnboundedSender<ChildEvent>,
    handles: mpsc::UnboundedSender<(&'static str, ActorHandle<Worker>)>,
    hold: Option<Hold>,
}

impl Boss {
    fn counter(&self, name: &'static str) -> Arc<AtomicUsize> {
        self.builds.lock().unwrap().entry(name).or_default().clone()
    }

    async fn spawn_kid(
        &self,
        ctx: &mut ActorContext<Self>,
        kid: &Kid,
    ) -> Result<ActorHandle<Worker>, SpawnError> {
        let counter = self.counter(kid.name);
        let gate = kid.restart_gate.clone();
        let stop_gate = kid.stop_gate.clone();
        ctx.spawn_child(move || Worker {
            incarnation: counter.fetch_add(1, Ordering::SeqCst),
            restart_gate: gate.clone(),
            stop_gate: stop_gate.clone(),
        })
        .named(kid.name)
        .restart_type(kid.restart)
        .significant(kid.significant)
        .await
    }
}

impl Actor for Boss {
    type Message = Cmd;
    type Response = Reply;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for kid in self.kids.clone() {
            let handle = self.spawn_kid(ctx, &kid).await?;
            let _ = self.handles.send((kid.name, handle));
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        Ok(match msg {
            Cmd::Ping => Reply::Pong,
            Cmd::Alive => Reply::Pong,
            Cmd::Terminate(name) => Reply::Done(
                ctx.terminate_child(name)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
            ),
            Cmd::StopChild(name) => Reply::Done(
                ctx.stop_child(name)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
            ),
            Cmd::BeginTerminate(name) => Reply::Done(
                ctx.begin_terminate_child(name)
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
            ),
            Cmd::BeginStop(name) => Reply::Done(
                ctx.begin_stop_child(name)
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
            ),
            Cmd::Restart(name) => Reply::Done(ctx.restart_child(name).map_err(|e| e.to_string())),
            Cmd::Pending => Reply::Names(
                ctx.children()
                    .into_iter()
                    .filter(|c| c.restart_pending)
                    .filter_map(|c| c.name)
                    .collect(),
            ),
            Cmd::Spawn(kid) => {
                let spawned = self.spawn_kid(ctx, &kid).await;
                if let Ok(handle) = &spawned {
                    let _ = self.handles.send((kid.name, handle.clone()));
                }
                Reply::Spawned(spawned.map(|_| ()))
            }
        })
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        note(
            &self.log,
            format!("child:{}:{:?}", event.child_id, event.action),
        );
        let _ = self.events.send(event.clone());
        if let Some(hold) = &self.hold {
            if event.child_id == ActorId::from(hold.child) {
                hold.seen.notify_one();
                hold.release.notified().await;
            }
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        note(&self.log, format!("stopped:{reason:?}"));
        Ok(())
    }
}

struct Rig {
    sys: Arc<ActorSystem>,
    log: Log,
    builds: Arc<Mutex<HashMap<&'static str, Arc<AtomicUsize>>>>,
    events: mpsc::UnboundedReceiver<ChildEvent>,
    events_tx: mpsc::UnboundedSender<ChildEvent>,
    handles: mpsc::UnboundedReceiver<(&'static str, ActorHandle<Worker>)>,
    handles_tx: mpsc::UnboundedSender<(&'static str, ActorHandle<Worker>)>,
}

impl Rig {
    fn new() -> Self {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (handles_tx, handles) = mpsc::unbounded_channel();
        Rig {
            sys: ActorSystem::create(unique("auto")).unwrap(),
            log: Arc::default(),
            builds: Arc::default(),
            events,
            events_tx,
            handles,
            handles_tx,
        }
    }

    fn boss(&self, kids: Vec<Kid>, hold: Option<Hold>) -> Boss {
        Boss {
            kids,
            builds: self.builds.clone(),
            log: self.log.clone(),
            events: self.events_tx.clone(),
            handles: self.handles_tx.clone(),
            hold,
        }
    }

    async fn start(
        &self,
        kids: Vec<Kid>,
        config: SupervisionConfig,
        hold: Option<Hold>,
    ) -> Result<ActorHandle<Boss>, SpawnError> {
        timeout(
            BOUND,
            self.boss(kids, hold)
                .spawn()
                .named("boss")
                .on_system(&self.sys)
                .with_supervision(config),
        )
        .await
        .expect("the boss starts")
    }

    /// The handles of the children the boss spawned, by name.
    fn kids(&mut self) -> HashMap<&'static str, ActorHandle<Worker>> {
        let mut out = HashMap::new();
        while let Ok((name, handle)) = self.handles.try_recv() {
            out.insert(name, handle);
        }
        out
    }

    async fn event(&mut self) -> ChildEvent {
        timeout(BOUND, self.events.recv())
            .await
            .expect("an event arrives")
            .expect("the supervisor is alive")
    }

    fn builds(&self, name: &'static str) -> usize {
        self.builds
            .lock()
            .unwrap()
            .get(name)
            .map_or(0, |n| n.load(Ordering::SeqCst))
    }
}

async fn alive(boss: &ActorHandle<Boss>) -> bool {
    matches!(
        timeout(BOUND, boss.send(Cmd::Ping)).await,
        Ok(Ok(Reply::Pong))
    )
}

/// How a significant child's end goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exit {
    Clean,
    Abnormal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// The event is `AutoShutdown` and the supervisor ends `ParentRequest`.
    Shutdown,
    /// The child is restarted and the supervisor goes on.
    Restart,
    /// The child is removed or left down and the supervisor goes on.
    Stays,
}

async fn end_child(handle: &ActorHandle<Worker>, exit: Exit) {
    match exit {
        Exit::Clean => {
            handle.stop(StopReason::Graceful).await.unwrap();
        }
        Exit::Abnormal => {
            let _ = handle.send(Msg::Boom).await;
        }
    }
}

// ---------------------------------------------------------------------------
// The modes
// ---------------------------------------------------------------------------

/// One significant (or plain) child ends, and the mode decides: a `Transient`
/// child's clean exit and a `Temporary` child's every exit shut the
/// supervisor down under `AnySignificant` and `AllSignificant`; a `Transient`
/// child's abnormal exit is a restart; a child that is not significant never
/// counts; `Never` never shuts down.
#[test]
fn shutdown_by_mode_and_exit() {
    use AutoShutdown::{AllSignificant, AnySignificant, Never};
    use RestartType::{Temporary, Transient};
    let mut rows: Vec<(AutoShutdown, RestartType, Exit, bool, Expect)> = Vec::new();
    for mode in [AnySignificant, AllSignificant] {
        rows.push((mode, Transient, Exit::Clean, true, Expect::Shutdown));
        rows.push((mode, Transient, Exit::Abnormal, true, Expect::Restart));
        rows.push((mode, Temporary, Exit::Clean, true, Expect::Shutdown));
        rows.push((mode, Temporary, Exit::Abnormal, true, Expect::Shutdown));
        rows.push((mode, Transient, Exit::Clean, false, Expect::Stays));
    }
    rows.push((Never, Transient, Exit::Clean, false, Expect::Stays));
    rows.push((Never, Temporary, Exit::Abnormal, false, Expect::Stays));

    for (mode, restart, exit, significant, expect) in rows {
        let row = format!("{mode:?} {restart:?} {exit:?} significant={significant}");
        common::run(2, async move {
            let mut rig = Rig::new();
            let config = SupervisionConfig::one_for_one()
                .max_restarts(10, Duration::from_secs(60))
                .with_auto_shutdown(mode);
            let boss = rig
                .start(vec![Kid::new("s", restart, significant)], config, None)
                .await
                .expect("the boss starts");
            let kids = rig.kids();
            end_child(&kids["s"], exit).await;
            let event = rig.event().await;
            assert_eq!(event.child_id, ActorId::from("s"), "{row}");
            match expect {
                Expect::Shutdown => {
                    assert_eq!(event.action, SupervisionAction::AutoShutdown, "{row}");
                    let reason = timeout(BOUND, boss.wait_stopped())
                        .await
                        .expect("the supervisor shuts down");
                    assert_eq!(reason, StopReason::ParentRequest, "{row}");
                }
                Expect::Restart => {
                    assert_eq!(event.action, SupervisionAction::RestartInitiated, "{row}");
                    assert!(alive(&boss).await, "{row}");
                }
                Expect::Stays => {
                    assert_eq!(event.action, SupervisionAction::Removed, "{row}");
                    assert!(alive(&boss).await, "{row}");
                }
            }
        });
    }
}

/// `AllSignificant` shuts down on the end of the last active significant
/// child only: while another one runs, and while another one is being
/// restarted, an end does not count.
#[test]
fn all_significant_waits_for_the_last_active_one() {
    use RestartType::{Temporary, Transient};
    common::run(2, async {
        let mut rig = Rig::new();
        let gate = Arc::new(Semaphore::new(0));
        let mut a = Kid::new("a", Transient, true);
        a.restart_gate = Some(gate.clone());
        let b = Kid::new("b", Temporary, true);
        let c = Kid::new("c", Temporary, true);
        let config = SupervisionConfig::one_for_one()
            .max_restarts(10, Duration::from_secs(60))
            .with_auto_shutdown(AutoShutdown::AllSignificant);
        let boss = rig
            .start(vec![a, b, c], config, None)
            .await
            .expect("the boss starts");
        let kids = rig.kids();

        // c ends while a and b run: not the last.
        end_child(&kids["c"], Exit::Clean).await;
        let event = rig.event().await;
        assert_eq!(event.child_id, ActorId::from("c"));
        assert_eq!(event.action, SupervisionAction::Removed);
        assert!(alive(&boss).await, "a and b still run");

        // a fails and is restarted; its restart waits at the gate.
        end_child(&kids["a"], Exit::Abnormal).await;
        let event = rig.event().await;
        assert_eq!(event.child_id, ActorId::from("a"));
        assert_eq!(event.action, SupervisionAction::RestartInitiated);

        // b ends while a is being restarted: a counts as active.
        end_child(&kids["b"], Exit::Clean).await;
        let event = rig.event().await;
        assert_eq!(event.child_id, ActorId::from("b"));
        assert_eq!(event.action, SupervisionAction::Removed);
        assert!(alive(&boss).await, "a is being restarted");

        // a comes up and ends: the last one.
        gate.add_permits(1);
        timeout(BOUND, async {
            while rig.builds("a") < 2 || !alive_child(&rig.sys, "a").await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a runs again");
        let again = rig.sys.get::<Worker>("a").expect("a is registered");
        end_child(&again, Exit::Clean).await;
        let event = rig.event().await;
        assert_eq!(event.child_id, ActorId::from("a"));
        assert_eq!(event.action, SupervisionAction::AutoShutdown);
        let reason = timeout(BOUND, boss.wait_stopped())
            .await
            .expect("the supervisor shuts down");
        assert_eq!(reason, StopReason::ParentRequest);
    });
}

async fn alive_child(sys: &Arc<ActorSystem>, name: &str) -> bool {
    match sys.get::<Worker>(name) {
        Some(handle) => matches!(timeout(BOUND, handle.send(Msg::Ping)).await, Ok(Ok(()))),
        None => false,
    }
}

/// The order of the end: the significant child's event first, as
/// `AutoShutdown`; the deaths still pending next, as `SupervisorStopping`;
/// then `on_stopped` with `ParentRequest`.
#[test]
fn event_then_parent_request_then_the_pending_deaths() {
    use RestartType::Temporary;
    common::run(2, async {
        let mut rig = Rig::new();
        let hold = Hold {
            child: "s",
            seen: Arc::default(),
            release: Arc::default(),
        };
        let config = SupervisionConfig::one_for_one()
            .max_restarts(10, Duration::from_secs(60))
            .with_auto_shutdown(AutoShutdown::AnySignificant);
        let boss = rig
            .start(
                vec![
                    Kid::new("s", Temporary, true),
                    Kid::new("x", Temporary, false),
                ],
                config,
                Some(hold.clone()),
            )
            .await
            .expect("the boss starts");
        let kids = rig.kids();
        end_child(&kids["s"], Exit::Clean).await;
        // The event of s is being delivered; x ends meanwhile, so its death
        // is pending when the supervisor stops.
        timeout(BOUND, hold.seen.notified())
            .await
            .expect("the significant child's event arrives");
        end_child(&kids["x"], Exit::Clean).await;
        timeout(BOUND, kids["x"].wait_stopped())
            .await
            .expect("x ends");
        hold.release.notify_one();
        let reason = timeout(BOUND, boss.wait_stopped())
            .await
            .expect("the supervisor shuts down");
        assert_eq!(reason, StopReason::ParentRequest);
        assert_eq!(
            lines(&rig.log),
            [
                "child:s:AutoShutdown",
                "child:x:SupervisorStopping",
                "stopped:ParentRequest"
            ]
        );
    });
}

/// The three combinations Erlang/OTP refuses are refused before any factory
/// runs, each with its own text.
#[test]
fn refusals_name_the_otp_reason() {
    use RestartType::{Permanent, Transient};
    const POOL: &str = "auto_shutdown with simple_one_for_one";
    const NEVER: &str = "significant child under auto_shutdown never";
    const PERMANENT: &str = "significant child with restart permanent";

    // A pool with a mode other than Never is refused at its own spawn.
    for mode in [AutoShutdown::AnySignificant, AutoShutdown::AllSignificant] {
        common::run(2, async move {
            let rig = Rig::new();
            let config = SupervisionConfig::simple_one_for_one().with_auto_shutdown(mode);
            let started = rig.start(Vec::new(), config, None).await;
            assert_eq!(
                started.err(),
                Some(SpawnError::InvalidSupervision(POOL)),
                "{mode:?}"
            );
        });
    }
    common::run(2, async {
        let rig = Rig::new();
        rig.start(Vec::new(), SupervisionConfig::simple_one_for_one(), None)
            .await
            .expect("a pool with Never is fine");
    });

    // A significant child is refused under Never and when it is Permanent.
    let rows: [(AutoShutdown, RestartType, Option<&'static str>); 4] = [
        (AutoShutdown::Never, Transient, Some(NEVER)),
        (AutoShutdown::AnySignificant, Permanent, Some(PERMANENT)),
        (AutoShutdown::AllSignificant, Permanent, Some(PERMANENT)),
        (AutoShutdown::AnySignificant, Transient, None),
    ];
    for (mode, restart, refused) in rows {
        common::run(2, async move {
            let rig = Rig::new();
            let boss = rig
                .start(
                    Vec::new(),
                    SupervisionConfig::one_for_one().with_auto_shutdown(mode),
                    None,
                )
                .await
                .expect("the boss starts");
            let reply = boss
                .send(Cmd::Spawn(Kid::new("late", restart, true)))
                .await
                .unwrap();
            match refused {
                Some(text) => {
                    assert_eq!(
                        reply,
                        Reply::Spawned(Err(SpawnError::InvalidSupervision(text))),
                        "{mode:?} {restart:?}"
                    );
                    assert_eq!(
                        rig.builds("late"),
                        0,
                        "{mode:?} {restart:?}: no factory ran"
                    );
                }
                None => assert_eq!(reply, Reply::Spawned(Ok(())), "{mode:?} {restart:?}"),
            }
        });
    }
}

/// A stop begun without waiting is still a stop the supervisor ordered: a
/// significant child it ends with `begin_terminate_child` or `begin_stop_child`
/// never shuts the supervisor down, whatever the child's restart type. The
/// order is not remembered past that end: a `Transient` child started again
/// that then ends cleanly on its own does shut it down.
#[test]
fn begun_stops_never_count_and_a_later_clean_end_does() {
    use RestartType::{Temporary, Transient};
    let rows: [(&str, RestartType, bool); 4] = [
        ("terminate", Transient, true),
        ("stop", Transient, true),
        ("terminate", Temporary, false),
        ("stop", Temporary, false),
    ];
    for (op, restart, then_ends_alone) in rows {
        let row = format!("begin_{op}_child of a {restart:?} significant child");
        common::run(2, async move {
            let mut rig = Rig::new();
            let config = SupervisionConfig::one_for_one()
                .max_restarts(10, Duration::from_secs(60))
                .with_auto_shutdown(AutoShutdown::AnySignificant);
            let boss = rig
                .start(vec![Kid::new("k", restart, true)], config, None)
                .await
                .expect("the boss starts");
            let _ = rig.kids();
            let cmd = match op {
                "terminate" => Cmd::BeginTerminate("k"),
                _ => Cmd::BeginStop("k"),
            };
            let reply = timeout(BOUND, boss.send(cmd)).await.unwrap().unwrap();
            assert_eq!(reply, Reply::Done(Ok(())), "{row}");
            let event = rig.event().await;
            assert_eq!(event.child_id, ActorId::from("k"), "{row}");
            assert_eq!(event.action, SupervisionAction::Removed, "{row}");
            assert!(alive(&boss).await, "{row}: an ordered end is no shutdown");
            if !then_ends_alone {
                return;
            }
            let reply = timeout(BOUND, boss.send(Cmd::Restart("k")))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reply, Reply::Done(Ok(())), "{row}: the restart");
            let again = timeout(BOUND, async {
                loop {
                    if rig.builds("k") == 2 {
                        if let Some(h) = rig.sys.get::<Worker>("k") {
                            if matches!(h.send(Msg::Ping).await, Ok(())) {
                                break h;
                            }
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{row}: the child runs again"));
            end_child(&again, Exit::Clean).await;
            let event = rig.event().await;
            assert_eq!(event.action, SupervisionAction::AutoShutdown, "{row}");
            let reason = timeout(BOUND, boss.wait_stopped())
                .await
                .expect("the supervisor shuts down");
            assert_eq!(reason, StopReason::ParentRequest, "{row}");
        });
    }
}

/// An end beside a child that is going away by order, or one whose restart is
/// still owed, is judged by what follows the other child's end: a start counts
/// it active, an ordered end with no start does not.
///
/// Row 1: a significant child being terminated is inactive, so the other
/// significant child's end is the last active one's. Row 2: the same for a
/// `Transient` child begun to stop, which stays down after a bounce. Row 3:
/// a child whose crash is waiting behind a pending group restart is still
/// active, so the end evaluated before it is not the last.
#[test]
fn a_child_counts_as_active_while_a_start_follows_its_end() {
    use RestartType::{Permanent, Temporary, Transient};
    // (how the first child is stopped, its stop is begun)
    for (begin, label) in [
        (Cmd::BeginTerminate("a"), "terminate"),
        (Cmd::BeginStop("a"), "stop of a transient child"),
    ] {
        common::run(2, async move {
            let mut rig = Rig::new();
            let gate = Arc::new(Semaphore::new(0));
            let mut a = Kid::new("a", Transient, true);
            a.stop_gate = Some(gate.clone());
            let b = Kid::new("b", Temporary, true);
            let config = SupervisionConfig::one_for_one()
                .max_restarts(10, Duration::from_secs(60))
                .with_auto_shutdown(AutoShutdown::AllSignificant);
            let boss = rig
                .start(vec![a, b], config, None)
                .await
                .expect("the boss starts");
            let kids = rig.kids();
            assert_eq!(
                boss.send(begin).await.unwrap(),
                Reply::Done(Ok(())),
                "{label}"
            );
            // a's `on_stopped` is held: the ordered stop is in progress.
            end_child(&kids["b"], Exit::Clean).await;
            let event = rig.event().await;
            assert_eq!(event.child_id, ActorId::from("b"), "{label}");
            assert_eq!(event.action, SupervisionAction::AutoShutdown, "{label}");
            gate.add_permits(1);
            let reason = timeout(BOUND, boss.wait_stopped())
                .await
                .expect("the supervisor shuts down");
            assert_eq!(reason, StopReason::ParentRequest, "{label}");
        });
    }
    common::run(2, async {
        let mut rig = Rig::new();
        let gate = Arc::new(Semaphore::new(0));
        let c = Kid::new("c", Transient, true);
        let d = Kid::new("d", Temporary, true);
        let mut f = Kid::new("f", Permanent, false);
        f.restart_gate = Some(gate.clone());
        let config = SupervisionConfig::rest_for_one()
            .max_restarts(10, Duration::from_secs(60))
            .with_auto_shutdown(AutoShutdown::AllSignificant);
        let boss = rig
            .start(vec![c, d, f], config, None)
            .await
            .expect("the boss starts");
        let kids = rig.kids();
        // f fails: its group restart is pending, held at f's restart gate.
        end_child(&kids["f"], Exit::Abnormal).await;
        let event = rig.event().await;
        assert_eq!(event.child_id, ActorId::from("f"));
        // d ends first, then c crashes: both wait behind the group, c's
        // restart decision still owed when d's end is evaluated.
        end_child(&kids["d"], Exit::Clean).await;
        end_child(&kids["c"], Exit::Abnormal).await;
        timeout(BOUND, async {
            loop {
                let Ok(Reply::Names(names)) = boss.send(Cmd::Pending).await else {
                    panic!("the supervisor answers");
                };
                if names.iter().any(|n| n == "c") && names.iter().any(|n| n == "d") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both ends wait behind the group");
        gate.add_permits(1);
        timeout(BOUND, async {
            while rig.builds("f") < 2 || rig.builds("c") < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("f and c start again");
        assert!(
            alive(&boss).await,
            "d's end shut down a supervisor whose c was owed a restart"
        );
        assert!(
            !lines(&rig.log).iter().any(|l| l.ends_with(":AutoShutdown")),
            "no event shut down: {:?}",
            lines(&rig.log)
        );
    });
}
