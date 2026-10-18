//! A child whose runner failed: no public call can fail a runner, so these
//! tests stand a failing task in for the runner of a real child inside a real
//! supervisor. The stand-in holds exactly what the runner holds (the child's
//! `JoinHandle`, its link guard and its slot) and panics on command, so the
//! supervisor sees what it sees when a runner fails: the slot's cell closes
//! without a fate and the guard fires, while the child's task goes on.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{sleep, timeout, Instant};

use crate::actor::context::ActorContext;
use crate::actor::runtime::{spawn_actor, ActorConfig};
use crate::actor::supervision::{
    rules, slot_pair, slot_pair_for, ChildLifecycle, ChildLinkGuard, ChildSpec, ChildState,
    DownPending, GroupPhase, GroupRestart, Origin, OwnStop, SupervisionConfig, TaskPlane,
    ABORT_SETTLE, DEFAULT_KILL_GRACE,
};
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult, SupervisionError};
use crate::system::read_status;
use crate::types::{
    ActorId, ActorStatus, ChildEvent, ChildStoppedInternal, RestartType, Shutdown, StatusValue,
    StopReason, SupervisionAction,
};

static UNIQ: AtomicUsize = AtomicUsize::new(0);

fn uname(base: &str) -> String {
    format!(
        "lost-runner-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// What the tests observe of the child.
#[derive(Default)]
struct Probe {
    pre_stops: AtomicUsize,
    ended: AtomicBool,
    /// While set, a turn blocks its thread instead of awaiting: for 1500 ms,
    /// or for `stuck_ms` when that is set.
    stuck: AtomicBool,
    /// The length of a stuck turn in milliseconds; zero reads as 1500.
    stuck_ms: std::sync::atomic::AtomicU64,
    /// Set by the test to end a stuck turn, or a blocking `on_stopped` of a
    /// supervisor child, before its length.
    unstick: AtomicBool,
    /// How long dropping the child value takes, in milliseconds.
    drop_ms: std::sync::atomic::AtomicU64,
    /// Set when a hung restart attempt saw the stop order written to its slot.
    ordered: AtomicBool,
    /// With `stuck`: after the blocked turn the child awaits for ever.
    then_hangs: AtomicBool,
    /// The runner stand-in writes the child's verdict before it fails.
    verdict_first: AtomicBool,
    /// The child's status plane, as its supervisor's slot holds it.
    plane: Mutex<Option<tokio::sync::watch::Receiver<StatusValue>>>,
    /// Whether the plane read `Stopped` or closed when the child's value was
    /// dropped.
    saw_the_end_at_drop: AtomicBool,
    /// While set, the child accepts a request to stop instead of vetoing it.
    accepts: AtomicBool,
    /// Set when the worker of a supervisor child finished its `on_stopped`.
    worker_done: AtomicBool,
    /// Set at the top of each turn the child handles.
    in_turn: AtomicBool,
}

/// A child that vetoes every request to stop and spends `TURN` inside each
/// message it handles.
struct Vetoer {
    probe: Arc<Probe>,
}

const TURN: Duration = Duration::from_millis(400);

/// The longest any timed outcome of the multi-threaded tests below may take.
/// A tighter limit would fail on a host that stalls its threads; the exact
/// timing is pinned on the paused clock, where an outcome lands on its
/// instant.
const HANG_BOUND: Duration = Duration::from_secs(10);

/// The stuck turn of the tests that time a verdict on a thread that never
/// yields: it holds its thread far past the verdict and the slack, and the
/// test ends it once its checks are done.
const LONG_STUCK: Duration = Duration::from_secs(15);

/// How far past its exact timing a verdict on a `LONG_STUCK` turn may land on
/// a real-time runtime: loose enough for a host that stalls its threads, and
/// far short of the turn a verdict that waited for its end would come with.
const STALL_SLACK: Duration = Duration::from_secs(3);

/// Holds the calling thread for `time`, or until `probe.unstick` is set.
fn block_unless_unstuck(probe: &Probe, time: Duration) {
    let until = std::time::Instant::now() + time;
    while !probe.unstick.load(Ordering::SeqCst) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Sets `probe.unstick` when dropped, so a stuck turn ends however the test
/// ends.
struct Unsticks(Arc<Probe>);

impl Drop for Unsticks {
    fn drop(&mut self) {
        self.0.unstick.store(true, Ordering::SeqCst);
    }
}

/// Makes the next stuck turn `LONG_STUCK` long; the returned guard ends it.
fn stuck_long(rig: &Rig) -> Unsticks {
    rig.probe
        .stuck_ms
        .store(LONG_STUCK.as_millis() as u64, Ordering::SeqCst);
    Unsticks(rig.probe.clone())
}

impl Actor for Vetoer {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.probe.in_turn.store(true, Ordering::SeqCst);
        if self.probe.stuck.load(Ordering::SeqCst) {
            let ms = match self.probe.stuck_ms.load(Ordering::SeqCst) {
                0 => 1500,
                ms => ms,
            };
            block_unless_unstuck(&self.probe, Duration::from_millis(ms));
            if self.probe.then_hangs.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
        } else {
            sleep(TURN).await;
        }
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.probe.pre_stops.fetch_add(1, Ordering::SeqCst);
        self.probe.accepts.load(Ordering::SeqCst)
    }
}

impl Drop for Vetoer {
    fn drop(&mut self) {
        if let Some(plane) = self.probe.plane.lock().unwrap().as_ref() {
            if read_status(plane) == ActorStatus::Stopped {
                self.probe.saw_the_end_at_drop.store(true, Ordering::SeqCst);
            }
        }
        let ms = self.probe.drop_ms.load(Ordering::SeqCst);
        if ms > 0 {
            let nap = || std::thread::sleep(Duration::from_millis(ms));
            // On a multi-thread runtime the dropping worker hands its queue to
            // another thread first, so the tasks its drop woke are not parked
            // behind the sleep.
            match tokio::runtime::Handle::try_current() {
                Ok(rt) if rt.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                    tokio::task::block_in_place(nap)
                }
                _ => nap(),
            }
        }
        self.probe.ended.store(true, Ordering::SeqCst);
    }
}

/// How a supervisor child spends its own `on_stopped`.
#[derive(Clone, Copy)]
enum OwnStopMode {
    /// It returns at once.
    Quick,
    /// It awaits this long, yielding.
    Sleep(Duration),
    /// It blocks its thread for this long, or until the probe's `unstick`.
    Block(Duration),
}

/// What a supervisor child and its one worker do.
struct Shape {
    /// The supervisor child's `pre_stop` vetoes every request.
    vetoes: bool,
    own_stop: OwnStopMode,
    /// The worker the child supervises: how long its `on_stopped` takes, its
    /// policy, and whether its `pre_stop` vetoes every request.
    worker: Option<(Duration, Shutdown, bool)>,
}

/// A supervisor child: it supervises one worker, and ends as `Shape` says.
struct SubSup {
    shape: Arc<Shape>,
    probe: Arc<Probe>,
}

struct SlowWorker {
    stop: Duration,
    vetoes: bool,
    probe: Arc<Probe>,
}

impl Actor for SlowWorker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        !self.vetoes
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        sleep(self.stop).await;
        self.probe.worker_done.store(true, Ordering::SeqCst);
        Ok(())
    }
}

impl Actor for SubSup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some((stop, shutdown, vetoes)) = self.shape.worker {
            let probe = self.probe.clone();
            ctx.spawn_child(move || SlowWorker {
                stop,
                vetoes,
                probe: probe.clone(),
            })
            .named(uname("worker"))
            .shutdown(shutdown)
            .await?;
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.probe.pre_stops.fetch_add(1, Ordering::SeqCst);
        !self.shape.vetoes
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        match self.shape.own_stop {
            OwnStopMode::Quick => {}
            OwnStopMode::Sleep(time) => sleep(time).await,
            OwnStopMode::Block(time) => block_unless_unstuck(&self.probe, time),
        }
        Ok(())
    }
}

impl Drop for SubSup {
    fn drop(&mut self) {
        self.probe.ended.store(true, Ordering::SeqCst);
    }
}

/// What the supervisor saw inside each `on_child_stopped`.
struct Report {
    event: ChildEvent,
    task_error: Option<ActorError>,
    last_error_clear: bool,
}

struct Sup {
    probe: Arc<Probe>,
    fail: Arc<Notify>,
    reports: Arc<Mutex<Vec<Report>>>,
    /// The reasons the supervisor was stopped with.
    stopped: Arc<Mutex<Vec<StopReason>>>,
    /// While set, the next delivery of a restart returns an error.
    fail_delivery: Arc<AtomicBool>,
}

#[derive(Clone)]
enum Cmd {
    /// Registers a vetoing child, with this kill grace, whose runner is the
    /// failing stand-in.
    AdoptWith(String, Duration),
    /// Registers a vetoing worker, with this policy and kill grace, whose
    /// runner is the failing stand-in.
    AdoptWorker(String, Shutdown, Duration),
    /// Registers a child that is itself a supervisor, with this policy and
    /// kill grace and this behavior, whose runner is the failing stand-in.
    AdoptSupervisor(String, Shutdown, Duration, Arc<Shape>),
    /// Fails the child's runner, waits until `children()` reads the child as
    /// not alive, and then calls `terminate_child` (or `stop_child`) in the
    /// same handler: the child's task ended and its death is not yet handled.
    EndThenCall(String, bool),
    /// `stop_child`, awaited.
    Stop(String),
    /// `terminate_child`, awaited.
    Terminate(String),
    /// Fails the child's runner, waits until `children()` reads the child as
    /// not alive, and checks what the supervisor then says of it. The run
    /// loop cannot route the runner's failure or the replacement report while
    /// this handler runs, so the checks see the child's ended task before its
    /// death is handled.
    Inspect(String),
    /// Makes the child a member the pending group restart waits for.
    PendGroup(String),
    /// `terminate_child`, awaited, and whether the child carries the
    /// verdict when the call returns.
    TerminateThenVerdict(String),
    /// Registers a child whose restart attempt is the failing stand-in.
    AdoptAttempt(String),
    /// Fails the stand-in attempt, then stops the supervisor.
    FailAttemptThenStop,
    /// As `AdoptAttempt`, with an attempt that never ends.
    AdoptHungAttempt(String),
    /// Fails the stand-in attempt only.
    FailOnly,
    /// Waits until a child value dropped, then stops the supervisor.
    WaitEndedThenStop,
    /// Registers three members of a group whose ends are all owed an
    /// evaluation: the first one's end is on its way to the loop, the other
    /// two wait among the deferred triggers, in order.
    OweGroup(String, String, String),
}

impl Sup {
    /// Registers `child` as a Temporary child of this supervisor, spawned
    /// with this `(policy, kill grace, is a supervisor)`, whose runner is a
    /// stand-in that owns the child's task, its guard and its slot and fails
    /// when told to.
    fn adopt<C: Actor>(
        &self,
        ctx: &mut ActorContext<Self>,
        name: String,
        child: C,
        config: ActorConfig,
        (shutdown, grace, is_supervisor): (Shutdown, Duration, bool),
    ) -> Result<(), ActorError> {
        let id = ActorId::from(name.clone());
        let system = ctx.resolved_system();
        let (handle, join, subtree) = spawn_actor(
            id.clone(),
            child,
            config,
            Some(name.clone()),
            Some(system.clone()),
            None,
            false,
            grace,
            None,
        )
        .map_err(ActorError::Spawn)?;
        let reaper = system.reaper_handle(ctx.runtime_handle());
        let guard = ChildLinkGuard::new(
            handle.stop_lane(),
            join.abort_handle(),
            reaper,
            rules::inherited_stop(is_supervisor, shutdown, grace),
            system,
            ctx.runtime_handle().clone(),
        );
        let verdicts = ctx
            .supervision_ref()
            .expect("a supervisor")
            .verdicts
            .clone();
        *self.probe.plane.lock().unwrap() = Some(handle.status_rx());
        let incarnation = ctx.resolved_system().next_incarnation();
        let (ctl, io) = slot_pair_for(verdicts, Some((id.clone(), incarnation)));
        io.publish(
            handle.stop_lane(),
            handle.status_rx(),
            OwnStop {
                is_supervisor,
                shutdown,
                kill_grace: grace,
            },
            Some(subtree.clone()),
        );
        let fail = self.fail.clone();
        let probe = self.probe.clone();
        let sup = ctx.supervision_mut().expect("a supervisor");
        // The stand-in for the runner: it owns the child's task, its
        // guard and its slot, and fails when told to.
        let task = sup.death_set.spawn({
            let id = id.clone();
            async move {
                let _join = join;
                let _guard = guard;
                let _io = io;
                fail.notified().await;
                if probe.verdict_first.load(Ordering::SeqCst) {
                    _io.write_verdict();
                }
                if std::hint::black_box(true) {
                    panic!("runner failed");
                }
                (id, StopReason::Graceful, incarnation)
            }
        });
        sup.task_owner
            .insert(task.id(), (id.clone(), incarnation, TaskPlane::Death));
        let predecessor = ctl.predecessor(subtree);
        sup.registry.register(ChildState {
            id,
            name: Some(name),
            spec: ChildSpec {
                restart_type: RestartType::Temporary,
                shutdown,
                start_timeout: None,
                kill_grace: grace,
                is_supervisor,
            },
            ctl,
            lifecycle: ChildLifecycle::Running(incarnation),
            origin: Origin::Initial,
            predecessor: Some(predecessor),
        });
        Ok(())
    }
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
            Cmd::AdoptWith(name, grace) => {
                let vetoer = Vetoer {
                    probe: self.probe.clone(),
                };
                self.adopt(
                    ctx,
                    name,
                    vetoer,
                    ActorConfig::default(),
                    (Shutdown::Timeout(Duration::from_secs(5)), grace, false),
                )?;
                Ok(Ok(()))
            }
            Cmd::AdoptAttempt(_) | Cmd::AdoptHungAttempt(_) => {
                let (name, hung) = match cmd {
                    Cmd::AdoptAttempt(name) => (name, false),
                    Cmd::AdoptHungAttempt(name) => (name, true),
                    _ => unreachable!(),
                };
                let id = ActorId::from(name.clone());
                let system = ctx.resolved_system();
                let (handle, join, subtree) = spawn_actor(
                    id.clone(),
                    Vetoer {
                        probe: self.probe.clone(),
                    },
                    ActorConfig::default(),
                    Some(name.clone()),
                    Some(system.clone()),
                    None,
                    false,
                    DEFAULT_KILL_GRACE,
                    None,
                )
                .map_err(ActorError::Spawn)?;
                let reaper = system.reaper_handle(ctx.runtime_handle());
                let guard = ChildLinkGuard::new(
                    handle.stop_lane(),
                    join.abort_handle(),
                    reaper,
                    rules::InheritedStop::Kill {
                        grace: DEFAULT_KILL_GRACE,
                    },
                    system,
                    ctx.runtime_handle().clone(),
                );
                let verdicts = ctx
                    .supervision_ref()
                    .expect("a supervisor")
                    .verdicts
                    .clone();
                let (ctl, io) = slot_pair(verdicts);
                io.publish(
                    handle.stop_lane(),
                    handle.status_rx(),
                    OwnStop::worker(DEFAULT_KILL_GRACE),
                    Some(subtree),
                );
                let incarnation = ctx.resolved_system().next_incarnation();
                let fail = if hung {
                    Arc::new(Notify::new())
                } else {
                    self.fail.clone()
                };
                let sup = ctx.supervision_mut().expect("a supervisor");
                let task = sup.restart_set.spawn({
                    let id = id.clone();
                    let probe = self.probe.clone();
                    async move {
                        let _join = join;
                        let _guard = guard;
                        if hung {
                            // An attempt whose initialization never ends: it
                            // answers a stop order by cancelling.
                            while io.order().is_none() {
                                sleep(Duration::from_millis(5)).await;
                            }
                            probe.ordered.store(true, Ordering::SeqCst);
                            io.mark_cancelled();
                            return crate::actor::supervision::RestartOutcome::Cancelled {
                                child_id: id,
                                incarnation,
                                drop_panic: None,
                            };
                        }
                        let _io = io;
                        fail.notified().await;
                        if std::hint::black_box(true) {
                            panic!("attempt failed");
                        }
                        crate::actor::supervision::RestartOutcome::Cancelled {
                            child_id: id,
                            incarnation,
                            drop_panic: None,
                        }
                    }
                });
                sup.task_owner
                    .insert(task.id(), (id.clone(), incarnation, TaskPlane::Restart));
                sup.registry.register(ChildState {
                    id,
                    name: Some(name),
                    spec: ChildSpec {
                        restart_type: RestartType::Temporary,
                        shutdown: Shutdown::Timeout(Duration::from_millis(if hung {
                            300
                        } else {
                            5000
                        })),
                        kill_grace: DEFAULT_KILL_GRACE,
                        start_timeout: None,
                        is_supervisor: false,
                    },
                    ctl,
                    lifecycle: ChildLifecycle::Restarting {
                        incarnation: 0,
                        next: incarnation,
                    },
                    origin: Origin::Initial,
                    predecessor: None,
                });
                Ok(Ok(()))
            }
            Cmd::OweGroup(first, second, third) => {
                let verdicts = ctx
                    .supervision_ref()
                    .expect("a supervisor")
                    .verdicts
                    .clone();
                let sup = ctx.supervision_mut().expect("a supervisor");
                for name in [&first, &second, &third] {
                    let (ctl, _io) = slot_pair(verdicts.clone());
                    let id = ActorId::from(name.clone());
                    sup.registry.register(ChildState {
                        id,
                        name: Some(name.clone()),
                        spec: ChildSpec {
                            restart_type: RestartType::Permanent,
                            shutdown: Shutdown::Timeout(Duration::from_secs(5)),
                            kill_grace: DEFAULT_KILL_GRACE,
                            start_timeout: None,
                            is_supervisor: false,
                        },
                        ctl,
                        lifecycle: ChildLifecycle::Down {
                            incarnation: 1,
                            pending: DownPending::Evaluate,
                        },
                        origin: Origin::Initial,
                        predecessor: None,
                    });
                }
                let crash = StopReason::Failure(ActorError::user("crashed"));
                for name in [&second, &third] {
                    sup.queued_triggers.push_back(ChildStoppedInternal {
                        child_id: ActorId::from(name.clone()),
                        reason: crash.clone(),
                        incarnation: 1,
                    });
                }
                let id = ActorId::from(first);
                let task = sup.death_set.spawn({
                    let id = id.clone();
                    async move { (id, crash, 1) }
                });
                sup.task_owner.insert(task.id(), (id, 1, TaskPlane::Death));
                Ok(Ok(()))
            }
            Cmd::FailOnly => {
                self.fail.notify_one();
                Ok(Ok(()))
            }
            Cmd::WaitEndedThenStop => {
                while !self.probe.ended.load(Ordering::SeqCst) {
                    sleep(Duration::from_millis(5)).await;
                }
                sleep(Duration::from_millis(20)).await;
                let _ = ctx.self_handle().stop(StopReason::Graceful).await;
                Ok(Ok(()))
            }
            Cmd::FailAttemptThenStop => {
                self.fail.notify_one();
                sleep(Duration::from_millis(20)).await;
                let _ = ctx.self_handle().stop(StopReason::Graceful).await;
                Ok(Ok(()))
            }
            Cmd::AdoptWorker(name, shutdown, grace) => {
                let vetoer = Vetoer {
                    probe: self.probe.clone(),
                };
                self.adopt(
                    ctx,
                    name,
                    vetoer,
                    ActorConfig::default(),
                    (shutdown, grace, false),
                )?;
                Ok(Ok(()))
            }
            Cmd::AdoptSupervisor(name, shutdown, grace, shape) => {
                let child = SubSup {
                    shape,
                    probe: self.probe.clone(),
                };
                self.adopt(
                    ctx,
                    name,
                    child,
                    ActorConfig::default().supervisor(),
                    (shutdown, grace, true),
                )?;
                Ok(Ok(()))
            }
            Cmd::EndThenCall(name, terminate) => {
                self.fail.notify_one();
                let alive = |ctx: &ActorContext<Self>| {
                    ctx.children()
                        .iter()
                        .any(|info| info.id.as_str() == name && info.is_alive)
                };
                let deadline = Instant::now() + Duration::from_secs(5);
                while alive(ctx) {
                    if Instant::now() >= deadline {
                        return Ok(Err("the child still reads alive after five seconds".into()));
                    }
                    sleep(Duration::from_millis(5)).await;
                }
                let answer = if terminate {
                    ctx.terminate_child(name).await.map(|_| ())
                } else {
                    ctx.stop_child(name).await.map(|_| ())
                };
                Ok(answer.map_err(|err| format!("{err:?}")))
            }
            Cmd::PendGroup(name) => {
                let sup = ctx.supervision_mut().expect("a supervisor");
                let id = ActorId::from(name);
                let awaited = sup
                    .registry
                    .get(&id)
                    .and_then(|child| child.lifecycle.occupant())
                    .expect("the child runs");
                sup.pending_group = Some(GroupPhase::Stopping(GroupRestart {
                    awaiting: [(id, awaited)].into_iter().collect(),
                    stop_queue: Default::default(),
                    head_reached: false,
                    restart_order: Vec::new(),
                    manual_overrides: Default::default(),
                    scope: Default::default(),
                }));
                Ok(Ok(()))
            }
            Cmd::TerminateThenVerdict(name) => {
                let answer = ctx.terminate_child(name.clone()).await;
                let flagged = ctx
                    .supervision_ref()
                    .and_then(|sup| sup.registry.get(&ActorId::from(name)))
                    .is_some_and(|child| child.ctl.is_unresponsive());
                Ok(Err(format!("{answer:?}; flagged={flagged}")))
            }
            Cmd::Terminate(name) => Ok(ctx
                .terminate_child(name)
                .await
                .map_err(|err| format!("{err:?}"))),
            Cmd::Stop(name) => Ok(ctx
                .stop_child(name)
                .await
                .map(|_| ())
                .map_err(|err| format!("{err:?}"))),
            Cmd::Inspect(name) => {
                self.fail.notify_one();
                let alive = |ctx: &ActorContext<Self>| {
                    ctx.children()
                        .iter()
                        .any(|info| info.id.as_str() == name && info.is_alive)
                };
                let deadline = Instant::now() + Duration::from_secs(1);
                while alive(ctx) {
                    if Instant::now() >= deadline {
                        return Ok(Err("the child still reads alive after one second".into()));
                    }
                    sleep(Duration::from_millis(5)).await;
                }
                let mut failed = Vec::new();
                let pending = ctx
                    .children()
                    .iter()
                    .find(|info| info.id.as_str() == name)
                    .map(|info| info.restart_pending);
                if pending != Some(true) {
                    failed.push(format!("restart_pending reads {pending:?}"));
                }
                match ctx.restart_child(name.clone()) {
                    Err(SupervisionError::ChildRestarting(_)) => {}
                    other => failed.push(format!("restart_child answers {other:?}")),
                }
                match ctx.delete_child(name.clone()) {
                    Err(SupervisionError::ChildRestarting(_)) => {}
                    other => failed.push(format!("delete_child answers {other:?}")),
                }
                Ok(if failed.is_empty() {
                    Ok(())
                } else {
                    Err(failed.join("; "))
                })
            }
        }
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.lock().unwrap().push(reason.clone());
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.reports.lock().unwrap().push(Report {
            event: event.clone(),
            task_error: ctx.last_supervision_task_error().cloned(),
            last_error_clear: ctx.last_error().is_none(),
        });
        if event.action == SupervisionAction::RestartInitiated
            && self.fail_delivery.swap(false, Ordering::SeqCst)
        {
            return Err(ActorError::user("the delivery failed"));
        }
        Ok(())
    }
}

struct Rig {
    sup: crate::actor::handle::ActorHandle<Sup>,
    probe: Arc<Probe>,
    fail: Arc<Notify>,
    reports: Arc<Mutex<Vec<Report>>>,
    stopped: Arc<Mutex<Vec<StopReason>>>,
    name: String,
}

async fn rig() -> Rig {
    rig_with(DEFAULT_KILL_GRACE).await
}

/// A supervisor adopting a child with this kill grace.
async fn rig_with(grace: Duration) -> Rig {
    rig_adopting(|name| Cmd::AdoptWith(name, grace)).await
}

/// A supervisor adopting a vetoing worker with this policy and kill grace.
async fn rig_over_worker(shutdown: Shutdown, grace: Duration) -> Rig {
    rig_adopting(|name| Cmd::AdoptWorker(name, shutdown, grace)).await
}

/// A supervisor adopting a child that is itself a supervisor, with this
/// policy, kill grace and behavior.
async fn rig_over_supervisor(shutdown: Shutdown, grace: Duration, shape: Shape) -> Rig {
    let shape = Arc::new(shape);
    rig_adopting(|name| Cmd::AdoptSupervisor(name, shutdown, grace, shape)).await
}

/// A supervisor that runs `adopt(name)` to register its one child.
async fn rig_adopting(adopt: impl FnOnce(String) -> Cmd) -> Rig {
    let probe = Arc::new(Probe::default());
    let fail = Arc::new(Notify::new());
    let reports = Arc::new(Mutex::new(Vec::new()));
    let stopped = Arc::new(Mutex::new(Vec::new()));
    let sup = Sup {
        probe: probe.clone(),
        fail: fail.clone(),
        reports: reports.clone(),
        stopped: stopped.clone(),
        fail_delivery: Arc::new(AtomicBool::new(false)),
    }
    .spawn()
    .named(uname("sup"))
    .with_supervision(SupervisionConfig::default())
    .await
    .expect("supervisor spawn");
    let name = uname("child");
    let adopted = timeout(Duration::from_secs(10), sup.send(adopt(name.clone())))
        .await
        .expect("adopt answers")
        .expect("supervisor answers");
    assert_eq!(adopted, Ok(()));
    Rig {
        sup,
        probe,
        fail,
        reports,
        stopped,
        name,
    }
}

/// The child has been asked to stop and vetoed once: its stop is ordered.
async fn until_vetoed(rig: &Rig) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while rig.probe.pre_stops.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the child never saw the request");
        sleep(Duration::from_millis(5)).await;
    }
}

/// Puts the child inside a long turn, so a `Kill` raised now is seen only
/// when the turn ends, and the child ends sooner only by the abort the guard
/// feeds the system's reaper. It returns once the turn has begun: the stop
/// lane is read ahead of the mailbox, so a `Kill` raised while the message
/// still waits there would end the child before the turn.
async fn make_busy(rig: &Rig) {
    let child = crate::system::ActorSystem::default()
        .get::<Vetoer>(&rig.name)
        .expect("the child is registered");
    rig.probe.in_turn.store(false, Ordering::SeqCst);
    child.notify(()).await.expect("the child takes the message");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rig.probe.in_turn.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the child never began the turn");
        sleep(Duration::from_millis(1)).await;
    }
}

async fn a_caller_waiting_on_a_child_whose_runner_fails_returns_ok_once_it_ends() {
    let rig = rig().await;
    let sup = rig.sup.clone();
    let name = rig.name.clone();
    let call = tokio::spawn(async move { sup.send(Cmd::Terminate(name)).await });
    until_vetoed(&rig).await;
    make_busy(&rig).await;
    rig.fail.notify_one();
    let answer = timeout(Duration::from_secs(10), call)
        .await
        .expect("the call returns")
        .expect("joined")
        .expect("supervisor answers");
    assert_eq!(
        answer,
        Ok(()),
        "the call returns Ok once the guard's Kill ended the child, never ChildUnresponsive"
    );
    assert!(
        rig.probe.ended.load(Ordering::SeqCst),
        "and only after the child ended"
    );
    // The end is reported once, with the reason the child ended with.
    let deadline = Instant::now() + Duration::from_secs(10);
    while rig.reports.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no report of the child's end");
        sleep(Duration::from_millis(5)).await;
    }
    sleep(Duration::from_millis(300)).await;
    let reports = rig.reports.lock().unwrap();
    assert_eq!(reports.len(), 1, "reported once");
    assert!(matches!(reports[0].event.reason, StopReason::Kill));
    assert_eq!(reports[0].event.action, SupervisionAction::Removed);
    // The runner's panic is in the supervisor's own accessor for crate
    // task failures, and the supervisor's last error is untouched.
    assert!(
        matches!(&reports[0].task_error, Some(ActorError::Panic(text)) if text == "runner failed"),
        "{:?}",
        reports[0].task_error
    );
    assert!(reports[0].last_error_clear);
}

/// The child's value takes 400 ms to drop, longer than the settle time that
/// bounds a wait on its subtree. `Stopped` is published only after the value
/// is gone, so the wait for the subtree that follows the end of a child whose
/// runner failed starts from a finished drop and does not expire.
///
/// The child's grace outlasts its turn, so it leaves its loop by itself; a
/// drop that outlasts the ladder after an abort is the verdict's case.
async fn a_slow_drop_of_a_child_whose_runner_failed_never_escalates() {
    let rig = rig_with(Duration::from_secs(2)).await;
    rig.probe.drop_ms.store(400, Ordering::SeqCst);
    let sup = rig.sup.clone();
    let name = rig.name.clone();
    let call = tokio::spawn(async move { sup.send(Cmd::Terminate(name)).await });
    until_vetoed(&rig).await;
    make_busy(&rig).await;
    rig.fail.notify_one();
    let answer = timeout(Duration::from_secs(10), call)
        .await
        .expect("the call returns")
        .expect("joined")
        .expect("supervisor answers");
    assert_eq!(answer, Ok(()), "a slow drop is not an unresponsive child");
    assert!(
        rig.probe.ended.load(Ordering::SeqCst),
        "and the call returned only after the value was dropped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_drop_of_a_child_whose_runner_failed_never_escalates_multi_thread() {
    a_slow_drop_of_a_child_whose_runner_failed_never_escalates().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_slow_drop_of_a_child_whose_runner_failed_never_escalates_current_thread() {
    a_slow_drop_of_a_child_whose_runner_failed_never_escalates().await;
}

async fn a_parent_stop_waits_for_a_child_whose_runner_failed() {
    let rig = rig().await;
    let stop = {
        let sup = rig.sup.clone();
        tokio::spawn(async move {
            sup.stop(StopReason::Graceful).await.expect("stop");
            sup.wait_stopped().await;
        })
    };
    until_vetoed(&rig).await;
    make_busy(&rig).await;
    rig.fail.notify_one();
    timeout(Duration::from_secs(10), stop)
        .await
        .expect("the parent stops")
        .expect("joined");
    assert!(
        rig.probe.ended.load(Ordering::SeqCst),
        "the parent publishes Stopped only after the child ended"
    );
}

async fn a_lost_runner_reports_its_childs_end_as_a_kill() {
    // No call waits on the child. Its runner fails, the guard's Kill and abort
    // end the child, and the replacement task reports how it ended.
    let rig = rig().await;
    rig.fail.notify_one();
    let deadline = Instant::now() + Duration::from_secs(10);
    while rig.reports.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no report of the child's end");
        sleep(Duration::from_millis(5)).await;
    }
    assert!(
        rig.probe.ended.load(Ordering::SeqCst),
        "the end is reported only once the child ended"
    );
    sleep(Duration::from_millis(300)).await;
    let reports = rig.reports.lock().unwrap();
    assert_eq!(reports.len(), 1, "reported once");
    assert!(
        matches!(reports[0].event.reason, StopReason::Kill),
        "{:?}",
        reports[0].event.reason
    );
    assert_eq!(reports[0].event.action, SupervisionAction::Removed);
    assert!(
        matches!(&reports[0].task_error, Some(ActorError::Panic(text)) if text == "runner failed")
    );
    assert!(reports[0].last_error_clear);
}

/// The child ends `Graceful` while its runner still runs, and the runner then
/// fails before it records the end: the replacement reports the reason the
/// child's plane carries, `Graceful`, not `Kill`.
async fn a_lost_runner_reports_the_reason_its_ended_child_wrote() {
    let rig = rig().await;
    rig.probe.accepts.store(true, Ordering::SeqCst);
    let child = crate::system::ActorSystem::default()
        .get::<Vetoer>(&rig.name)
        .expect("the child is registered");
    child.stop(StopReason::Graceful).await.expect("accepted");
    let plane = rig.probe.plane.lock().unwrap().clone().expect("a plane");
    let deadline = Instant::now() + Duration::from_secs(10);
    while plane.has_changed().is_ok() && plane.borrow().status != ActorStatus::Stopped {
        assert!(Instant::now() < deadline, "the child never ended");
        sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(plane.borrow().reason, Some(StopReason::Graceful));
    assert!(
        rig.reports.lock().unwrap().is_empty(),
        "the stand-in runner records nothing before it fails"
    );
    rig.fail.notify_one();
    let deadline = Instant::now() + Duration::from_secs(10);
    while rig.reports.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no report of the child's end");
        sleep(Duration::from_millis(5)).await;
    }
    let reports = rig.reports.lock().unwrap();
    assert_eq!(reports.len(), 1, "reported once");
    assert_eq!(reports[0].event.reason, StopReason::Graceful);
    assert_eq!(reports[0].event.action, SupervisionAction::Removed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_runner_reports_the_reason_its_ended_child_wrote_multi_thread() {
    a_lost_runner_reports_the_reason_its_ended_child_wrote().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_lost_runner_reports_the_reason_its_ended_child_wrote_current_thread() {
    a_lost_runner_reports_the_reason_its_ended_child_wrote().await;
}

async fn children_and_restart_child_read_a_lost_runners_ended_task_as_down_with_its_decision_owed()
{
    let rig = rig().await;
    let answer = timeout(
        Duration::from_secs(10),
        rig.sup.send(Cmd::Inspect(rig.name.clone())),
    )
    .await
    .expect("the supervisor answers")
    .expect("the supervisor answers");
    assert_eq!(answer, Ok(()), "{answer:?}");
    // After the handler returns, the runner's failure and the replacement
    // report are handled: the end is reported once, as a kill.
    let deadline = Instant::now() + Duration::from_secs(10);
    while rig.reports.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no report of the child's end");
        sleep(Duration::from_millis(5)).await;
    }
    sleep(Duration::from_millis(300)).await;
    let reports = rig.reports.lock().unwrap();
    assert_eq!(reports.len(), 1, "reported once");
    assert!(matches!(reports[0].event.reason, StopReason::Kill));
    assert_eq!(reports[0].event.action, SupervisionAction::Removed);
    // The child's value was dropped before the plane read its end.
    assert!(
        !rig.probe.saw_the_end_at_drop.load(Ordering::SeqCst),
        "the plane read the child's end before its value was dropped"
    );
}

/// The child's value is dropped before the plane its supervisor follows reads
/// the child's end: an idle child ends at its next turn boundary, a busy one
/// is aborted after its kill grace, and in both the value's `Drop` sees a
/// plane that has neither published `Stopped` nor closed.
async fn lost_runner_end_is_observed_after_the_actor_value_drops(busy: bool) {
    let rig = rig().await;
    if busy {
        make_busy(&rig).await;
    }
    // The runner fails: the guard's Kill ends the child.
    rig.fail.notify_one();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rig.probe.ended.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the child never ended");
        sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !rig.probe.saw_the_end_at_drop.load(Ordering::SeqCst),
        "the plane read the child's end before its value was dropped (busy: {busy})"
    );
    // The plane carries the reason the incarnation ended with, written by the
    // task or, for an abort, by the context before the plane closed.
    let plane = rig.probe.plane.lock().unwrap().clone().expect("a plane");
    let deadline = Instant::now() + Duration::from_secs(10);
    while plane.has_changed().is_ok() && plane.borrow().status != ActorStatus::Stopped {
        assert!(Instant::now() < deadline, "the plane never ended");
        sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        plane.borrow().reason,
        Some(StopReason::Kill),
        "the ended plane holds the incarnation's reason (busy: {busy})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_runner_end_is_observed_after_the_actor_value_drops_idle_multi_thread() {
    lost_runner_end_is_observed_after_the_actor_value_drops(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn lost_runner_end_is_observed_after_the_actor_value_drops_idle_current_thread() {
    lost_runner_end_is_observed_after_the_actor_value_drops(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_runner_end_is_observed_after_the_actor_value_drops_abort_multi_thread() {
    // The race window of the abort is nanoseconds wide: the row is repeated.
    for _ in 0..50 {
        lost_runner_end_is_observed_after_the_actor_value_drops(true).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn lost_runner_end_is_observed_after_the_actor_value_drops_abort_current_thread() {
    lost_runner_end_is_observed_after_the_actor_value_drops(true).await;
}

// Each test runs on a multi-threaded runtime and on a current-thread one,
// sharing one body.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn children_and_restart_child_read_a_lost_runners_ended_task_as_down_with_its_decision_owed_multi_thread(
) {
    children_and_restart_child_read_a_lost_runners_ended_task_as_down_with_its_decision_owed()
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn children_and_restart_child_read_a_lost_runners_ended_task_as_down_with_its_decision_owed_current_thread(
) {
    children_and_restart_child_read_a_lost_runners_ended_task_as_down_with_its_decision_owed()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_waiting_on_a_child_whose_runner_fails_returns_ok_once_it_ends_multi_thread() {
    a_caller_waiting_on_a_child_whose_runner_fails_returns_ok_once_it_ends().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_caller_waiting_on_a_child_whose_runner_fails_returns_ok_once_it_ends_current_thread() {
    a_caller_waiting_on_a_child_whose_runner_fails_returns_ok_once_it_ends().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_stop_waits_for_a_child_whose_runner_failed_multi_thread() {
    a_parent_stop_waits_for_a_child_whose_runner_failed().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_parent_stop_waits_for_a_child_whose_runner_failed_current_thread() {
    a_parent_stop_waits_for_a_child_whose_runner_failed().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_runner_reports_its_childs_end_as_a_kill_multi_thread() {
    a_lost_runner_reports_its_childs_end_as_a_kill().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_lost_runner_reports_its_childs_end_as_a_kill_current_thread() {
    a_lost_runner_reports_its_childs_end_as_a_kill().await;
}

/// Starts the child's stuck turn, and returns once it has begun: it blocks
/// its thread for 1500 ms, or as `stuck_long` set it.
async fn make_stuck(rig: &Rig) {
    rig.probe.stuck.store(true, Ordering::SeqCst);
    make_busy(rig).await;
}

async fn caller_told_unresponsive_on_a_lost_runner_leaves_one_verdict_for_the_group_barrier() {
    #[cfg(feature = "tracing")]
    let logged = capture::install();
    // First form: a handler's `terminate_child` waits on the closed cell of a
    // member the pending group waits for.
    let rig = rig().await;
    assert_eq!(
        rig.sup
            .send(Cmd::PendGroup(rig.name.clone()))
            .await
            .unwrap(),
        Ok(())
    );
    let unsticks = stuck_long(&rig);
    make_stuck(&rig).await;
    let call = {
        let sup = rig.sup.clone();
        let name = rig.name.clone();
        tokio::spawn(async move { sup.send(Cmd::TerminateThenVerdict(name)).await })
    };
    sleep(Duration::from_millis(50)).await;
    let failed_at = Instant::now();
    rig.fail.notify_one();
    let answer = timeout(Duration::from_secs(10), call)
        .await
        .expect("the call returns")
        .expect("joined")
        .expect("the supervisor answers");
    let told_at = Instant::now();
    let text = answer.expect_err("the call reports what it saw");
    assert!(text.contains("ChildUnresponsive"), "{text}");
    assert!(
        text.contains("flagged=true"),
        "the verdict is recorded when the caller is told: {text}"
    );
    assert!(
        told_at.duration_since(failed_at) >= DEFAULT_KILL_GRACE + ABORT_SETTLE,
        "the bound counts from the wait on the closed cell"
    );
    assert!(
        told_at.duration_since(failed_at) < DEFAULT_KILL_GRACE + ABORT_SETTLE + STALL_SLACK,
        "the caller is told at the bound: {:?}",
        told_at.duration_since(failed_at)
    );
    // The pending group's barrier escalates without waiting another budget:
    // the supervisor stops with the verdict, well before the stuck turn ends.
    timeout(Duration::from_secs(5), rig.sup.wait_stopped())
        .await
        .expect("the supervisor stops");
    assert!(
        told_at.elapsed() < STALL_SLACK,
        "no further budget was waited: {:?}",
        told_at.elapsed()
    );
    assert!(
        !rig.probe.ended.load(Ordering::SeqCst),
        "the stuck turn still runs"
    );
    let reasons = rig.stopped.lock().unwrap().clone();
    assert!(
        matches!(
            reasons.as_slice(),
            [StopReason::Failure(ActorError::Supervision(
                SupervisionError::ChildUnresponsive(_)
            ))]
        ),
        "{reasons:?}"
    );
    // The replacement task, once it would have expired, finds the verdict
    // recorded and the supervisor gone: nothing is reported twice. A second
    // wake-up of the supervisor's verdict check is not observable from
    // outside, since wake-ups coalesce into one permit; the single write is
    // pinned by `a_lost_verdict_is_recorded_and_signalled_once` in `slot.rs`.
    sleep(Duration::from_millis(400)).await;
    assert_eq!(rig.reports.lock().unwrap().len(), 0);
    // The caller and the group barrier received the verdict, and it was
    // logged once, where it was made.
    #[cfg(feature = "tracing")]
    assert_eq!(
        capture::naming(&logged.lock().unwrap(), &rig.name).len(),
        1,
        "the verdict is logged once"
    );
    drop(unsticks);
}

async fn a_parent_stop_records_the_verdict_of_a_lost_runner_and_moves_on() {
    // Second form: the teardown's wait reaches the same closed cell.
    #[cfg(feature = "tracing")]
    let logged = capture::install();
    let rig = rig().await;
    let unsticks = stuck_long(&rig);
    make_stuck(&rig).await;
    rig.fail.notify_one();
    sleep(Duration::from_millis(30)).await;
    let stopping_at = Instant::now();
    rig.sup.stop(StopReason::Graceful).await.expect("stop");
    timeout(Duration::from_secs(5), rig.sup.wait_stopped())
        .await
        .expect("the supervisor stops");
    let took = stopping_at.elapsed();
    assert!(
        took < DEFAULT_KILL_GRACE + ABORT_SETTLE + STALL_SLACK,
        "the teardown moved on at the verdict: {took:?}"
    );
    assert!(!rig.probe.ended.load(Ordering::SeqCst));
    // The verdict is one flag and one wake-up: the replacement task, once its
    // bound expires, finds the flag set and the supervisor gone. No call
    // received it, so it is logged once.
    sleep(Duration::from_millis(400)).await;
    assert_eq!(rig.reports.lock().unwrap().len(), 0);
    #[cfg(feature = "tracing")]
    assert_eq!(
        capture::naming(&logged.lock().unwrap(), &rig.name).len(),
        1,
        "the teardown's verdict is logged once"
    );
    drop(unsticks);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn caller_told_unresponsive_on_a_lost_runner_leaves_one_verdict_for_the_group_barrier_multi_thread(
) {
    caller_told_unresponsive_on_a_lost_runner_leaves_one_verdict_for_the_group_barrier().await;
}

async fn a_runner_that_fails_after_the_verdict_logs_it_once() {
    // The runner writes the verdict, then fails; the wait on the closed cell
    // that follows finds the flag set and makes no second verdict.
    #[cfg(feature = "tracing")]
    let logged = capture::install();
    let rig = rig().await;
    rig.probe.verdict_first.store(true, Ordering::SeqCst);
    make_stuck(&rig).await;
    rig.fail.notify_one();
    // The replacement task's bound, and the turn's end, both pass.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rig.probe.ended.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the incarnation never ended");
        sleep(Duration::from_millis(10)).await;
    }
    sleep(Duration::from_millis(100)).await;
    #[cfg(feature = "tracing")]
    assert_eq!(
        capture::naming(&logged.lock().unwrap(), &rig.name).len(),
        1,
        "the verdict is logged once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_runner_that_fails_after_the_verdict_logs_it_once_multi_thread() {
    a_runner_that_fails_after_the_verdict_logs_it_once().await;
}

async fn a_lost_runners_stuck_incarnation_is_aborted_at_its_next_await() {
    // The incarnation's turn blocks its thread, then awaits for ever: the
    // guard's abort, due long before the turn returns, is the only thing
    // that ends it, and it lands at that await.
    let rig = rig().await;
    rig.probe.then_hangs.store(true, Ordering::SeqCst);
    // Taken before the turn is sent: the turn begins after this instant, so
    // an end that comes with the turn is at least its 1500 ms past it.
    let stuck_at = Instant::now();
    make_stuck(&rig).await;
    rig.fail.notify_one();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rig.probe.ended.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the incarnation was never aborted"
        );
        sleep(Duration::from_millis(10)).await;
    }
    assert!(
        stuck_at.elapsed() >= Duration::from_millis(1200),
        "it ended with its turn, not before: {:?}",
        stuck_at.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_runners_stuck_incarnation_is_aborted_at_its_next_await_multi_thread() {
    a_lost_runners_stuck_incarnation_is_aborted_at_its_next_await().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parent_stop_records_the_verdict_of_a_lost_runner_and_moves_on_multi_thread() {
    a_parent_stop_records_the_verdict_of_a_lost_runner_and_moves_on().await;
}

/// The kill grace of the child in `lost_runner_verdict_waits_the_childs_own_kill_grace`.
const OWN_GRACE: Duration = Duration::from_millis(600);

async fn lost_runner_verdict_waits_the_childs_own_kill_grace() {
    let bound = OWN_GRACE + ABORT_SETTLE;

    // First form: a handler's `stop_child` waits on the closed cell of an
    // incarnation stuck in a turn that holds its thread until the test ends
    // it. The verdict comes at the child's own grace plus the settle time
    // after the cell closed, and not before.
    let rig = rig_with(OWN_GRACE).await;
    let unsticks = stuck_long(&rig);
    make_stuck(&rig).await;
    let call = {
        let sup = rig.sup.clone();
        let name = rig.name.clone();
        tokio::spawn(async move { sup.send(Cmd::Stop(name)).await })
    };
    sleep(Duration::from_millis(50)).await;
    let failed_at = Instant::now();
    rig.fail.notify_one();
    let answer = timeout(Duration::from_secs(10), call)
        .await
        .expect("the call returns")
        .expect("joined")
        .expect("the supervisor answers");
    let took = failed_at.elapsed();
    let text = answer.expect_err("the call reports what it saw");
    assert!(text.contains("ChildUnresponsive"), "{text}");
    assert!(
        took >= bound,
        "the verdict waits the child's own grace plus the settle time: {took:?}"
    );
    assert!(
        took < bound + STALL_SLACK,
        "and within a margin of that: {took:?}"
    );
    assert!(
        !rig.probe.ended.load(Ordering::SeqCst),
        "the stuck turn still runs"
    );
    drop(unsticks);

    // Second form: no caller waits. A pending group's barrier on the child
    // escalates in the same window, through the replacement task's wait.
    let rig = rig_with(OWN_GRACE).await;
    let unsticks = stuck_long(&rig);
    assert_eq!(
        rig.sup
            .send(Cmd::PendGroup(rig.name.clone()))
            .await
            .unwrap(),
        Ok(())
    );
    make_stuck(&rig).await;
    let failed_at = Instant::now();
    rig.fail.notify_one();
    timeout(Duration::from_secs(5), rig.sup.wait_stopped())
        .await
        .expect("the supervisor stops with the verdict");
    let took = failed_at.elapsed();
    assert!(
        took >= bound,
        "the barrier escalates after the child's own grace plus the settle time: {took:?}"
    );
    assert!(
        took < bound + STALL_SLACK,
        "and within a margin of that: {took:?}"
    );
    assert!(!rig.probe.ended.load(Ordering::SeqCst));
    drop(unsticks);
}

// A stuck turn holds a worker until the test ends it and the waits are timed
// against it, so the body runs on a multi-threaded runtime only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_runner_verdict_waits_the_childs_own_kill_grace_multi_thread() {
    lost_runner_verdict_waits_the_childs_own_kill_grace().await;
}

// ---------------------------------------------------------------------------
// The registration of each runner with the plane it runs in
// ---------------------------------------------------------------------------

struct Plain;

impl Actor for Plain {
    type Message = bool;
    type Response = ();

    async fn handle(&mut self, crash: bool, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if crash {
            panic!("child crashed on command");
        }
        Ok(())
    }
}

struct Owners {
    events: Arc<Mutex<Vec<ChildEvent>>>,
}

#[derive(Clone)]
enum OwnerCmd {
    Spawn(String),
    /// The running incarnation the ledger reads for the child (none while
    /// it is not running), and every (child, incarnation) the supervisor's
    /// task registry holds.
    Read(String),
    /// Cancels every task of the death plane, as a failure of every runner.
    AbortRunners,
}

impl Actor for Owners {
    type Message = OwnerCmd;
    type Response = (Option<u64>, Vec<(String, u64, TaskPlane)>);

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }

    async fn handle(
        &mut self,
        cmd: OwnerCmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match cmd {
            OwnerCmd::Spawn(name) => {
                ctx.spawn_child(|| Plain)
                    .named(name)
                    .restart_type(RestartType::Permanent)
                    .await?;
                Ok((None, Vec::new()))
            }
            OwnerCmd::AbortRunners => {
                ctx.supervision_mut()
                    .expect("a supervisor")
                    .death_set
                    .abort_all();
                Ok((None, Vec::new()))
            }
            OwnerCmd::Read(name) => {
                let sup = ctx.supervision_ref().expect("a supervisor");
                let ledger = sup.registry.get(&ActorId::from(name)).and_then(|child| {
                    match child.lifecycle {
                        ChildLifecycle::Running(n) => Some(n),
                        _ => None,
                    }
                });
                let owners = sup
                    .task_owner
                    .values()
                    .map(|(id, incarnation, plane)| (id.as_str().to_string(), *incarnation, *plane))
                    .collect();
                Ok((ledger, owners))
            }
        }
    }
}

async fn every_runner_is_registered_with_its_plane() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sup = Owners {
        events: events.clone(),
    }
    .spawn()
    .named(uname("owners"))
    .with_supervision(SupervisionConfig::default())
    .await
    .expect("supervisor spawn");
    let name = uname("owned");
    let _ = sup.send(OwnerCmd::Spawn(name.clone())).await.unwrap();

    // After the spawn: the death plane's task for the initial incarnation
    // maps to (child, that incarnation).
    let (ledger, owners) = sup.send(OwnerCmd::Read(name.clone())).await.unwrap();
    let first = ledger.expect("the child is registered");
    assert_eq!(
        owners,
        vec![(name.clone(), first, TaskPlane::Death)],
        "after the spawn"
    );

    // After a supervised restart: the first incarnation's entry is cleared
    // when its completion drains, and the restarted incarnation's runner is
    // registered under its own incarnation.
    let handle = crate::system::ActorSystem::default()
        .get::<Plain>(&name)
        .expect("the child is registered");
    let _ = handle.send(true).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (ledger, owners) = sup.send(OwnerCmd::Read(name.clone())).await.unwrap();
        if let Some(second) = ledger {
            if second != first && owners == vec![(name.clone(), second, TaskPlane::Death)] {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "after the restart the runner is registered under the new incarnation: {ledger:?} {owners:?}"
        );
        sleep(Duration::from_millis(10)).await;
    }

    // The keys are the death plane's task ids: when the restarted
    // incarnation's runner fails, its task id finds its owner, and the end of
    // the incarnation is reported exactly once.
    let kills = |events: &Arc<Mutex<Vec<ChildEvent>>>| {
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| {
                matches!(event.reason, StopReason::Kill) && event.child_id.as_str() == name
            })
            .count()
    };
    assert_eq!(kills(&events), 0);
    let _ = sup.send(OwnerCmd::AbortRunners).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while kills(&events) == 0 {
        assert!(
            Instant::now() < deadline,
            "the failed runner's task id found no owner, so no death was reported"
        );
        sleep(Duration::from_millis(10)).await;
    }
    sleep(Duration::from_millis(300)).await;
    assert_eq!(kills(&events), 1, "the end is reported exactly once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_runner_is_registered_with_its_plane_multi_thread() {
    every_runner_is_registered_with_its_plane().await;
}

#[tokio::test(flavor = "current_thread")]
async fn every_runner_is_registered_with_its_plane_current_thread() {
    every_runner_is_registered_with_its_plane().await;
}

static TEARDOWN_PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());

async fn a_teardown_of_an_attempt_lost_after_its_spawn_does_not_panic() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            TEARDOWN_PANICS.lock().unwrap().push(info.to_string());
            prev(info);
        }));
    });
    let probe = Arc::new(Probe::default());
    let stopped = Arc::new(Mutex::new(Vec::new()));
    let sup = Sup {
        probe: probe.clone(),
        fail: Arc::new(Notify::new()),
        reports: Arc::new(Mutex::new(Vec::new())),
        stopped: stopped.clone(),
        fail_delivery: Arc::new(AtomicBool::new(false)),
    }
    .spawn()
    .named(uname("sup"))
    .with_supervision(SupervisionConfig::default())
    .await
    .expect("supervisor spawn");
    let name = uname("child");
    assert_eq!(
        sup.send(Cmd::AdoptAttempt(name.clone())).await.unwrap(),
        Ok(())
    );
    let child = crate::system::ActorSystem::default()
        .get::<Vetoer>(&name)
        .expect("the child is registered");
    child.notify(()).await.expect("busy turn");
    sleep(Duration::from_millis(30)).await;
    let _ = sup.send(Cmd::FailAttemptThenStop).await;
    let done = timeout(Duration::from_secs(5), sup.wait_stopped()).await;
    assert!(done.is_ok(), "the supervisor's teardown never finished");
    assert_eq!(
        stopped.lock().unwrap().len(),
        1,
        "the supervisor ran its on_stopped"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !probe.ended.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the attempt's incarnation never ended"
        );
        sleep(Duration::from_millis(10)).await;
    }
    sleep(Duration::from_millis(300)).await;
    let panics = TEARDOWN_PANICS.lock().unwrap().clone();
    assert!(
        !panics
            .iter()
            .any(|p| p.contains("all branches are disabled")),
        "the supervisor's teardown panicked: {panics:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_teardown_of_an_attempt_lost_after_its_spawn_does_not_panic_multi_thread() {
    a_teardown_of_an_attempt_lost_after_its_spawn_does_not_panic().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_teardown_of_an_attempt_lost_after_its_spawn_does_not_panic_current_thread() {
    a_teardown_of_an_attempt_lost_after_its_spawn_does_not_panic().await;
}

async fn a_teardown_of_a_lost_attempt_does_not_wait_on_an_older_siblings_attempt() {
    // The newer child's attempt is lost after its spawn, so the walk settles
    // it first, while the older child's attempt still initializes in the
    // restart plane and has not been ordered yet.
    let probe = Arc::new(Probe::default());
    let stopped = Arc::new(Mutex::new(Vec::new()));
    let sup = Sup {
        probe: probe.clone(),
        fail: Arc::new(Notify::new()),
        reports: Arc::new(Mutex::new(Vec::new())),
        stopped: stopped.clone(),
        fail_delivery: Arc::new(AtomicBool::new(false)),
    }
    .spawn()
    .named(uname("sup"))
    .with_supervision(SupervisionConfig::default())
    .await
    .expect("supervisor spawn");
    let (older, newer) = (uname("older"), uname("newer"));
    assert_eq!(
        sup.send(Cmd::AdoptHungAttempt(older.clone()))
            .await
            .unwrap(),
        Ok(())
    );
    assert_eq!(
        sup.send(Cmd::AdoptAttempt(newer.clone())).await.unwrap(),
        Ok(())
    );
    let child = crate::system::ActorSystem::default()
        .get::<Vetoer>(&newer)
        .expect("the child is registered");
    child.notify(()).await.expect("busy turn");
    sleep(Duration::from_millis(30)).await;
    let _ = sup.send(Cmd::FailAttemptThenStop).await;
    let done = timeout(Duration::from_secs(5), sup.wait_stopped()).await;
    assert!(
        done.is_ok(),
        "the teardown waits on the older sibling's attempt, which it has not ordered"
    );
    assert_eq!(stopped.lock().unwrap().len(), 1);
    assert!(
        probe.ordered.load(Ordering::SeqCst),
        "the teardown ordered the older sibling's attempt in its turn"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_teardown_of_a_lost_attempt_does_not_wait_on_an_older_siblings_attempt_multi_thread() {
    a_teardown_of_a_lost_attempt_does_not_wait_on_an_older_siblings_attempt().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_teardown_of_a_lost_attempt_does_not_wait_on_an_older_siblings_attempt_current_thread() {
    a_teardown_of_a_lost_attempt_does_not_wait_on_an_older_siblings_attempt().await;
}

async fn a_final_collection_is_not_held_by_the_replacement_of_a_lost_attempt() {
    let probe = Arc::new(Probe::default());
    let stopped = Arc::new(Mutex::new(Vec::new()));
    let sup = Sup {
        probe: probe.clone(),
        fail: Arc::new(Notify::new()),
        reports: Arc::new(Mutex::new(Vec::new())),
        stopped: stopped.clone(),
        fail_delivery: Arc::new(AtomicBool::new(false)),
    }
    .spawn()
    .named(uname("sup"))
    .with_supervision(SupervisionConfig::default())
    .await
    .expect("supervisor spawn");
    let (x, y) = (uname("x"), uname("y"));
    assert_eq!(
        sup.send(Cmd::AdoptAttempt(x.clone())).await.unwrap(),
        Ok(())
    );
    assert_eq!(
        sup.send(Cmd::AdoptHungAttempt(y.clone())).await.unwrap(),
        Ok(())
    );
    let child = crate::system::ActorSystem::default()
        .get::<Vetoer>(&x)
        .expect("the child is registered");
    child.notify(()).await.expect("busy turn");
    sleep(Duration::from_millis(30)).await;
    // The loop routes the attempt's JoinError: a replacement task follows
    // the live incarnation in the death plane, the ledger stays Restarting.
    let _ = sup.send(Cmd::FailOnly).await;
    sleep(Duration::from_millis(30)).await;
    // The incarnation ends while the supervisor is busy; then it stops.
    let _ = sup.send(Cmd::WaitEndedThenStop).await;
    let done = timeout(Duration::from_secs(3), sup.wait_stopped()).await;
    assert!(
        done.is_ok(),
        "the supervisor's stop hangs in its final collection"
    );
    assert!(
        probe.ordered.load(Ordering::SeqCst),
        "the teardown ordered the hung attempt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_final_collection_is_not_held_by_the_replacement_of_a_lost_attempt_multi_thread() {
    a_final_collection_is_not_held_by_the_replacement_of_a_lost_attempt().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_final_collection_is_not_held_by_the_replacement_of_a_lost_attempt_current_thread() {
    a_final_collection_is_not_held_by_the_replacement_of_a_lost_attempt().await;
}

async fn a_failing_delivery_in_a_group_evaluation_leaves_the_trigger_owed() {
    let reports = Arc::new(Mutex::new(Vec::new()));
    let stopped = Arc::new(Mutex::new(Vec::new()));
    let fail_delivery = Arc::new(AtomicBool::new(true));
    let sup = Sup {
        probe: Arc::new(Probe::default()),
        fail: Arc::new(Notify::new()),
        reports: reports.clone(),
        stopped: stopped.clone(),
        fail_delivery: fail_delivery.clone(),
    }
    .spawn()
    .named(uname("sup"))
    .with_supervision(SupervisionConfig::one_for_all())
    .await
    .expect("supervisor spawn");
    let (a, b, c) = (uname("a"), uname("b"), uname("c"));
    let _ = sup
        .send(Cmd::OweGroup(a.clone(), b.clone(), c.clone()))
        .await;
    let _ = timeout(Duration::from_secs(5), sup.wait_stopped()).await;
    assert!(
        matches!(stopped.lock().unwrap().as_slice(), [StopReason::Failure(_)]),
        "the supervisor stopped with the failed delivery: {:?}",
        stopped.lock().unwrap()
    );
    let reports = reports.lock().unwrap();
    let seen: Vec<(Option<String>, SupervisionAction)> = reports
        .iter()
        .map(|r| (r.event.child_name.clone(), r.event.action))
        .collect();
    assert_eq!(
        seen,
        vec![
            (Some(b), SupervisionAction::RestartInitiated),
            (Some(a), SupervisionAction::SupervisorStopping),
            (Some(c), SupervisionAction::SupervisorStopping),
        ],
        "the failing member's report was delivered once; the final drain then delivers the trigger's own and the report left behind it, in that order"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_delivery_in_a_group_evaluation_leaves_the_trigger_owed_multi_thread() {
    a_failing_delivery_in_a_group_evaluation_leaves_the_trigger_owed().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_failing_delivery_in_a_group_evaluation_leaves_the_trigger_owed_current_thread() {
    a_failing_delivery_in_a_group_evaluation_leaves_the_trigger_owed().await;
}

/// A capture of every WARN event, installed once for the test binary; tests
/// find their own lines by the exact `child` field.
#[cfg(feature = "tracing")]
pub(crate) mod capture {
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex, OnceLock};

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    #[derive(Clone, Default)]
    struct Warnings {
        lines: Arc<Mutex<Vec<String>>>,
    }

    struct Text(String);

    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    impl Subscriber for Warnings {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == Level::WARN
        }

        fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut text = Text(String::new());
            event.record(&mut text);
            self.lines.lock().unwrap().push(text.0);
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }

    pub(crate) fn install() -> Arc<Mutex<Vec<String>>> {
        static GLOBAL: OnceLock<Warnings> = OnceLock::new();
        GLOBAL
            .get_or_init(|| {
                let warnings = Warnings::default();
                tracing::subscriber::set_global_default(warnings.clone())
                    .expect("no other global subscriber is installed in this test binary");
                warnings
            })
            .lines
            .clone()
    }

    pub(super) fn naming(lines: &[String], child: &str) -> Vec<String> {
        let token = format!("child={child} ");
        lines
            .iter()
            .filter(|line| line.contains(&token))
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// A child that is itself a supervisor
// ---------------------------------------------------------------------------

/// The first report of the child's end, once it arrived and nothing more
/// followed it: the reason and the action.
async fn the_one_report(rig: &Rig) -> (StopReason, SupervisionAction) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while rig.reports.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no report of the child's end");
        sleep(Duration::from_millis(5)).await;
    }
    sleep(Duration::from_millis(300)).await;
    let reports = rig.reports.lock().unwrap();
    assert_eq!(reports.len(), 1, "reported once");
    (reports[0].event.reason.clone(), reports[0].event.action)
}

/// The shape of the supervisor child of the tests that wait on its orderly
/// stop: it vetoes the first request, and its one worker takes `worker_stop`
/// in its `on_stopped`.
fn slow_worker_below(worker_stop: Duration) -> Shape {
    Shape {
        vetoes: true,
        own_stop: OwnStopMode::Quick,
        worker: Some((
            worker_stop,
            Shutdown::Timeout(Duration::from_secs(5)),
            false,
        )),
    }
}

async fn a_caller_waiting_on_an_infinity_supervisor_child_whose_runner_fails_gets_no_early_verdict()
{
    let rig = rig_over_supervisor(
        Shutdown::Infinity,
        DEFAULT_KILL_GRACE,
        slow_worker_below(Duration::from_millis(600)),
    )
    .await;
    let call = {
        let sup = rig.sup.clone();
        let name = rig.name.clone();
        tokio::spawn(async move { sup.send(Cmd::Terminate(name)).await })
    };
    until_vetoed(&rig).await;
    let failed_at = Instant::now();
    rig.fail.notify_one();
    let answer = timeout(Duration::from_secs(10), call)
        .await
        .expect("the call returns")
        .expect("joined")
        .expect("supervisor answers");
    assert_eq!(
        answer,
        Ok(()),
        "an unbounded orderly stop is waited for, never called unresponsive"
    );
    assert!(
        failed_at.elapsed() >= Duration::from_millis(600),
        "the call returned before the worker's stop was over: {:?}",
        failed_at.elapsed()
    );
    assert!(
        rig.probe.worker_done.load(Ordering::SeqCst) && rig.probe.ended.load(Ordering::SeqCst),
        "and only after the child's whole subtree ended"
    );
    let (reason, action) = the_one_report(&rig).await;
    assert!(matches!(reason, StopReason::ParentKilled), "{reason:?}");
    assert_eq!(action, SupervisionAction::Removed);
}

async fn a_parent_stop_waits_for_an_infinity_supervisor_child_whose_runner_failed() {
    let rig = rig_over_supervisor(
        Shutdown::Infinity,
        DEFAULT_KILL_GRACE,
        slow_worker_below(Duration::from_millis(600)),
    )
    .await;
    let stop = {
        let sup = rig.sup.clone();
        tokio::spawn(async move {
            sup.stop(StopReason::Graceful).await.expect("stop");
            sup.wait_stopped().await;
        })
    };
    until_vetoed(&rig).await;
    rig.fail.notify_one();
    timeout(Duration::from_secs(10), stop)
        .await
        .expect("the parent stops")
        .expect("joined");
    assert!(
        rig.probe.worker_done.load(Ordering::SeqCst) && rig.probe.ended.load(Ordering::SeqCst),
        "the parent publishes Stopped only after the child's subtree ended"
    );
    let reasons = rig.stopped.lock().unwrap().clone();
    assert!(
        matches!(reasons.as_slice(), [StopReason::Graceful]),
        "the parent stops with the reason it was given: {reasons:?}"
    );
}

/// The window and the grace of the supervisor child of the window tests.
const WINDOW: Duration = Duration::from_millis(400);
const WINDOW_GRACE: Duration = Duration::from_millis(100);

/// A supervisor child whose own `on_stopped` is the given mode, with no worker.
fn own_stop_of(own_stop: OwnStopMode) -> Shape {
    Shape {
        vetoes: true,
        own_stop,
        worker: None,
    }
}

async fn a_lost_runner_wait_follows_a_supervisor_childs_window_first(paused: bool) {
    // First form: a handler's `terminate_child` waits on the closed cell. The
    // child stops in order for its window, then its Kill is raised and its
    // task aborted a grace later, in the middle of a yielding `on_stopped`:
    // it ends as a kill, not before the window, and the call is not told it
    // is unresponsive at the worker's bound of 200 ms.
    let rig = rig_over_supervisor(
        Shutdown::Timeout(WINDOW),
        WINDOW_GRACE,
        own_stop_of(OwnStopMode::Sleep(Duration::from_secs(2))),
    )
    .await;
    let call = {
        let sup = rig.sup.clone();
        let name = rig.name.clone();
        tokio::spawn(async move { sup.send(Cmd::Terminate(name)).await })
    };
    until_vetoed(&rig).await;
    let failed_at = Instant::now();
    rig.fail.notify_one();
    let answer = timeout(Duration::from_secs(10), call)
        .await
        .expect("the call returns")
        .expect("joined")
        .expect("supervisor answers");
    let took = failed_at.elapsed();
    assert_eq!(answer, Ok(()), "the abort ended the child: {answer:?}");
    assert!(
        took >= WINDOW,
        "the child was given its window first: {took:?}"
    );
    let by = if paused {
        WINDOW + WINDOW_GRACE + ABORT_SETTLE + Duration::from_millis(500)
    } else {
        HANG_BOUND
    };
    assert!(
        took < by,
        "and the call returned by the bound of its stop: {took:?}"
    );
    let (reason, action) = the_one_report(&rig).await;
    assert!(matches!(reason, StopReason::Kill), "{reason:?}");
    assert_eq!(action, SupervisionAction::Removed);

    // Second form: no caller waits. A pending group's barrier on the child
    // reaches the replacement task's wait, which follows the same bound: the
    // child ends at its abort, before any verdict, and the supervisor goes on.
    let rig = rig_over_supervisor(
        Shutdown::Timeout(WINDOW),
        WINDOW_GRACE,
        own_stop_of(OwnStopMode::Sleep(Duration::from_secs(2))),
    )
    .await;
    assert_eq!(
        rig.sup
            .send(Cmd::PendGroup(rig.name.clone()))
            .await
            .unwrap(),
        Ok(())
    );
    rig.fail.notify_one();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rig.probe.ended.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the child never ended");
        sleep(Duration::from_millis(5)).await;
    }
    sleep(Duration::from_millis(400)).await;
    assert!(
        timeout(Duration::from_millis(100), rig.sup.wait_stopped())
            .await
            .is_err(),
        "no verdict stopped the supervisor while the child was ending in order"
    );
    assert!(rig.stopped.lock().unwrap().is_empty());
}

async fn a_lost_supervisor_child_stuck_in_a_turn_gets_the_verdict_after_its_window_and_grace() {
    // The child's own `on_stopped` blocks its thread until the test ends
    // it, so the abort cannot land: the verdict comes at the window, the
    // grace and the settle time, and not before, and while the turn runs.
    let rig = rig_over_supervisor(
        Shutdown::Timeout(WINDOW),
        WINDOW_GRACE,
        own_stop_of(OwnStopMode::Block(LONG_STUCK)),
    )
    .await;
    let unsticks = Unsticks(rig.probe.clone());
    let call = {
        let sup = rig.sup.clone();
        let name = rig.name.clone();
        tokio::spawn(async move { sup.send(Cmd::TerminateThenVerdict(name)).await })
    };
    until_vetoed(&rig).await;
    let failed_at = Instant::now();
    rig.fail.notify_one();
    let answer = timeout(Duration::from_secs(10), call)
        .await
        .expect("the call returns")
        .expect("joined")
        .expect("the supervisor answers");
    let took = failed_at.elapsed();
    let bound = WINDOW + WINDOW_GRACE + ABORT_SETTLE;
    let text = answer.expect_err("the call reports what it saw");
    assert!(text.contains("ChildUnresponsive"), "{text}");
    assert!(text.contains("flagged=true"), "{text}");
    assert!(
        took >= bound,
        "no verdict before the window, the grace and the settle time: {took:?}"
    );
    assert!(
        took < bound + STALL_SLACK,
        "and the verdict by that bound: {took:?}"
    );
    assert!(
        !rig.probe.ended.load(Ordering::SeqCst),
        "the stuck turn still runs"
    );
    drop(unsticks);
}

async fn an_infinity_worker_whose_runner_fails_is_still_judged_at_its_kill_grace() {
    // A worker has no orderly stop, whatever its own policy: a vetoing
    // worker in a turn that never yields is called unresponsive at its kill grace plus the
    // settle time, never earlier and never unbounded.
    let rig = rig_over_worker(Shutdown::Infinity, DEFAULT_KILL_GRACE).await;
    let unsticks = stuck_long(&rig);
    make_stuck(&rig).await;
    let call = {
        let sup = rig.sup.clone();
        let name = rig.name.clone();
        tokio::spawn(async move { sup.send(Cmd::TerminateThenVerdict(name)).await })
    };
    // The worker is inside its blocked turn and cannot acknowledge the
    // request, so nothing shows the call has committed; this head start only
    // lets it reach its wait before the runner fails. A call that is later
    // still waits from its own start, which the slack below absorbs.
    sleep(Duration::from_millis(50)).await;
    let failed_at = Instant::now();
    rig.fail.notify_one();
    let answer = timeout(Duration::from_secs(10), call)
        .await
        .expect("the call returns")
        .expect("joined")
        .expect("the supervisor answers");
    let took = failed_at.elapsed();
    let text = answer.expect_err("the call reports what it saw");
    assert!(text.contains("ChildUnresponsive"), "{text}");
    assert!(text.contains("flagged=true"), "{text}");
    assert!(
        took >= DEFAULT_KILL_GRACE + ABORT_SETTLE,
        "not before the kill grace and the settle time: {took:?}"
    );
    assert!(
        took < DEFAULT_KILL_GRACE + ABORT_SETTLE + STALL_SLACK,
        "and by that bound: {took:?}"
    );
    assert!(!rig.probe.ended.load(Ordering::SeqCst));
    drop(unsticks);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_waiting_on_an_infinity_supervisor_child_whose_runner_fails_gets_no_early_verdict_multi_thread(
) {
    a_caller_waiting_on_an_infinity_supervisor_child_whose_runner_fails_gets_no_early_verdict()
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_caller_waiting_on_an_infinity_supervisor_child_whose_runner_fails_gets_no_early_verdict_current_thread(
) {
    a_caller_waiting_on_an_infinity_supervisor_child_whose_runner_fails_gets_no_early_verdict()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_stop_waits_for_an_infinity_supervisor_child_whose_runner_failed_multi_thread() {
    a_parent_stop_waits_for_an_infinity_supervisor_child_whose_runner_failed().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_parent_stop_waits_for_an_infinity_supervisor_child_whose_runner_failed_current_thread() {
    a_parent_stop_waits_for_an_infinity_supervisor_child_whose_runner_failed().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_runner_wait_follows_a_supervisor_childs_window_first_multi_thread() {
    a_lost_runner_wait_follows_a_supervisor_childs_window_first(false).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_lost_runner_wait_follows_a_supervisor_childs_window_first_current_thread() {
    a_lost_runner_wait_follows_a_supervisor_childs_window_first(true).await;
}

// A blocked turn holds a worker thread until the test ends it and the waits
// are timed against it, so these two run on a multi-threaded runtime only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_supervisor_child_stuck_in_a_turn_gets_the_verdict_after_its_window_and_grace_multi_thread(
) {
    a_lost_supervisor_child_stuck_in_a_turn_gets_the_verdict_after_its_window_and_grace().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_infinity_worker_whose_runner_fails_is_still_judged_at_its_kill_grace_multi_thread() {
    an_infinity_worker_whose_runner_fails_is_still_judged_at_its_kill_grace().await;
}

/// The reason a supervisor child ended with, read three ways. The child
/// supervises one worker; its window is 2 s. `veto_worker` makes the worker
/// refuse every request under a 10 s policy, so the child's orderly stop
/// outlives the window and the child's own `Kill` stage ends it; otherwise
/// the child ends in order, as `ParentKilled`.
async fn a_lost_supervisor_child_reports_the_reason_it_ended_with(veto_worker: bool) {
    let window = Duration::from_secs(2);
    let want = |reason: &StopReason| {
        if veto_worker {
            matches!(reason, StopReason::Kill)
        } else {
            matches!(reason, StopReason::ParentKilled)
        }
    };
    let shape = |vetoes: bool| Shape {
        vetoes,
        own_stop: OwnStopMode::Quick,
        worker: Some((
            Duration::ZERO,
            if veto_worker {
                Shutdown::Timeout(Duration::from_secs(10))
            } else {
                Shutdown::Timeout(window)
            },
            veto_worker,
        )),
    };

    // (i) The replacement task's report, with no caller waiting.
    let rig =
        rig_over_supervisor(Shutdown::Timeout(window), DEFAULT_KILL_GRACE, shape(false)).await;
    rig.fail.notify_one();
    let (reason, action) = the_one_report(&rig).await;
    assert!(want(&reason), "replacement task: {reason:?}");
    assert_eq!(action, SupervisionAction::Removed);

    // (ii) An awaited `terminate_child` on the lost cell.
    let rig = rig_over_supervisor(Shutdown::Timeout(window), DEFAULT_KILL_GRACE, shape(true)).await;
    let call = {
        let sup = rig.sup.clone();
        let name = rig.name.clone();
        tokio::spawn(async move { sup.send(Cmd::Terminate(name)).await })
    };
    until_vetoed(&rig).await;
    rig.fail.notify_one();
    let answer = timeout(Duration::from_secs(10), call)
        .await
        .expect("the call returns")
        .expect("joined")
        .expect("supervisor answers");
    assert_eq!(answer, Ok(()), "awaited call");
    let (reason, action) = the_one_report(&rig).await;
    assert!(want(&reason), "awaited call: {reason:?}");
    assert_eq!(action, SupervisionAction::Removed);

    // (iii) A call made from a handler after the task ended, before the death
    // is drained, records the same reason.
    for terminate in [true, false] {
        let rig =
            rig_over_supervisor(Shutdown::Timeout(window), DEFAULT_KILL_GRACE, shape(false)).await;
        let answer = timeout(
            Duration::from_secs(15),
            rig.sup.send(Cmd::EndThenCall(rig.name.clone(), terminate)),
        )
        .await
        .expect("the handler answers")
        .expect("the supervisor answers");
        assert_eq!(answer, Ok(()), "terminate: {terminate}");
        let (reason, action) = the_one_report(&rig).await;
        assert!(
            want(&reason),
            "handler call, terminate {terminate}: {reason:?}"
        );
        assert_eq!(action, SupervisionAction::Removed);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_supervisor_child_reports_the_reason_it_ended_with_multi_thread() {
    a_lost_supervisor_child_reports_the_reason_it_ended_with(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_lost_supervisor_child_reports_the_reason_it_ended_with_current_thread() {
    a_lost_supervisor_child_reports_the_reason_it_ended_with(false).await;
}

// The control: the child's own Kill stage ends it at its window, and every
// reader reads that `Kill`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_supervisor_child_ended_by_its_kill_stage_reports_a_kill_multi_thread() {
    a_lost_supervisor_child_reports_the_reason_it_ended_with(true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_lost_supervisor_child_ended_by_its_kill_stage_reports_a_kill_current_thread() {
    a_lost_supervisor_child_reports_the_reason_it_ended_with(true).await;
}

// ---------------------------------------------------------------------------
// A waiter on the status plane runs before a callback can block the thread
// ---------------------------------------------------------------------------

/// An actor that notes, when `on_stopped` begins, whether a task waiting for
/// its `Stopping` status has already run.
struct NotesItsWaiter {
    waiter_ran: Arc<AtomicBool>,
    saw_the_waiter: Arc<AtomicBool>,
}

impl Actor for NotesItsWaiter {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.saw_the_waiter
            .store(self.waiter_ran.load(Ordering::SeqCst), Ordering::SeqCst);
        Ok(())
    }
}

/// With one thread, a task woken by the `Stopping` publish can run before
/// `on_stopped` only if the stopping task yields in between, which is what
/// keeps a thread-blocking `on_stopped` from starving the waiters on the
/// plane.
#[tokio::test(flavor = "current_thread")]
async fn a_waiter_on_the_status_plane_runs_before_on_stopped_begins() {
    let (waiter_ran, saw_the_waiter) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let handle = NotesItsWaiter {
        waiter_ran: waiter_ran.clone(),
        saw_the_waiter: saw_the_waiter.clone(),
    }
    .spawn()
    .named(uname("notes-its-waiter"))
    .await
    .expect("spawn");
    let mut plane = handle.status_rx();
    let flag = waiter_ran.clone();
    let waiter = tokio::spawn(async move {
        let _ = plane
            .wait_for(|value| value.status == ActorStatus::Stopping)
            .await;
        flag.store(true, Ordering::SeqCst);
    });
    // The waiter has registered its interest.
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    handle.stop(StopReason::Graceful).await.expect("stop");
    timeout(Duration::from_secs(5), handle.wait_stopped())
        .await
        .expect("the actor stops");
    waiter.await.expect("the waiter ends");
    assert!(
        saw_the_waiter.load(Ordering::SeqCst),
        "the waiter woken by the Stopping publish ran before on_stopped began"
    );
}

/// A supervisor that stops itself when it reports a restart it initiated,
/// leaving the restart attempt in flight, and notes, when `on_stopped`
/// begins, whether a task waiting for its `Stopping` status has already run.
struct StopsWithAnAttemptInFlight {
    child: String,
    waiter_ran: Arc<AtomicBool>,
    saw_the_waiter: Arc<AtomicBool>,
}

impl Actor for StopsWithAnAttemptInFlight {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Plain)
            .named(self.child.clone())
            .restart_type(RestartType::Permanent)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if event.action == SupervisionAction::RestartInitiated {
            ctx.self_handle()
                .stop(StopReason::Graceful)
                .await
                .expect("the supervisor stops itself");
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.saw_the_waiter
            .store(self.waiter_ran.load(Ordering::SeqCst), Ordering::SeqCst);
        Ok(())
    }
}

/// As above for a supervisor with a restart attempt in flight when it
/// publishes `Stopping`: the yield is the same for every actor.
#[tokio::test(flavor = "current_thread")]
async fn a_waiter_on_the_status_plane_runs_before_on_stopped_with_an_attempt_in_flight() {
    let (waiter_ran, saw_the_waiter) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let child = uname("attempt-child");
    let handle = StopsWithAnAttemptInFlight {
        child: child.clone(),
        waiter_ran: waiter_ran.clone(),
        saw_the_waiter: saw_the_waiter.clone(),
    }
    .spawn()
    .named(uname("attempt-sup"))
    .with_supervision(SupervisionConfig::default())
    .await
    .expect("spawn");
    let mut plane = handle.status_rx();
    let flag = waiter_ran.clone();
    let waiter = tokio::spawn(async move {
        let _ = plane
            .wait_for(|value| value.status == ActorStatus::Stopping)
            .await;
        flag.store(true, Ordering::SeqCst);
    });
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    let crashing = crate::system::ActorSystem::default()
        .get::<Plain>(&child)
        .expect("the child is registered");
    let _ = crashing.send(true).await;
    timeout(Duration::from_secs(5), handle.wait_stopped())
        .await
        .expect("the supervisor stops");
    waiter.await.expect("the waiter ends");
    assert!(
        saw_the_waiter.load(Ordering::SeqCst),
        "the waiter woken by the Stopping publish ran before on_stopped began"
    );
}

/// A supervisor whose turn holds until `release`, then waits until its
/// child's death is recorded (the death then waits in the plane) and stops
/// itself. In the delivery of that death with the supervisor stopping, it
/// notes whether a task waiting for its `Stopping` status has already run.
struct NotesItsWaiterAtDelivery {
    child: String,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    waiter_ran: Arc<AtomicBool>,
    delivered: Arc<AtomicBool>,
    saw_the_waiter: Arc<AtomicBool>,
}

impl Actor for NotesItsWaiterAtDelivery {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Plain)
            .named(self.child.clone())
            .restart_type(RestartType::Temporary)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        while ctx.children().iter().any(|child| child.is_alive) {
            tokio::task::yield_now().await;
        }
        ctx.self_handle()
            .stop(StopReason::Graceful)
            .await
            .expect("the supervisor stops itself");
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if event.action == SupervisionAction::SupervisorStopping {
            self.saw_the_waiter
                .store(self.waiter_ran.load(Ordering::SeqCst), Ordering::SeqCst);
            self.delivered.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// As above for a supervisor whose first callback of the stop is the
/// delivery of a death still pending when its loop ended: the waiter runs
/// before that delivery begins, not only before `on_stopped`. The death's
/// runner has ended before the stop, so collecting it never waits.
#[tokio::test(flavor = "current_thread")]
async fn a_waiter_on_the_status_plane_runs_before_the_first_final_delivery() {
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let (waiter_ran, delivered, saw_the_waiter) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let child = uname("pending-death-child");
    let handle = NotesItsWaiterAtDelivery {
        child: child.clone(),
        entered: entered.clone(),
        release: release.clone(),
        waiter_ran: waiter_ran.clone(),
        delivered: delivered.clone(),
        saw_the_waiter: saw_the_waiter.clone(),
    }
    .spawn()
    .named(uname("pending-death-sup"))
    .with_supervision(SupervisionConfig::default())
    .await
    .expect("spawn");
    let mut plane = handle.status_rx();
    let (flag, registered) = (waiter_ran.clone(), Arc::new(AtomicBool::new(false)));
    let parked = registered.clone();
    let waiter = tokio::spawn(async move {
        // Set before the first poll of the wait, which registers it without
        // yielding in between.
        parked.store(true, Ordering::SeqCst);
        let _ = plane
            .wait_for(|value| value.status == ActorStatus::Stopping)
            .await;
        flag.store(true, Ordering::SeqCst);
    });
    while !registered.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    handle.notify(()).await.expect("notify");
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("the turn begins");
    // The child dies while the turn holds, so its death waits in the plane.
    let crashing = crate::system::ActorSystem::default()
        .get::<Plain>(&child)
        .expect("the child is registered");
    let _ = crashing.send(true).await;
    release.notify_one();
    timeout(Duration::from_secs(5), handle.wait_stopped())
        .await
        .expect("the supervisor stops");
    waiter.await.expect("the waiter ends");
    assert!(
        delivered.load(Ordering::SeqCst),
        "the death was delivered with the supervisor stopping"
    );
    assert!(
        saw_the_waiter.load(Ordering::SeqCst),
        "the waiter woken by the Stopping publish ran before the first final delivery"
    );
}
