#![allow(dead_code)]
//! Behavioral suite for supervision-tree teardown under `StopReason::Kill`.
//!
//! A task dying with `Kill` - whether configured directly or escalated from
//! a vetoed `Shutdown::Timeout` - never awaits its own children: it drops
//! its supervision state directly, and each live child's link guard fires
//! from its own drop glue. A child that is a worker is raised `Kill`: a
//! cooperative one dies at its own next turn boundary, and only one that
//! never yields is aborted, and only after its grace window elapses. A child
//! that is itself a supervisor is raised `ParentKilled` instead: it runs
//! `on_stopped` and stops its own children in order, bounded by its own
//! `Shutdown`, and its own children follow the same split one level down.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::{sleep, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorHandle, ActorResult, ActorStatus, ActorSystem, ChildEvent, RestartType,
    Shutdown, StopReason,
};

mod common;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type EventLog = Arc<Mutex<Vec<ChildEvent>>>;

fn recorder() -> EventLog {
    Arc::new(Mutex::new(Vec::new()))
}

/// Unique registry names so tests can run in parallel within one process.
static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("cascade-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

/// Polls a synchronous predicate until it holds or the deadline passes.
async fn wait_until<F>(timeout_ms: u64, mut pred: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// The tree-node actor
// ---------------------------------------------------------------------------

/// A generic supervision-tree node. Spawns one named child on command
/// (optionally itself a supervisor), can be wedged on a never-ready future,
/// can crash on command, and can be made to veto every stoppable stop
/// forever - everything the suite below needs to build and probe trees of
/// arbitrary shape.
struct Node {
    veto: bool,
    events: EventLog,
    /// Where every node of a tree records the reason its `on_stopped`
    /// received, when a test asks for it.
    stops: Option<StopLog>,
    /// The kill grace of every child this node spawns, and of theirs, when a
    /// test sets one; the default grace otherwise.
    child_grace: Option<Duration>,
}

type StopLog = Arc<std::sync::Mutex<Vec<StopReason>>>;

impl Node {
    fn new(veto: bool, events: EventLog) -> Self {
        Self {
            veto,
            events,
            stops: None,
            child_grace: None,
        }
    }

    fn recording_stops(mut self, stops: &StopLog) -> Self {
        self.stops = Some(stops.clone());
        self
    }
}

#[derive(Clone)]
enum NodeMsg {
    SpawnChild {
        name: String,
        as_supervisor: bool,
        shutdown: Shutdown,
        restart_type: RestartType,
        veto: bool,
    },
    /// Sets the flag once the handler runs, then parks for good.
    Wedge(Arc<std::sync::atomic::AtomicBool>),
    Crash,
    Ping,
}

impl Actor for Node {
    type Message = NodeMsg;
    type Response = ();

    async fn handle(&mut self, msg: NodeMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            NodeMsg::SpawnChild {
                name,
                as_supervisor,
                shutdown,
                restart_type,
                veto,
            } => {
                let events = self.events.clone();
                let stops = self.stops.clone();
                let child_grace = self.child_grace;
                let mut config = ActorConfig::default();
                if as_supervisor {
                    config = config.supervisor();
                }
                let mut builder = ctx
                    .spawn_child(move || {
                        let mut node = Node::new(veto, events.clone());
                        node.child_grace = child_grace;
                        match &stops {
                            Some(stops) => node.recording_stops(stops),
                            None => node,
                        }
                    })
                    .named(name)
                    .shutdown(shutdown)
                    .restart_type(restart_type)
                    .with_config(config);
                if let Some(grace) = child_grace {
                    builder = builder.kill_grace(grace);
                }
                builder.await?;
                Ok(())
            }
            NodeMsg::Wedge(wedged) => {
                // Parks the handler on a never-ready future: this actor
                // never returns to its message loop, so even a Kill signal
                // can only be observed by aborting the task outright.
                wedged.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                unreachable!("pending() never resolves")
            }
            NodeMsg::Crash => panic!("node crashed on command"),
            NodeMsg::Ping => Ok(()),
        }
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        !self.veto
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if let Some(stops) = &self.stops {
            stops.lock().unwrap().push(reason.clone());
        }
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().await.push(event.clone());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

/// A wedged (never-cooperating) child's `status()` must never report
/// `Stopped` while it is still alive, from the instant its supervisor is
/// killed through to the reaper actually aborting it: `status() == Stopped`
/// implies `!is_alive()` on every exit path, aborted or not, not just
/// eventually. Once the child is truly gone, its name is free for an
/// immediate same-name respawn.
#[test]
fn status_stopped_implies_not_alive_through_a_cascade() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let name = uname("w");

        let sup = Node::new(false, recorder())
            .spawn()
            .named(uname("sup"))
            .on_system(&sys)
            .supervisor()
            .await
            .unwrap();

        sup.send(NodeMsg::SpawnChild {
            name: name.clone(),
            as_supervisor: false,
            shutdown: Shutdown::Timeout(Duration::from_secs(5)),
            restart_type: RestartType::Permanent,
            veto: false,
        })
        .await
        .unwrap();

        let child = sys.get::<Node>(&name).expect("child registered");
        // The handler runs only after the child's start, and the Kill comes
        // only once the wedge holds.
        let wedged = Arc::new(std::sync::atomic::AtomicBool::new(false));
        child.notify(NodeMsg::Wedge(wedged.clone())).await.unwrap();
        assert!(
            wait_until(2_000, || wedged.load(Ordering::SeqCst)).await,
            "the child's handler parks"
        );

        sup.stop(StopReason::Kill).await.unwrap();

        // Poll continuously from the instant Kill is raised through to the
        // reaper's abort actually finishing: at no point may `status()`
        // report `Stopped` while `is_alive()` is still true.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.status() == ActorStatus::Stopped {
                assert!(
                    !child.is_alive(),
                    "status() must never report Stopped for a child that is still alive"
                );
            }
            if !child.is_alive() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the wedged child was never aborted by the reaper within 5s"
            );
            tokio::task::yield_now().await;
        }

        child.wait_stopped().await;
        assert!(!child.is_alive());
        assert!(
            sys.get::<Node>(&name).is_none(),
            "the name must be released once the child is truly gone"
        );

        let respawned = Node::new(false, recorder())
            .spawn()
            .named(name.clone())
            .on_system(&sys)
            .await;
        if let Err(err) = respawned {
            panic!("a same-name respawn under a freed name must succeed: {err}");
        }
    });
}

/// The inherited stop of a supervisor child whose `Shutdown::Timeout` is at
/// the very edge of what `Instant::checked_add` accepts, and beyond it: the
/// deadlines the guard computes in its drop glue saturate instead of
/// panicking, the child runs `on_stopped` and stops its own child in order,
/// and it ends when that child has.
#[test]
fn inherited_stop_with_an_edge_of_range_shutdown_does_not_panic() {
    common::run(0, inherited_stop_with_an_edge_of_range_shutdown());
}

async fn inherited_stop_with_an_edge_of_range_shutdown() {
    let base = Instant::now();
    let mut lo = Duration::ZERO;
    let mut hi = Duration::from_secs(u64::MAX / 2);
    while hi - lo > Duration::from_nanos(1) {
        let mid = lo + (hi - lo) / 2;
        if base.checked_add(mid).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let thirty_years = Duration::from_secs(30 * 365 * 86_400);
    // A panic in the guard's drop glue is caught by the runtime and would not
    // fail the stop by itself, so the test listens for one.
    static OVERFLOWS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info.to_string().contains("overflow") {
            OVERFLOWS.lock().unwrap().push(info.to_string());
        }
        previous(info);
    }));
    for d in [thirty_years, lo - Duration::from_micros(500), Duration::MAX] {
        let stops: StopLog = Arc::default();
        let sys = ActorSystem::create(uname("inherited-edge")).unwrap();
        let top = Node::new(false, recorder())
            .recording_stops(&stops)
            .spawn()
            .named(uname("top"))
            .on_system(&sys)
            .supervisor()
            .await
            .unwrap();
        let (mid_name, leaf_name) = (uname("mid"), uname("leaf"));
        top.send(NodeMsg::SpawnChild {
            name: mid_name.clone(),
            as_supervisor: true,
            shutdown: Shutdown::Timeout(d),
            restart_type: RestartType::Permanent,
            veto: false,
        })
        .await
        .unwrap();
        let mid = sys.get::<Node>(&mid_name).expect("mid registered");
        mid.send(NodeMsg::SpawnChild {
            name: leaf_name.clone(),
            as_supervisor: false,
            shutdown: Shutdown::Timeout(Duration::from_secs(5)),
            restart_type: RestartType::Permanent,
            veto: false,
        })
        .await
        .unwrap();

        top.stop(StopReason::Kill).await.unwrap();
        top.wait_stopped().await;
        tokio::time::timeout(Duration::from_secs(5), mid.wait_stopped())
            .await
            .unwrap_or_else(|_| panic!("the middle supervisor stops (window {d:?})"));
        assert!(
            wait_until(2_000, || sys.get::<Node>(&leaf_name).is_none()).await,
            "the leaf ends with it (window {d:?})"
        );
        let stops = stops.lock().unwrap().clone();
        assert!(
            matches!(
                stops.as_slice(),
                [StopReason::ParentKilled, StopReason::ParentRequest]
            ),
            "the middle supervisor ran `on_stopped` and stopped its leaf in order, \
             and the killed root ran none (window {d:?}): {stops:?}"
        );
    }
    let _ = std::panic::take_hook();
    let overflows = OVERFLOWS.lock().unwrap().clone();
    assert!(
        overflows.is_empty(),
        "no deadline overflowed in a guard's drop glue: {overflows:?}"
    );
}

// ---------------------------------------------------------------------------
// The reaper follows the runtime that is current when a guard needs it
// ---------------------------------------------------------------------------

/// A worker whose single turn holds until `release` is set (for at most ten
/// seconds) before flipping `finished` - the in-flight-turn probe for
/// `kill_cascade_grace_survives_a_runtime_change`.
struct TurnWorker {
    began: Arc<std::sync::atomic::AtomicBool>,
    release: Arc<std::sync::atomic::AtomicBool>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl Actor for TurnWorker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.began.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.release.load(Ordering::SeqCst) && Instant::now() < deadline {
            sleep(Duration::from_millis(1)).await;
        }
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// The kill grace of the runtime-change test's worker: far longer than the
/// test holds its turn after the cascade's Kill.
const TURN_WORKER_GRACE: Duration = Duration::from_secs(2);

/// A supervisor that spawns one named `TurnWorker` child on `on_started`,
/// with `TURN_WORKER_GRACE`.
struct TurnSupervisor {
    child_name: String,
    began: Arc<std::sync::atomic::AtomicBool>,
    release: Arc<std::sync::atomic::AtomicBool>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl Actor for TurnSupervisor {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (began, release, finished) = (
            self.began.clone(),
            self.release.clone(),
            self.finished.clone(),
        );
        ctx.spawn_child(move || TurnWorker {
            began: began.clone(),
            release: release.clone(),
            finished: finished.clone(),
        })
        .named(self.child_name.clone())
        .kill_grace(TURN_WORKER_GRACE)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A runtime change under one system: the same Kill cascade run on runtime A,
/// then again on runtime B after A is dropped, on the same system. A system's
/// reaper is spawned lazily on whichever runtime first needs it; once that
/// runtime is gone, a link guard's drop must still find (or respawn) a live
/// reaper on whatever runtime is current, honoring the child's full grace
/// instead of aborting it immediately.
///
/// The worker is inside a turn when its supervisor is killed, and the turn
/// is released only after the supervisor has stopped and a further 50 ms,
/// well inside the worker's 2 s grace: a healthy reaper leaves it its grace
/// to finish that turn cooperatively, while a reaper that aborted at once
/// would have ended it first.
#[test]
fn kill_cascade_grace_survives_a_runtime_change() {
    let sys = ActorSystem::create(uname("kill-grace-runtime-change")).unwrap();

    let run_phase = |sys: &Arc<ActorSystem>, rt: &tokio::runtime::Runtime, tag: &str| -> bool {
        rt.block_on(async {
            let began = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let child_name = uname(&format!("{tag}-worker"));

            let sup = TurnSupervisor {
                child_name: child_name.clone(),
                began: began.clone(),
                release: release.clone(),
                finished: finished.clone(),
            }
            .spawn()
            .named(uname(&format!("{tag}-sup")))
            .on_system(sys)
            .supervisor()
            .await
            .unwrap();

            assert!(
                wait_until(2_000, || sys.get::<TurnWorker>(&child_name).is_some()).await,
                "{tag}: worker must be registered before it can be probed"
            );
            let worker: ActorHandle<TurnWorker> = sys.get(&child_name).unwrap();

            worker.notify(()).await.unwrap();
            // The turn runs only after the worker's start: the Kill comes
            // once the worker is inside it.
            assert!(
                wait_until(2_000, || began.load(Ordering::SeqCst)).await,
                "{tag}: the worker's turn begins"
            );

            sup.stop(StopReason::Kill).await.unwrap();
            sup.wait_stopped().await;
            sleep(Duration::from_millis(50)).await;
            assert!(
                worker.is_alive() && !finished.load(Ordering::SeqCst),
                "{tag}: the worker is still inside its turn, within its grace"
            );
            release.store(true, Ordering::SeqCst);

            assert!(
                wait_until(2_000, || sys.get::<TurnWorker>(&child_name).is_none()).await,
                "{tag}: the killed worker must eventually be reaped"
            );

            finished.load(Ordering::SeqCst)
        })
    };

    let rt_a = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let a_finished = run_phase(&sys, &rt_a, "rt-a");
    drop(rt_a);
    assert!(
        a_finished,
        "on the first runtime, with its own reaper alive, the worker's in-flight \
         turn must finish within its grace before it dies"
    );

    let rt_b = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let b_finished = run_phase(&sys, &rt_b, "rt-b");
    drop(rt_b);
    assert!(
        b_finished,
        "after runtime A drops, the same system's reaper must still honor the \
         child's grace on a fresh runtime instead of aborting it immediately"
    );
}
