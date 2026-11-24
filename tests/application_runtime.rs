//! Applications at runtime: hooks that panic before their future exists, tops
//! that end before the start returns or are taken twice, a stop that waits for
//! the top's subtree, an own system released only when every actor of it let
//! go, and a controller whose end leaves no application without an owner.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_actors::application::{
    AppError, AppRoot, AppState, AppSystem, Application, Applications, NodeCause, StartError,
};
use tokio_actors::{
    Actor, ActorContext, ActorError, ActorExt, ActorHandle, ActorResult, ActorSystem, RestartType,
    ShutdownPolicy, StopOutcome, StopReason, SubtreeEnd, SystemConfig,
};

/// The tests share the default system, where every controller runs.
static SERIAL: Mutex<()> = Mutex::const_new(());

fn ready_now<F: Future>(fut: F) -> Option<F::Output> {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match std::pin::pin!(fut).poll(&mut cx) {
        std::task::Poll::Ready(output) => Some(output),
        std::task::Poll::Pending => None,
    }
}

async fn within<F: Future>(what: &str, fut: F) -> F::Output {
    // A hang detector, never a wait for an event.
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .unwrap_or_else(|_| panic!("hung: {what}"))
}

/// Waits until `name` reads `Loaded`, the documented way to wait for a held
/// application's release.
async fn released(apps: &Applications, name: &str) {
    let held = |rows: Vec<tokio_actors::application::AppInfo>| {
        rows.iter()
            .any(|r| r.name == name && r.state != AppState::Loaded)
    };
    within("the release", async {
        while held(apps.which_applications().await.unwrap()) {}
    })
    .await;
}

enum Quit {
    Graceful,
    Fail,
    /// Blocks the actor's thread until the channel gives, after `entered`.
    Block(std_mpsc::Receiver<()>, oneshot::Sender<()>),
}

/// A top, or a worker beside or below one.
#[derive(Default)]
struct Top {
    /// Spawns one child in `on_started` and sends it here.
    child: Option<mpsc::UnboundedSender<ActorHandle<Top>>>,
    /// Signals, then blocks its value's drop until the channel gives.
    drop_gate: Option<(oneshot::Sender<()>, std_mpsc::Receiver<()>)>,
}

impl Drop for Top {
    fn drop(&mut self) {
        if let Some((entered, open)) = self.drop_gate.take() {
            let _ = entered.send(());
            let _ = tokio::task::block_in_place(|| open.recv());
        }
    }
}

impl Actor for Top {
    type Message = Quit;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(tx) = self.child.take() {
            let _ = tx.send(ctx.spawn_child(Top::default).await?);
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Quit, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Quit::Graceful => drop(ctx.stop(StopReason::Graceful)),
            Quit::Fail => return Err(ActorError::User("top failed".into())),
            Quit::Block(open, entered) => {
                let _ = entered.send(());
                let _ = tokio::task::block_in_place(|| open.recv());
            }
        }
        Ok(())
    }
}

type Slot<T> = Arc<StdMutex<Option<T>>>;

#[derive(Default)]
struct Probe {
    name: &'static str,
    shared: Option<&'static str>,
    start_panics: bool,
    prep_panics: bool,
    /// The top ends itself with this before `AppRoot::of`.
    ends: Option<fn() -> Quit>,
    /// The root to return instead of a new top.
    given: Option<ActorHandle<Top>>,
    /// A second root of the system, spawned beside the top.
    beside: Slot<Top>,
    tops: Option<mpsc::UnboundedSender<ActorHandle<Top>>>,
    /// Whether the top's subtree had ended when `stop` ran.
    stops: Option<mpsc::UnboundedSender<bool>>,
    gate: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    last: Slot<ActorHandle<Top>>,
    /// While set, `start` fails once the top is spawned, and clears it.
    fails: Option<Arc<AtomicBool>>,
    /// `start` waits here before it spawns the top.
    start_gate: Option<oneshot::Receiver<()>>,
    /// `stop` calls this controller and reports what came of it.
    calls: Option<(Applications, mpsc::UnboundedSender<String>)>,
}

impl Application for Probe {
    fn name(&self) -> &str {
        self.name
    }

    fn system(&self) -> AppSystem {
        self.shared
            .map_or(AppSystem::Own, |s| AppSystem::Shared(s.into()))
    }

    fn start(
        &mut self,
        system: &Arc<ActorSystem>,
    ) -> impl Future<Output = Result<AppRoot, StartError>> + Send {
        let panics = std::mem::take(&mut self.start_panics);
        assert!(!panics, "start panics before its future exists");
        let (system, ends, tops) = (system.clone(), self.ends, self.tops.clone());
        let (given, beside, last) = (self.given.clone(), self.beside.clone(), self.last.clone());
        let (fails, start_gate) = (self.fails.clone(), self.start_gate.take());
        async move {
            if let Some(gate) = start_gate {
                let _ = gate.await;
            }
            if let Some(given) = given {
                return Ok(AppRoot::of(&given));
            }
            let top = Top::default().spawn().on_system(&system).await?;
            let worker = beside.lock().unwrap().take();
            if let Some(worker) = worker {
                let grace = Duration::from_millis(10);
                worker.spawn().on_system(&system).kill_grace(grace).await?;
            }
            *last.lock().unwrap() = Some(top.clone());
            if let Some(tops) = tops {
                let _ = tops.send(top.clone());
            }
            if fails.is_some_and(|fails| fails.swap(false, Ordering::SeqCst)) {
                return Err("start failed".into());
            }
            if let Some(end) = ends {
                top.notify(end()).await?;
                top.wait_stopped().await;
            }
            Ok(AppRoot::of(&top))
        }
    }

    fn prep_stop(&mut self) -> impl Future<Output = ()> + Send {
        assert!(
            !self.prep_panics,
            "prep_stop panics before its future exists"
        );
        let gate = self.gate.take();
        async move {
            if let Some((entered, open)) = gate {
                let _ = entered.send(());
                let _ = open.await;
            }
        }
    }

    fn stop(&mut self) -> impl Future<Output = ()> + Send {
        let (stops, last) = (self.stops.clone(), self.last.lock().unwrap().clone());
        let calls = self.calls.clone();
        async move {
            if let Some((apps, report)) = calls {
                // Neither form can end while this hook holds its stop: the
                // unbounded call is still pending, the bounded one times out.
                let pending = ready_now(apps.stop_all()).is_none();
                let bounded = apps.timeout(Duration::ZERO).stop_all().await;
                let _ = report.send(format!(
                    "{pending} {}",
                    matches!(bounded, Err(AppError::Timeout))
                ));
            }
            let ended = last.is_some_and(|top| {
                ready_now(top.observe_subtree().wait_ended()) == Some(SubtreeEnd::Ended)
            });
            if let Some(stops) = stops {
                let _ = stops.send(ended);
            }
        }
    }
}

const TR: RestartType = RestartType::Transient;
const TE: RestartType = RestartType::Temporary;

fn failed<T: std::fmt::Debug>(result: Result<T, AppError>) {
    assert!(
        matches!(result, Err(AppError::StartFailed { .. })),
        "{result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn application_hooks_and_top_end() {
    let _serial = SERIAL.lock().await;
    let apps = Applications::spawn().await.unwrap();
    let (stops_tx, mut stops) = mpsc::unbounded_channel();
    // `start` panics before its future exists, then starts.
    let probe = Probe {
        name: "panicking-start",
        start_panics: true,
        ..Probe::default()
    };
    apps.load(probe).await.unwrap();
    failed(apps.start("panicking-start", TE).await);
    let rows = apps.which_applications().await.unwrap();
    assert_eq!(rows[0].state, AppState::Loaded);
    apps.start("panicking-start", TE).await.unwrap();
    // `prep_stop` panics in its prelude: the stop goes on, `stop` sees the
    // subtree ended, and a restart is free at once.
    let probe = Probe {
        name: "panicking-prep",
        prep_panics: true,
        stops: Some(stops_tx.clone()),
        ..Probe::default()
    };
    apps.load(probe).await.unwrap();
    apps.start("panicking-prep", TE).await.unwrap();
    let stop = apps.stop("panicking-prep").await.unwrap();
    assert!(!stop.held, "{stop:?}");
    assert!(
        within("the stop hook", stops.recv()).await.unwrap(),
        "stop ran beside the subtree"
    );
    apps.start("panicking-prep", TE).await.unwrap();
    // The system of an application of its own is not shared: an application
    // that names it refuses to start, as an own application refuses a taken
    // name.
    let probe = Probe {
        name: "joiner",
        shared: Some("panicking-prep"),
        ..Probe::default()
    };
    apps.load(probe).await.unwrap();
    let joined = apps.start("joiner", TE).await;
    assert!(
        matches!(joined, Err(AppError::SystemTaken(_))),
        "{joined:?}"
    );
    // A top that ended before `AppRoot::of` gives a start that succeeds and
    // its own end.
    let (stops_tx, mut stops) = mpsc::unbounded_channel();
    for name in ["graceful-top", "failed-top"] {
        let end: fn() -> Quit = if name == "failed-top" {
            || Quit::Fail
        } else {
            || Quit::Graceful
        };
        let probe = Probe {
            name,
            shared: Some("hooks"),
            ends: Some(end),
            stops: Some(stops_tx.clone()),
            ..Probe::default()
        };
        apps.load(probe).await.unwrap();
        apps.start(name, TR).await.unwrap();
        within("the end hooks", stops.recv()).await.unwrap();
        apps.stop_all().await.unwrap();
        match ready_now(apps.node_shutdown()) {
            None => assert_eq!(name, "graceful-top"),
            Some(verdict) => {
                let cause = verdict.unwrap().cause;
                assert!(
                    matches!(cause, NodeCause::Ended(StopReason::Failure(_))),
                    "{cause:?}"
                );
            }
        }
    }
    within("the controller's shutdown", apps.shutdown())
        .await
        .unwrap();
    // A root spawned before the load is taken once: a second application and
    // a second run that return a taken root fail to start.
    let apps = Applications::spawn().await.unwrap();
    let system = ActorSystem::get_named("hooks").unwrap();
    let early = Top::default().spawn().on_system(&system).await.unwrap();
    for name in ["early", "twice"] {
        let probe = Probe {
            name,
            shared: Some("hooks"),
            given: Some(early.clone()),
            ..Probe::default()
        };
        apps.load(probe).await.unwrap();
    }
    apps.start("early", TE).await.unwrap();
    failed(apps.start("twice", TE).await);
    let (tops_tx, mut tops) = mpsc::unbounded_channel();
    let probe = Probe {
        name: "rerun",
        shared: Some("hooks"),
        tops: Some(tops_tx),
        ..Probe::default()
    };
    apps.load(probe).await.unwrap();
    apps.start("rerun", TE).await.unwrap();
    let run1 = tops.recv().await.unwrap();
    apps.stop("rerun").await.unwrap();
    apps.unload("rerun").await.unwrap();
    let probe = Probe {
        name: "rerun",
        shared: Some("hooks"),
        given: Some(run1),
        ..Probe::default()
    };
    apps.load(probe).await.unwrap();
    failed(apps.start("rerun", TE).await);
    within("the controller's shutdown", apps.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_stop_waits_for_the_subtree() {
    let _serial = SERIAL.lock().await;
    // A short per-root timeout: the top is killed, and its child, blocked in
    // a handler, cannot take its `Kill`.
    let policy = ShutdownPolicy {
        per_actor_timeout: Duration::from_millis(20),
        ..ShutdownPolicy::default()
    };
    let config = SystemConfig {
        shutdown_policy: policy,
        ..SystemConfig::default()
    };
    let system = ActorSystem::create_with("subtree", config).unwrap();
    let (child_tx, mut child) = mpsc::unbounded_channel();
    let top = Top {
        child: Some(child_tx),
        drop_gate: None,
    };
    let supervised = tokio_actors::SupervisionConfig::one_for_one();
    let top = top
        .spawn()
        .on_system(&system)
        .with_supervision(supervised)
        .await
        .unwrap();
    let apps = Applications::spawn().await.unwrap();
    let probe = Probe {
        name: "subtree",
        shared: Some("subtree"),
        given: Some(top),
        ..Probe::default()
    };
    apps.load(probe).await.unwrap();
    apps.start("subtree", TE).await.unwrap();
    let worker = child.recv().await.unwrap();
    let (open, gate) = std_mpsc::channel();
    let (entered_tx, entered) = oneshot::channel();
    worker.notify(Quit::Block(gate, entered_tx)).await.unwrap();
    entered.await.unwrap();
    let stop = within("the stop", apps.stop("subtree")).await.unwrap();
    assert!(stop.held, "{stop:?}");
    assert!(matches!(
        apps.start("subtree", TE).await,
        Err(AppError::Held(_))
    ));
    open.send(()).unwrap();
    within("the child's end", worker.observe_subtree().wait_ended()).await;
    released(&apps, "subtree").await;
    within("the controller's shutdown", apps.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_release_waits_for_every_incarnation() {
    let _serial = SERIAL.lock().await;
    let apps = Applications::spawn().await.unwrap();
    let (entered_tx, entered) = oneshot::channel();
    let (open, gate) = std_mpsc::channel();
    let worker = Top {
        child: None,
        drop_gate: Some((entered_tx, gate)),
    };
    let probe = Probe {
        name: "own-release",
        beside: Arc::new(StdMutex::new(Some(worker))),
        ..Probe::default()
    };
    apps.load(probe).await.unwrap();
    apps.start("own-release", TE).await.unwrap();
    // The walk stops W, whose value's drop blocks: W is reported
    // unresponsive and the application is held until W's subtree ended.
    let stop = within("the stop", apps.stop("own-release")).await.unwrap();
    entered.await.unwrap();
    let walk = stop.system.as_ref().unwrap();
    let unresponsive = walk
        .outcomes
        .iter()
        .any(|(_, r)| r.outcome == StopOutcome::Unresponsive);
    assert!(stop.held && unresponsive, "{stop:?}");
    assert!(matches!(
        apps.start("own-release", TE).await,
        Err(AppError::Held(_))
    ));
    open.send(()).unwrap();
    released(&apps, "own-release").await;
    apps.start("own-release", TE).await.unwrap();
    within("the controller's shutdown", apps.shutdown())
        .await
        .unwrap();
}

#[cfg(feature = "telemetry")]
/// The ids of the controllers the default system lists.
fn controllers() -> Vec<tokio_actors::ActorId> {
    let rows = ActorSystem::default().snapshot();
    let found = rows.iter().filter(|r| r.type_name.ends_with("Controller"));
    found.map(|r| r.actor.id().clone()).collect()
}

#[cfg(feature = "telemetry")]
/// Spawns a controller and names it: the one the default system lists after
/// the spawn and not before it. A controller of an earlier test may still be
/// listed while its runtime ends, so a type name alone does not name this one.
async fn spawn_named() -> (Applications, tokio_actors::ActorId) {
    let before = controllers();
    let apps = Applications::spawn().await.unwrap();
    let id = controllers().into_iter().find(|id| !before.contains(id));
    (apps, id.expect("a controller"))
}

#[cfg(feature = "telemetry")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn controller_end_hands_on_its_applications() {
    let _serial = SERIAL.lock().await;
    // A controller killed while a stop waits in `prep_stop`: the stop runs to
    // its end, hooks included, and the own system is walked.
    let (apps, controller) = spawn_named().await;
    let (stops_tx, mut stops) = mpsc::unbounded_channel();
    let (entered_tx, entered) = oneshot::channel();
    let (open, open_rx) = oneshot::channel();
    let gate = Some((entered_tx, open_rx));
    let probe = Probe {
        name: "killed-in-stop",
        stops: Some(stops_tx),
        gate,
        ..Probe::default()
    };
    apps.load(probe).await.unwrap();
    apps.start("killed-in-stop", TE).await.unwrap();
    let stopping = apps.clone();
    let stop = tokio::spawn(async move { stopping.stop("killed-in-stop").await });
    entered.await.unwrap();
    ActorSystem::default().kill_by_id(&controller).await;
    open.send(()).unwrap();
    within("the stop hook", stops.recv()).await.unwrap();
    let ended = within("the node verdict", apps.node_shutdown()).await;
    assert!(
        matches!(ended, Err(AppError::Ended(StopReason::Kill))),
        "{ended:?}"
    );
    assert!(matches!(stop.await.unwrap(), Err(AppError::Ended(_))));
    // A controller killed while idle: the top reads `Kill` and no hook runs.
    let (apps, controller) = spawn_named().await;
    let (stops_tx, mut stops) = mpsc::unbounded_channel();
    let (tops_tx, mut tops) = mpsc::unbounded_channel();
    let probe = Probe {
        name: "killed-idle",
        shared: Some("idle"),
        stops: Some(stops_tx),
        tops: Some(tops_tx),
        ..Probe::default()
    };
    apps.load(probe).await.unwrap();
    apps.start("killed-idle", TE).await.unwrap();
    let top = tops.recv().await.unwrap();
    ActorSystem::default().kill_by_id(&controller).await;
    assert_eq!(
        within("the top's end", top.wait_stopped()).await,
        StopReason::Kill
    );
    let ended = within("the node verdict", apps.node_shutdown()).await;
    assert!(
        matches!(ended, Err(AppError::Ended(StopReason::Kill))),
        "{ended:?}"
    );
    assert!(
        stops.try_recv().is_err(),
        "no hook of an application at rest runs"
    );
}

struct Far(
    mpsc::UnboundedSender<ActorHandle<Top>>,
    tokio::runtime::Handle,
);

impl Application for Far {
    fn name(&self) -> &str {
        "far-top"
    }

    async fn start(&mut self, system: &Arc<ActorSystem>) -> Result<AppRoot, StartError> {
        let system = system.clone();
        let spawn = async move { Top::default().spawn().on_system(&system).await };
        let top = self.1.spawn(spawn).await??;
        let _ = self.0.send(top.clone());
        Ok(AppRoot::of(&top))
    }
}

#[test]
fn controller_runtime_end_kills_its_tops_only() {
    // The controller's runtime ends with an own application whose top runs on
    // a second runtime: the top is killed, the own system is not shut down.
    let main = tokio::runtime::Runtime::new().unwrap();
    let _serial = main.block_on(SERIAL.lock());
    let ctl = tokio::runtime::Runtime::new().unwrap();
    let (tops_tx, mut tops) = mpsc::unbounded_channel();
    ctl.block_on(async {
        let apps = Applications::spawn().await.unwrap();
        apps.load(Far(tops_tx, main.handle().clone()))
            .await
            .unwrap();
        apps.start("far-top", TE).await.unwrap();
    });
    let top = tops.try_recv().unwrap();
    drop(ctl);
    main.block_on(async {
        assert_eq!(
            within("the top's end", top.wait_stopped()).await,
            StopReason::Kill
        );
        let own = ActorSystem::get_named("far-top").unwrap();
        let spawned = Top::default().spawn().on_system(&own).await;
        assert!(spawned.is_ok(), "the own system is still active");
    });
}

/// A failed start, a start that outlasts a bound, and a hook that calls its own
/// controller.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn application_claim_pins() {
    let _serial = SERIAL.lock().await;
    let apps = Applications::spawn().await.unwrap();

    // After a failed start the own system is shut down and the next start reactivates it;
    // a shared system is not shut down and what the failed start left running stays.
    for (name, shared) in [
        ("pins-own", None),
        ("pins-shared", Some("pins-shared-system")),
    ] {
        let (tops_tx, mut tops) = mpsc::unbounded_channel();
        let probe = Probe {
            name,
            shared,
            tops: Some(tops_tx),
            fails: Some(Arc::new(AtomicBool::new(true))),
            ..Probe::default()
        };
        apps.load(probe).await.unwrap();
        failed(apps.start(name, TE).await);
        let top = tops.recv().await.unwrap();
        if shared.is_none() {
            within("the own system's walk", top.wait_stopped()).await;
            apps.start(name, TE).await.unwrap();
            assert!(
                tops.recv().await.unwrap().is_alive(),
                "the next start reactivated the system"
            );
            apps.stop(name).await.unwrap();
        } else {
            assert!(top.is_alive(), "a shared system is left alone");
        }
        released(&apps, name).await;
    }

    // A start is bounded by no one but the caller: it outlasts a bound and still completes.
    let (tops_tx, mut tops) = mpsc::unbounded_channel();
    let (open, start_gate) = oneshot::channel();
    let probe = Probe {
        name: "pins-slow",
        shared: Some("pins-slow-system"),
        tops: Some(tops_tx),
        start_gate: Some(start_gate),
        ..Probe::default()
    };
    apps.load(probe).await.unwrap();
    let bounded = apps
        .clone()
        .timeout(Duration::ZERO)
        .start("pins-slow", TE)
        .await;
    assert!(matches!(bounded, Err(AppError::Timeout)), "{bounded:?}");
    open.send(()).unwrap();
    within("the start", tops.recv()).await.unwrap();
    within("the start to land", async {
        while apps
            .which_applications()
            .await
            .unwrap()
            .iter()
            .any(|r| r.name == "pins-slow" && r.state != AppState::Running)
        {}
    })
    .await;

    // A hook that calls its own controller does not end unless bounded.
    let (report, mut reports) = mpsc::unbounded_channel();
    let probe = Probe {
        name: "pins-hook",
        shared: Some("pins-slow-system"),
        calls: Some((apps.clone(), report)),
        ..Probe::default()
    };
    apps.load(probe).await.unwrap();
    apps.start("pins-hook", TE).await.unwrap();
    apps.stop("pins-hook").await.unwrap();
    assert_eq!(
        within("the hook's report", reports.recv()).await.unwrap(),
        "true true"
    );
    within("the controller's shutdown", apps.shutdown())
        .await
        .unwrap();
}
