#![allow(dead_code)]
//! A supervised child placed on another runtime with `on_runtime`: every
//! incarnation, restart and grandchild runs there, the kill grace is kept on
//! the child's own runtime, a runtime that is gone or not driven ends in a
//! bounded way, and a runtime without the time driver is refused at the call.

mod common;

use std::future::{Future, IntoFuture};
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::runtime::{Builder, Handle, Id, Runtime};
use tokio_actors::{
    actor::context::ActorContext, Actor, ActorConfig, ActorExt, ActorHandle, ActorResult,
    ActorSystem, ChildEvent, ChildInfo, RestartType, Shutdown, SpawnError, StopReason,
    SupervisionAction, SupervisionConfig, SupervisionError,
};

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

fn unique(base: &str) -> String {
    format!(
        "placement-{base}-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

// ---------------------------------------------------------------------------
// Runtimes
// ---------------------------------------------------------------------------

/// The thread that drives a current-thread runtime, and the means to stop it.
struct Driver {
    stop: Mutex<Option<mpsc::Sender<()>>>,
    done: Mutex<mpsc::Receiver<()>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Driver {
    /// Stops the thread and waits until it has returned, so the runtime is
    /// alive and nobody polls it. Does nothing the second time.
    fn stop(&self) {
        let Some(stop) = self.stop.lock().unwrap().take() else {
            return;
        };
        let _ = stop.send(());
        self.done
            .lock()
            .unwrap()
            .recv_timeout(HANG_BOUND)
            .expect("the driver returns");
        if let Some(thread) = self.thread.lock().unwrap().take() {
            thread.join().unwrap();
        }
    }
}

/// A runtime a child is placed on.
struct Placed {
    runtime: Option<Arc<Runtime>>,
    handle: Handle,
    driver: Option<Arc<Driver>>,
}

impl Placed {
    /// A multi-thread runtime with one worker: it drives itself.
    fn multi_thread() -> Self {
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("placed")
            .enable_all()
            .build()
            .unwrap();
        Self {
            handle: runtime.handle().clone(),
            runtime: Some(Arc::new(runtime)),
            driver: None,
        }
    }

    /// A current-thread runtime that a dedicated thread drives until the
    /// driver is stopped.
    fn driven() -> Self {
        let runtime = Arc::new(Builder::new_current_thread().enable_all().build().unwrap());
        let handle = runtime.handle().clone();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let driven = runtime.clone();
        let thread = std::thread::spawn(move || {
            let (end_tx, end_rx) = tokio::sync::oneshot::channel::<()>();
            std::thread::spawn(move || {
                let _ = stop_rx.recv();
                let _ = end_tx.send(());
            });
            driven.block_on(async {
                let _ = end_rx.await;
            });
            let _ = done_tx.send(());
        });
        Self {
            handle,
            runtime: Some(runtime),
            driver: Some(Arc::new(Driver {
                stop: Mutex::new(Some(stop_tx)),
                done: Mutex::new(done_rx),
                thread: Mutex::new(Some(thread)),
            })),
        }
    }

    /// A current-thread runtime that nothing ever drives.
    fn undriven() -> Self {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        Self {
            handle: runtime.handle().clone(),
            runtime: Some(Arc::new(runtime)),
            driver: None,
        }
    }

    fn handle(&self) -> Handle {
        self.handle.clone()
    }

    fn id(&self) -> Id {
        self.handle.id()
    }

    /// Ends the runtime, from a thread that is not inside one.
    fn end(&mut self) {
        if let Some(driver) = &self.driver {
            driver.stop();
        }
        if let Some(runtime) = self.runtime.take() {
            std::thread::spawn(move || drop(runtime)).join().unwrap();
        }
    }
}

impl Drop for Placed {
    fn drop(&mut self) {
        self.end();
    }
}

/// A runtime of the flavor the suite runs under, for a test that owns its
/// supervisor's runtime.
fn flavored_runtime() -> Runtime {
    if std::env::var("TOKIO_ACTORS_TEST_FLAVOR").as_deref() == Ok("current_thread") {
        Builder::new_current_thread().enable_all().build().unwrap()
    } else {
        Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }
}

/// Polls `condition` until it holds. The bound only limits how long a real hang takes to report.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + HANG_BOUND;
    while !condition() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// Actors
// ---------------------------------------------------------------------------

/// Where each incarnation of a worker started.
type Log = Arc<Mutex<Vec<(&'static str, Id)>>>;

fn starts_of(log: &Log, tag: &str) -> Vec<Id> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(t, _)| *t == tag)
        .map(|(_, id)| *id)
        .collect()
}

enum WorkerMsg {
    Crash,
    /// Holds the turn open for good: only an abort ends it.
    Park,
    /// Starts a plain child of that name, which reports under the tag
    /// "grandchild".
    Grandchild(String),
    /// Starts a plain child of that name on the given runtime, with a short
    /// stop window.
    PlacedGrandchild(String, Handle),
}

struct Worker {
    log: Log,
    tag: &'static str,
}

impl Actor for Worker {
    type Message = WorkerMsg;
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.log
            .lock()
            .unwrap()
            .push((self.tag, Handle::current().id()));
        Ok(())
    }

    async fn handle(&mut self, msg: WorkerMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            WorkerMsg::Crash => panic!("worker crash"),
            WorkerMsg::Park => {
                self.log
                    .lock()
                    .unwrap()
                    .push(("in-turn", Handle::current().id()));
                std::future::pending::<()>().await
            }
            WorkerMsg::PlacedGrandchild(name, runtime) => {
                let log = self.log.clone();
                ctx.spawn_child(move || Worker {
                    log: log.clone(),
                    tag: "grandchild",
                })
                .named(name)
                .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
                .kill_grace(Duration::from_millis(300))
                .on_runtime(&runtime)
                .await?;
            }
            WorkerMsg::Grandchild(name) => {
                let log = self.log.clone();
                ctx.spawn_child(move || Worker {
                    log: log.clone(),
                    tag: "grandchild",
                })
                .named(name)
                .await?;
            }
        }
        Ok(())
    }
}

/// What the supervisor is told to spawn.
#[derive(Clone)]
struct Spec {
    name: String,
    tag: &'static str,
    log: Log,
    runtime: Option<Handle>,
    shutdown: Shutdown,
    grace: Duration,
    start_timeout: Option<Duration>,
    as_supervisor: bool,
    /// Called by the factory from the second incarnation on.
    on_restart: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Counts every call of the factory.
    built: Arc<AtomicUsize>,
}

impl Spec {
    fn new(log: &Log, tag: &'static str) -> Self {
        Self {
            name: unique(tag),
            tag,
            log: log.clone(),
            runtime: None,
            shutdown: Shutdown::Timeout(Duration::from_millis(200)),
            grace: Duration::from_millis(300),
            start_timeout: None,
            as_supervisor: false,
            on_restart: None,
            built: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn on(mut self, placed: &Placed) -> Self {
        self.runtime = Some(placed.handle());
        self
    }
}

enum Cmd {
    Spawn(Spec),
    /// Awaits the spawn once, in a poll that catches a panic.
    SpawnCatching(Spec),
    StopChild(String),
    TerminateChild(String),
    Children,
}

enum Reply {
    Spawned(Result<(), SpawnError>),
    Supervision(Result<(), SupervisionError>),
    Roster(Vec<ChildInfo>),
    /// The panic text of a spawn that panicked, and how many children the
    /// supervisor held afterwards.
    Caught(Option<String>, usize),
}

struct Supervisor {
    events: Arc<Mutex<Vec<ChildEvent>>>,
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .unwrap_or_default(),
    }
}

impl Supervisor {
    fn builder<'a>(
        ctx: &'a mut ActorContext<Self>,
        spec: &Spec,
    ) -> tokio_actors::ChildSpawnBuilder<
        'a,
        Self,
        impl Fn() -> Worker + Send + Sync + 'static,
        Worker,
    > {
        let log = spec.log.clone();
        let tag = spec.tag;
        let built = spec.built.clone();
        let on_restart = spec.on_restart.clone();
        let mut builder = ctx
            .spawn_child(move || {
                if built.fetch_add(1, Ordering::SeqCst) > 0 {
                    if let Some(hook) = &on_restart {
                        hook();
                    }
                }
                Worker {
                    log: log.clone(),
                    tag,
                }
            })
            .named(spec.name.clone())
            .restart_type(RestartType::Permanent)
            .shutdown(spec.shutdown)
            .kill_grace(spec.grace);
        if let Some(runtime) = &spec.runtime {
            builder = builder.on_runtime(runtime);
        }
        if let Some(timeout) = spec.start_timeout {
            builder = builder.start_timeout(timeout);
        }
        if spec.as_supervisor {
            builder = builder.with_config(ActorConfig {
                supervision: Some(SupervisionConfig::one_for_one()),
                ..Default::default()
            });
        }
        builder
    }
}

impl Actor for Supervisor {
    type Message = Cmd;
    type Response = Reply;

    async fn handle(&mut self, cmd: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        Ok(match cmd {
            Cmd::Spawn(spec) => Reply::Spawned(
                Self::builder(ctx, &spec)
                    .await
                    .map(|_: ActorHandle<Worker>| ()),
            ),
            Cmd::SpawnCatching(spec) => {
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut future = Self::builder(ctx, &spec).into_future();
                    let mut cx = Context::from_waker(Waker::noop());
                    matches!(pin!(&mut future).poll(&mut cx), Poll::Ready(Ok(_)))
                }));
                match caught {
                    Ok(_) => Reply::Caught(None, ctx.children().len()),
                    Err(payload) => Reply::Caught(Some(panic_text(payload)), ctx.children().len()),
                }
            }
            Cmd::StopChild(name) => Reply::Supervision(ctx.stop_child(name).await.map(|_| ())),
            Cmd::TerminateChild(name) => {
                Reply::Supervision(ctx.terminate_child(name).await.map(|_| ()))
            }
            Cmd::Children => Reply::Roster(ctx.children()),
        })
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

async fn spawn_supervisor(
    config: SupervisionConfig,
    events: &Arc<Mutex<Vec<ChildEvent>>>,
) -> ActorHandle<Supervisor> {
    Supervisor {
        events: events.clone(),
    }
    .spawn()
    .named(unique("supervisor"))
    .with_supervision(config)
    .await
    .unwrap()
}

async fn spawn_child_via(sup: &ActorHandle<Supervisor>, spec: &Spec) -> Result<(), SpawnError> {
    match sup.send(Cmd::Spawn(spec.clone())).await.unwrap() {
        Reply::Spawned(result) => result,
        _ => unreachable!(),
    }
}

async fn supervision_via(sup: &ActorHandle<Supervisor>, cmd: Cmd) -> Result<(), SupervisionError> {
    match sup.send(cmd).await.unwrap() {
        Reply::Supervision(result) => result,
        _ => unreachable!(),
    }
}

async fn roster(sup: &ActorHandle<Supervisor>) -> Vec<ChildInfo> {
    match sup.send(Cmd::Children).await.unwrap() {
        Reply::Roster(children) => children,
        _ => unreachable!(),
    }
}

fn worker(name: &str) -> ActorHandle<Worker> {
    ActorSystem::default()
        .get::<Worker>(name)
        .expect("the worker is registered")
}

/// Parks the worker `name` in a turn and waits until it is in it.
async fn park(log: &Log, name: &str) {
    let before = starts_of(log, "in-turn").len();
    worker(name).notify(WorkerMsg::Park).await.unwrap();
    until("the worker is parked", || {
        starts_of(log, "in-turn").len() > before
    })
    .await;
}

/// Ends a runtime from a thread that is not inside one.
fn end_runtime(runtime: Runtime) {
    std::thread::spawn(move || drop(runtime)).join().unwrap();
}

fn child_events(
    events: &Arc<Mutex<Vec<ChildEvent>>>,
    name: &str,
) -> Vec<(StopReason, SupervisionAction)> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.child_name.as_deref() == Some(name))
        .map(|event| (event.reason.clone(), event.action))
        .collect()
}

// ---------------------------------------------------------------------------
// Placement holds for every incarnation
// ---------------------------------------------------------------------------

/// The first incarnation, a crash restart, a bounce by `stop_child` and a
/// group restart started by a sibling's crash all run on the placed runtime,
/// while the unplaced sibling stays on the supervisor's.
fn placement_survives_every_kind_of_restart(make: fn() -> Placed) {
    let placed = make();
    let placed_id = placed.id();
    common::run(2, async move {
        let supervisor_runtime = Handle::current().id();
        assert_ne!(placed_id, supervisor_runtime);
        let log: Log = Default::default();
        let sup = spawn_supervisor(
            SupervisionConfig::one_for_all().max_restarts(20, Duration::from_secs(60)),
            &Default::default(),
        )
        .await;
        let far = Spec::new(&log, "far").on(&placed);
        let near = Spec::new(&log, "near");
        spawn_child_via(&sup, &far).await.unwrap();
        spawn_child_via(&sup, &near).await.unwrap();
        until("both first incarnations", || {
            starts_of(&log, "far").len() == 1 && starts_of(&log, "near").len() == 1
        })
        .await;

        // A crash of the placed child restarts the whole group.
        let _ = worker(&far.name).send(WorkerMsg::Crash).await;
        until("the crash restart", || {
            starts_of(&log, "far").len() == 2 && starts_of(&log, "near").len() == 2
        })
        .await;

        // A bounce restarts the placed child alone.
        supervision_via(&sup, Cmd::StopChild(far.name.clone()))
            .await
            .unwrap();
        until("the bounce", || starts_of(&log, "far").len() == 3).await;

        // A crash of the unplaced child restarts the group, the placed child
        // included.
        let _ = worker(&near.name).send(WorkerMsg::Crash).await;
        until("the group restart", || {
            starts_of(&log, "far").len() >= 4 && starts_of(&log, "near").len() >= 3
        })
        .await;

        assert!(starts_of(&log, "far").iter().all(|id| *id == placed_id));
        assert!(starts_of(&log, "near")
            .iter()
            .all(|id| *id == supervisor_runtime));
        let _ = sup.stop(StopReason::Graceful).await;
        sup.wait_stopped().await;
    });
}

#[test]
fn placement_survives_every_kind_of_restart_on_a_multi_thread_runtime() {
    placement_survives_every_kind_of_restart(Placed::multi_thread);
}

/// A placed child that supervises in turn starts its own children on the
/// placed runtime, before and after one of its restarts.
fn grandchild_of_a_placed_child_inherits_the_runtime(make: fn() -> Placed) {
    let placed = make();
    let placed_id = placed.id();
    common::run(2, async move {
        let log: Log = Default::default();
        let sup = spawn_supervisor(
            SupervisionConfig::one_for_one().max_restarts(20, Duration::from_secs(60)),
            &Default::default(),
        )
        .await;
        let mut middle = Spec::new(&log, "middle").on(&placed);
        middle.as_supervisor = true;
        spawn_child_via(&sup, &middle).await.unwrap();
        until("the middle child", || starts_of(&log, "middle").len() == 1).await;

        worker(&middle.name)
            .send(WorkerMsg::Grandchild(unique("grand")))
            .await
            .unwrap();
        until("the first grandchild", || {
            starts_of(&log, "grandchild").len() == 1
        })
        .await;

        let _ = worker(&middle.name).send(WorkerMsg::Crash).await;
        until("the restart of the middle child", || {
            starts_of(&log, "middle").len() == 2
        })
        .await;
        worker(&middle.name)
            .send(WorkerMsg::Grandchild(unique("grand")))
            .await
            .unwrap();
        until("the second grandchild", || {
            starts_of(&log, "grandchild").len() >= 2
        })
        .await;

        assert!(starts_of(&log, "middle").iter().all(|id| *id == placed_id));
        assert!(starts_of(&log, "grandchild")
            .iter()
            .all(|id| *id == placed_id));
        let _ = sup.stop(StopReason::Graceful).await;
        sup.wait_stopped().await;
    });
}

#[test]
fn on_runtime_with_the_supervisors_own_runtime_changes_nothing() {
    common::run(2, async {
        let here = Handle::current();
        let log: Log = Default::default();
        let sup = spawn_supervisor(SupervisionConfig::one_for_one(), &Default::default()).await;
        let mut spec = Spec::new(&log, "same");
        spec.runtime = Some(here.clone());
        spawn_child_via(&sup, &spec).await.unwrap();
        until("the child", || starts_of(&log, "same").len() == 1).await;
        assert_eq!(starts_of(&log, "same"), vec![here.id()]);
        let _ = worker(&spec.name).send(WorkerMsg::Crash).await;
        until("the restart", || starts_of(&log, "same").len() == 2).await;
        assert_eq!(starts_of(&log, "same"), vec![here.id(), here.id()]);
        let _ = sup.stop(StopReason::Graceful).await;
        sup.wait_stopped().await;
    });
}

// ---------------------------------------------------------------------------
// The kill grace stays on the child's runtime
// ---------------------------------------------------------------------------

/// The supervisor's runtime ends while its placed child is parked in a turn:
/// the child is given its whole grace, because the grace runs on the child's
/// runtime. A reaper on the ended runtime would abort it at once.
fn supervisor_runtime_ending_first_keeps_the_childs_grace(make: fn() -> Placed) {
    let placed = make();
    let supervisor_runtime = flavored_runtime();
    let log: Log = Default::default();
    let mut spec = Spec::new(&log, "target").on(&placed);
    spec.grace = Duration::from_millis(400);
    let name = spec.name.clone();
    let (ended_tx, ended_rx) = mpsc::channel::<Duration>();
    let started = Arc::new(Mutex::new(None::<Instant>));
    supervisor_runtime.block_on({
        let log = log.clone();
        let placed_handle = placed.handle();
        let started = started.clone();
        async move {
            let sup = spawn_supervisor(SupervisionConfig::one_for_one(), &Default::default()).await;
            spawn_child_via(&sup, &spec).await.unwrap();
            until("the child", || starts_of(&log, "target").len() == 1).await;
            park(&log, &name).await;
            let watcher = worker(&name);
            placed_handle.spawn(async move {
                watcher.wait_stopped().await;
                let at = (*started.lock().unwrap()).expect("the clock was read");
                let _ = ended_tx.send(at.elapsed());
            });
        }
    });
    *started.lock().unwrap() = Some(Instant::now());
    end_runtime(supervisor_runtime);
    let took = ended_rx
        .recv_timeout(HANG_BOUND)
        .expect("the child ends after the grace");
    assert!(
        took >= Duration::from_millis(300),
        "the child ended {took:?} after its supervisor's runtime, before its grace of 400 ms"
    );
    assert!(took < Duration::from_secs(5), "the child ended in {took:?}");
}

#[test]
fn supervisor_runtime_ending_first_keeps_the_childs_grace_on_a_multi_thread_runtime() {
    supervisor_runtime_ending_first_keeps_the_childs_grace(Placed::multi_thread);
}

/// What ends a parked placed child, once the supervisor's runtime is gone.
enum Ender {
    KillById,
    KillByName,
    StopKill,
    StopWithinKill,
}

/// Every kill path takes its grace from the child's own runtime: after the
/// supervisor's runtime has ended, a kill raised from a third runtime still
/// leaves the child its grace, which the guard of the ended supervisor armed
/// on the child's runtime. A kill backstop on the ended runtime would abort
/// the child at once.
fn a_kill_from_a_third_runtime_waits_the_childs_grace(how: Ender) {
    let placed = Placed::multi_thread();
    let supervisor_runtime = flavored_runtime();
    let log: Log = Default::default();
    let mut spec = Spec::new(&log, "target").on(&placed);
    spec.grace = Duration::from_millis(800);
    let name = spec.name.clone();
    supervisor_runtime.block_on({
        let log = log.clone();
        let spec = spec.clone();
        async move {
            let sup = spawn_supervisor(SupervisionConfig::one_for_one(), &Default::default()).await;
            spawn_child_via(&sup, &spec).await.unwrap();
            until("the child", || starts_of(&log, "target").len() == 1).await;
            park(&log, &spec.name).await;
        }
    });
    let child = worker(&name);
    let started = Instant::now();
    end_runtime(supervisor_runtime);
    let third = flavored_runtime();
    let took = third.block_on(async move {
        let kill = {
            let child = child.clone();
            let name = name.clone();
            async move {
                match how {
                    Ender::KillById => {
                        ActorSystem::default().kill_by_id(child.id()).await;
                    }
                    Ender::KillByName => {
                        let _ = ActorSystem::default().kill(&name).await;
                    }
                    Ender::StopKill => {
                        let _ = child.stop(StopReason::Kill).await;
                    }
                    Ender::StopWithinKill => {
                        let _ = child
                            .stop_within(StopReason::Kill, Duration::from_secs(5))
                            .await;
                    }
                }
            }
        };
        let kill = tokio::spawn(kill);
        tokio::time::timeout(HANG_BOUND, child.wait_stopped())
            .await
            .expect("the child ends");
        let took = started.elapsed();
        let _ = kill.await;
        took
    });
    assert!(
        took >= Duration::from_millis(600),
        "the child ended {took:?} after its supervisor's runtime, before its grace of 800 ms"
    );
    end_runtime(third);
    drop(placed);
}

// ---------------------------------------------------------------------------
// A placed runtime that is gone
// ---------------------------------------------------------------------------

/// The events of a child whose runtime is gone: each incarnation is killed,
/// each restart is initiated and fails, and the restart budget ends the
/// supervisor.
fn gone_runtime_events(max_restarts: u32) -> Vec<(StopReason, SupervisionAction)> {
    let mut events = Vec::new();
    for _ in 0..max_restarts {
        events.push((StopReason::Kill, SupervisionAction::RestartInitiated));
    }
    events.push((StopReason::Kill, SupervisionAction::BudgetExhausted));
    events
}

#[test]
fn a_child_whose_runtime_is_gone_while_it_runs_spends_the_budget() {
    let mut placed = Placed::multi_thread();
    common::run(2, async move {
        let events: Arc<Mutex<Vec<ChildEvent>>> = Default::default();
        let log: Log = Default::default();
        let sup = spawn_supervisor(
            SupervisionConfig::one_for_one().max_restarts(2, Duration::from_secs(60)),
            &events,
        )
        .await;
        let near = Spec::new(&log, "near");
        spawn_child_via(&sup, &near).await.unwrap();
        let far = Spec::new(&log, "far").on(&placed);
        spawn_child_via(&sup, &far).await.unwrap();
        until("the placed child", || starts_of(&log, "far").len() == 1).await;
        let sibling = worker(&near.name);

        placed.end();

        let reason = tokio::time::timeout(HANG_BOUND, sup.wait_stopped())
            .await
            .expect("the supervisor ends");
        assert_eq!(reason, StopReason::ParentRequest);
        assert_eq!(child_events(&events, &far.name), gone_runtime_events(2));
        assert_eq!(starts_of(&log, "far").len(), 1, "no restart ever starts");
        tokio::time::timeout(HANG_BOUND, sibling.wait_stopped())
            .await
            .expect("the sibling ends with the supervisor");
        assert!(ActorSystem::default().get::<Worker>(&far.name).is_none());
        assert!(!ActorSystem::default()
            .actor_ids()
            .contains(&far.name.as_str().into()));
    });
}

// ---------------------------------------------------------------------------
// A placed runtime that nothing drives
// ---------------------------------------------------------------------------

#[test]
fn an_undriven_placed_runtime_gives_stop_child_a_bounded_verdict() {
    let placed = Placed::undriven();
    common::run(2, async move {
        let log: Log = Default::default();
        let sup = spawn_supervisor(SupervisionConfig::one_for_one(), &Default::default()).await;
        let far = Spec::new(&log, "far").on(&placed);
        spawn_child_via(&sup, &far).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(starts_of(&log, "far").is_empty(), "nothing runs the child");

        let asked = Instant::now();
        let answer = tokio::time::timeout(
            HANG_BOUND,
            supervision_via(&sup, Cmd::StopChild(far.name.clone())),
        )
        .await
        .expect("the answer is bounded");
        let took = asked.elapsed();
        assert!(
            matches!(answer, Err(SupervisionError::ChildUnresponsive(_))),
            "{answer:?}"
        );
        assert!(
            took >= Duration::from_millis(450),
            "the whole ladder ran: {took:?}"
        );
        assert!(took < Duration::from_secs(5), "answered after {took:?}");
    });
}

/// A supervisor stopped from above, with a grandchild on a runtime nothing
/// drives, ends once the placed grandchild has its verdict, which comes from
/// the ladder on the supervisor's runtime and not from the grandchild's own.
#[test]
fn a_stop_from_above_with_an_undriven_placed_grandchild_is_bounded() {
    let placed = Placed::undriven();
    common::run(2, async move {
        let log: Log = Default::default();
        let top = spawn_supervisor(SupervisionConfig::one_for_one(), &Default::default()).await;
        let mut middle = Spec::new(&log, "middle");
        middle.as_supervisor = true;
        middle.shutdown = Shutdown::Timeout(Duration::from_secs(60));
        spawn_child_via(&top, &middle).await.unwrap();
        until("the middle child", || starts_of(&log, "middle").len() == 1).await;
        worker(&middle.name)
            .send(WorkerMsg::PlacedGrandchild(
                unique("grand"),
                placed.handle(),
            ))
            .await
            .unwrap();
        assert!(starts_of(&log, "grandchild").is_empty());

        let asked = Instant::now();
        let _ = top.stop(StopReason::Graceful).await;
        tokio::time::timeout(HANG_BOUND, top.wait_stopped())
            .await
            .expect("the grandparent ends");
        let took = asked.elapsed();
        // The verdict for the placed grandchild takes its window, grace and
        // settle (600 ms); the middle child's own window is 60 s.
        assert!(took >= Duration::from_millis(500), "ended after {took:?}");
        assert!(
            took < Duration::from_secs(20),
            "the verdict came from the supervisor's runtime: {took:?}"
        );
    });
}

/// A restart that lands on a runtime that stopped being driven leaves the
/// child restart-pending: no incarnation, no charge beyond the crash that
/// asked for it, and the manual operations answer `ChildRestarting`.
#[test]
fn a_restart_onto_an_undriven_runtime_stays_pending_and_charges_nothing() {
    let placed = Placed::driven();
    let driver = placed.driver.clone().unwrap();
    let events: Arc<Mutex<Vec<ChildEvent>>> = Default::default();
    let log: Log = Default::default();
    let mut far = Spec::new(&log, "far").on(&placed);
    far.start_timeout = Some(Duration::from_millis(300));
    // The factory of the restart stops the thread that drives the runtime
    // and waits for it, so the new incarnation is spawned onto an idle one.
    far.on_restart = Some(Arc::new(move || driver.stop()));
    let far_name = far.name.clone();
    common::run(2, async move {
        let sup = spawn_supervisor(
            SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(60)),
            &events,
        )
        .await;
        spawn_child_via(&sup, &far).await.unwrap();
        until("the first incarnation", || {
            starts_of(&log, "far").len() == 1
        })
        .await;
        let _ = worker(&far_name).send(WorkerMsg::Crash).await;

        // Three times the start bound of the child: it does not end the wait.
        tokio::time::sleep(Duration::from_millis(900)).await;
        let children = roster(&sup).await;
        let child = children
            .iter()
            .find(|c| c.name.as_deref() == Some(far_name.as_str()))
            .expect("the child is on the roster");
        assert!(!child.is_alive, "{child:?}");
        assert!(child.restart_pending, "{child:?}");
        assert_eq!(starts_of(&log, "far").len(), 1, "no second incarnation");
        assert_eq!(
            child_events(&events, &far_name).len(),
            1,
            "only the crash was charged"
        );
        let answer = tokio::time::timeout(
            HANG_BOUND,
            supervision_via(&sup, Cmd::TerminateChild(far_name.clone())),
        )
        .await
        .expect("answered at once");
        assert!(
            matches!(answer, Err(SupervisionError::ChildRestarting(_))),
            "{answer:?}"
        );
        let stopping = Instant::now();
        let _ = sup.stop(StopReason::Graceful).await;
        tokio::time::timeout(HANG_BOUND, sup.wait_stopped())
            .await
            .expect("the supervisor stops while a restart is pending");
        assert!(stopping.elapsed() < Duration::from_secs(3));
    });
    drop(placed);
}

// ---------------------------------------------------------------------------
// A runtime without the time driver is refused at the call
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A group restart onto a runtime that stops being driven
// ---------------------------------------------------------------------------

/// Under `OneForAll`, a member placed on a runtime that nothing drives any
/// more cannot start again: with a start timeout on it, the supervisor stops
/// with the unresponsive verdict for that member once the timeout, the kill
/// grace and the settle have passed.
#[test]
fn a_group_restart_onto_an_undriven_runtime_stops_the_supervisor_with_the_unresponsive_verdict() {
    let placed = Placed::driven();
    let driver = placed.driver.clone().unwrap();
    let log: Log = Default::default();
    let mut far = Spec::new(&log, "far").on(&placed);
    far.start_timeout = Some(Duration::from_millis(300));
    // The runtime stops being driven when the second incarnation is built.
    far.on_restart = Some(Arc::new(move || driver.stop()));
    let near = Spec::new(&log, "near");
    let far_name = far.name.clone();
    let near_name = near.name.clone();
    common::run(2, async move {
        let sup = spawn_supervisor(
            SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60)),
            &Default::default(),
        )
        .await;
        spawn_child_via(&sup, &far).await.unwrap();
        spawn_child_via(&sup, &near).await.unwrap();
        until("both first incarnations", || {
            starts_of(&log, "far").len() == 1 && starts_of(&log, "near").len() == 1
        })
        .await;
        let _ = worker(&near_name).send(WorkerMsg::Crash).await;
        let reason = tokio::time::timeout(Duration::from_secs(30), sup.wait_stopped())
            .await
            .expect("the supervisor stops");
        match reason {
            StopReason::Failure(error) => {
                let text = format!("{error:?}");
                assert!(
                    text.contains("ChildUnresponsive") && text.contains(&far_name),
                    "{text}"
                );
            }
            other => panic!("unexpected stop reason {other:?}"),
        }
    });
    drop(placed);
}
