//! A restart of a placed child onto a runtime that stopped being driven: the
//! restart stays pending, `stop_child` and `terminate_child` answer
//! `ChildRestarting`, and the restart lands once the runtime is driven again;
//! under `RestForOne` a group restart that waits for the child with a start
//! timeout stops the supervisor with the unresponsive verdict.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::runtime::{Builder, Handle, Id, Runtime};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{oneshot, Notify};
use tokio_actors::{
    actor::context::ActorContext, Actor, ActorExt, ActorHandle, ActorResult, ActorSystem,
    ChildEvent, ChildInfo, RestartType, Shutdown, StopReason, SupervisionAction, SupervisionConfig,
    SupervisionError,
};

/// The longest a wait for an event may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

fn unique(base: &str) -> String {
    format!(
        "undriven-restart-{base}-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

// ---------------------------------------------------------------------------
// A current-thread runtime that threads drive and stop driving
// ---------------------------------------------------------------------------

/// The thread that drives a runtime, and the means to stop it.
struct Driver {
    stop: Mutex<Option<oneshot::Sender<()>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Driver {
    fn start(runtime: Arc<Runtime>) -> Arc<Self> {
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            runtime.block_on(async {
                let _ = stop_rx.await;
            });
        });
        Arc::new(Self {
            stop: Mutex::new(Some(stop_tx)),
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Stops the thread and waits until it has returned, so the runtime is
    /// alive and nobody polls it. Does nothing the second time.
    fn stop(&self) {
        let Some(stop) = self.stop.lock().unwrap().take() else {
            return;
        };
        let _ = stop.send(());
        if let Some(thread) = self.thread.lock().unwrap().take() {
            thread.join().unwrap();
        }
    }
}

/// A current-thread runtime a child is placed on, driven from the start.
struct Placed {
    runtime: Option<Arc<Runtime>>,
    handle: Handle,
    drivers: Mutex<Vec<Arc<Driver>>>,
}

impl Placed {
    fn driven() -> Self {
        let runtime = Arc::new(Builder::new_current_thread().enable_all().build().unwrap());
        let placed = Self {
            handle: runtime.handle().clone(),
            runtime: Some(runtime),
            drivers: Mutex::new(Vec::new()),
        };
        placed.drive_again();
        placed
    }

    fn id(&self) -> Id {
        self.handle.id()
    }

    /// Starts a thread that drives the runtime, returning its driver.
    fn drive_again(&self) -> Arc<Driver> {
        let driver = Driver::start(self.runtime.clone().expect("the runtime is alive"));
        self.drivers.lock().unwrap().push(driver.clone());
        driver
    }

    /// The driver that is running now.
    fn current_driver(&self) -> Arc<Driver> {
        self.drivers.lock().unwrap().last().unwrap().clone()
    }
}

impl Drop for Placed {
    fn drop(&mut self) {
        for driver in self.drivers.lock().unwrap().iter() {
            driver.stop();
        }
        if let Some(runtime) = self.runtime.take() {
            std::thread::spawn(move || drop(runtime)).join().unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// Actors
// ---------------------------------------------------------------------------

/// Where each incarnation started: its tag and the runtime it ran on.
type Start = (&'static str, Id);

enum WorkerMsg {
    Crash,
}

struct Worker {
    tag: &'static str,
    starts: UnboundedSender<Start>,
}

impl Actor for Worker {
    type Message = WorkerMsg;
    type Response = ();

    async fn on_started(&mut self, _: &mut ActorContext<Self>) -> ActorResult<()> {
        let _ = self.starts.send((self.tag, Handle::current().id()));
        Ok(())
    }

    async fn handle(&mut self, _: WorkerMsg, _: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("worker crash");
    }
}

/// What the supervisor starts: a child, and what its factory does from the
/// second incarnation on.
struct Spec {
    name: String,
    tag: &'static str,
    runtime: Option<Handle>,
    start_timeout: Option<Duration>,
    on_restart: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Spec {
    fn new(tag: &'static str) -> Self {
        Self {
            name: unique(tag),
            tag,
            runtime: None,
            start_timeout: None,
            on_restart: None,
        }
    }
}

enum Cmd {
    StopChild(String),
    TerminateChild(String),
    Children,
}

enum Reply {
    Supervision(Result<(), SupervisionError>),
    Roster(Vec<ChildInfo>),
}

/// Starts its children, in order, when it starts.
struct Supervisor {
    specs: Vec<Spec>,
    starts: UnboundedSender<Start>,
    events: Arc<Mutex<Vec<ChildEvent>>>,
}

impl Actor for Supervisor {
    type Message = Cmd;
    type Response = Reply;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for spec in self.specs.drain(..) {
            let built = Arc::new(AtomicUsize::new(0));
            let starts = self.starts.clone();
            let tag = spec.tag;
            let on_restart = spec.on_restart.clone();
            let mut builder = ctx
                .spawn_child(move || {
                    if built.fetch_add(1, Ordering::SeqCst) > 0 {
                        if let Some(hook) = &on_restart {
                            hook();
                        }
                    }
                    Worker {
                        tag,
                        starts: starts.clone(),
                    }
                })
                .named(spec.name.clone())
                .restart_type(RestartType::Permanent)
                .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
                .kill_grace(Duration::from_millis(300));
            if let Some(runtime) = &spec.runtime {
                builder = builder.on_runtime(runtime);
            }
            if let Some(timeout) = spec.start_timeout {
                builder = builder.start_timeout(timeout);
            }
            builder.await?;
        }
        Ok(())
    }

    async fn handle(&mut self, cmd: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        Ok(match cmd {
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
        _: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

async fn spawn_supervisor(
    config: SupervisionConfig,
    specs: Vec<Spec>,
    events: &Arc<Mutex<Vec<ChildEvent>>>,
) -> (ActorHandle<Supervisor>, UnboundedReceiver<Start>) {
    let (starts_tx, starts_rx) = unbounded_channel();
    let sup = Supervisor {
        specs,
        starts: starts_tx,
        events: events.clone(),
    }
    .spawn()
    .named(unique("supervisor"))
    .with_supervision(config)
    .await
    .unwrap();
    (sup, starts_rx)
}

async fn supervision_via(sup: &ActorHandle<Supervisor>, cmd: Cmd) -> Result<(), SupervisionError> {
    match tokio::time::timeout(HANG_BOUND, sup.send(cmd))
        .await
        .expect("the supervisor answers")
        .unwrap()
    {
        Reply::Supervision(result) => result,
        Reply::Roster(_) => unreachable!(),
    }
}

async fn roster_entry(sup: &ActorHandle<Supervisor>, name: &str) -> ChildInfo {
    let Reply::Roster(children) = sup.send(Cmd::Children).await.unwrap() else {
        unreachable!()
    };
    children
        .into_iter()
        .find(|child| child.name.as_deref() == Some(name))
        .expect("the child is on the roster")
}

/// Waits until the child is running its current incarnation and no restart
/// is pending for it.
async fn until_running(sup: &ActorHandle<Supervisor>, name: &str) {
    let end = Instant::now() + HANG_BOUND;
    loop {
        let child = roster_entry(sup, name).await;
        if child.is_alive && !child.restart_pending {
            return;
        }
        assert!(
            Instant::now() < end,
            "timed out waiting for {name}: {child:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn next_start(starts: &mut UnboundedReceiver<Start>) -> Start {
    tokio::time::timeout(HANG_BOUND, starts.recv())
        .await
        .expect("an incarnation starts")
        .expect("the supervisor holds the channel")
}

async fn crash(name: &str) {
    let _ = ActorSystem::default()
        .get::<Worker>(name)
        .expect("the worker is registered")
        .send(WorkerMsg::Crash)
        .await;
}

// ---------------------------------------------------------------------------
// One-for-one: the restart waits for the runtime
// ---------------------------------------------------------------------------

/// A restart onto a runtime that stopped being driven stays pending: both
/// manual stops answer `ChildRestarting` while it waits, and it lands on the
/// placed runtime as soon as the runtime is driven again. The crash that
/// began it is the only charge: a budget of two restarts still holds one
/// more, and the one after that exhausts it.
#[test]
fn a_pending_restart_onto_an_undriven_runtime_lands_when_the_runtime_is_driven_again() {
    let placed = Placed::driven();
    let placed_id = placed.id();
    let first_driver = placed.current_driver();
    let restarting = Arc::new(Notify::new());
    let mut far = Spec::new("far");
    far.runtime = Some(placed.handle.clone());
    // The factory of the restart stops the thread that drives the runtime and
    // waits for it, so the new incarnation is spawned onto an idle runtime.
    far.on_restart = Some({
        let restarting = restarting.clone();
        Arc::new(move || {
            first_driver.stop();
            restarting.notify_one();
        })
    });
    let far_name = far.name.clone();
    let events: Arc<Mutex<Vec<ChildEvent>>> = Default::default();
    let placed = Arc::new(placed);
    let driving = placed.clone();
    common::run(2, async move {
        let supervisor_runtime = Handle::current().id();
        let (sup, mut starts) = spawn_supervisor(
            SupervisionConfig::one_for_one().max_restarts(2, Duration::from_secs(60)),
            vec![far],
            &events,
        )
        .await;
        assert_eq!(next_start(&mut starts).await, ("far", placed_id));
        until_running(&sup, &far_name).await;

        crash(&far_name).await;
        tokio::time::timeout(HANG_BOUND, restarting.notified())
            .await
            .expect("the restart builds its incarnation");

        // The incarnation is spawned onto a runtime nobody drives: it has not
        // started, the child is restart-pending, and neither manual stop
        // takes it.
        let child = roster_entry(&sup, &far_name).await;
        assert!(!child.is_alive && child.restart_pending, "{child:?}");
        assert!(starts.try_recv().is_err(), "no second incarnation yet");
        let answer = supervision_via(&sup, Cmd::StopChild(far_name.clone())).await;
        assert!(
            matches!(answer, Err(SupervisionError::ChildRestarting(_))),
            "{answer:?}"
        );
        let answer = supervision_via(&sup, Cmd::TerminateChild(far_name.clone())).await;
        assert!(
            matches!(answer, Err(SupervisionError::ChildRestarting(_))),
            "{answer:?}"
        );
        assert!(starts.try_recv().is_err(), "still no second incarnation");

        // Driving the runtime again lets the restart land, on that runtime.
        driving.drive_again();
        let (tag, runtime) = next_start(&mut starts).await;
        assert_eq!((tag, runtime), ("far", placed_id));
        assert_ne!(runtime, supervisor_runtime);
        until_running(&sup, &far_name).await;
        // The waiting restart was charged once: one more crash is restarted,
        // and the crash after it exhausts the budget of two.
        crash(&far_name).await;
        assert_eq!(next_start(&mut starts).await, ("far", placed_id));
        until_running(&sup, &far_name).await;
        crash(&far_name).await;
        let reason = tokio::time::timeout(HANG_BOUND, sup.wait_stopped())
            .await
            .expect("the supervisor stops when its budget is spent");
        assert_eq!(reason, StopReason::ParentRequest);
        let last = events.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last.action, SupervisionAction::BudgetExhausted);
    });
    drop(placed);
}

// ---------------------------------------------------------------------------
// Rest-for-one: the group restart cannot finish
// ---------------------------------------------------------------------------

/// Under `RestForOne`, a member started after the crashed one and placed on a
/// runtime that nothing drives any more cannot start again: with a start
/// timeout on it, the supervisor stops with the unresponsive verdict for that
/// member once the timeout, the kill grace and the settle have passed.
#[test]
fn a_rest_for_one_restart_onto_an_undriven_runtime_stops_the_supervisor_with_the_unresponsive_verdict(
) {
    let placed = Placed::driven();
    let placed_id = placed.id();
    let driver = placed.current_driver();
    let near = Spec::new("near");
    let mut far = Spec::new("far");
    far.runtime = Some(placed.handle.clone());
    far.start_timeout = Some(Duration::from_millis(300));
    // The runtime stops being driven when the second incarnation is built.
    far.on_restart = Some(Arc::new(move || driver.stop()));
    let near_name = near.name.clone();
    let far_name = far.name.clone();
    common::run(2, async move {
        let (sup, mut starts) = spawn_supervisor(
            SupervisionConfig::rest_for_one().max_restarts(5, Duration::from_secs(60)),
            vec![near, far],
            &Default::default(),
        )
        .await;
        let mut first = [next_start(&mut starts).await, next_start(&mut starts).await];
        first.sort_by_key(|(tag, _)| *tag);
        assert_eq!(first[0].0, "far");
        assert_eq!(first[0].1, placed_id);
        assert_eq!(first[1].0, "near");
        until_running(&sup, &near_name).await;
        until_running(&sup, &far_name).await;

        crash(&near_name).await;
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
