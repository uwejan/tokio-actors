//! Behavioral suite for supervision-tree teardown under `StopReason::Kill`.
//!
//! A task dying with `Kill` - whether configured directly or escalated from
//! a vetoed `Shutdown::Timeout` - never awaits its own children: it drops
//! its supervision state directly, and each live child's link guard fires
//! from its own drop glue, raising Kill on that child's lane and repeating
//! the same inversion one level down. A cooperative descendant dies at its
//! own next turn boundary; only a child that never yields is aborted, and
//! only after its grace window elapses.

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

/// Re-looks a named `Node` up until it answers a `Ping` - proof that
/// whichever incarnation is registered under the name right now is alive and
/// responsive (a crashed predecessor can never satisfy this).
async fn wait_responsive(sys: &ActorSystem, name: &str, timeout_ms: u64) -> ActorHandle<Node> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Some(handle) = sys.get::<Node>(name) {
            if handle.send(NodeMsg::Ping).await.is_ok() {
                return handle;
            }
        }
        assert!(Instant::now() < deadline, "{name} never became responsive");
        sleep(Duration::from_millis(10)).await;
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
}

impl Node {
    fn new(veto: bool, events: EventLog) -> Self {
        Self { veto, events }
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
    Wedge,
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
                let mut config = ActorConfig::default();
                if as_supervisor {
                    config = config.supervisor();
                }
                ctx.spawn_child(move || Node::new(veto, events.clone()))
                    .named(name)
                    .shutdown(shutdown)
                    .restart_type(restart_type)
                    .with_config(config)
                    .await?;
                Ok(())
            }
            NodeMsg::Wedge => {
                // Parks the handler on a never-ready future: this actor
                // never returns to its message loop, so even a Kill signal
                // can only be observed by aborting the task outright.
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

#[tokio::test(flavor = "multi_thread")]
async fn cascaded_subtree_has_no_observably_running_descendant_after_kill() {
    let sys = ActorSystem::create(uname("sys")).unwrap();
    let mid_name = uname("mid");
    let leaf_name = uname("leaf");

    let top = Node::new(false, recorder())
        .spawn()
        .named(uname("top"))
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();

    top.send(NodeMsg::SpawnChild {
        name: mid_name.clone(),
        as_supervisor: true,
        shutdown: Shutdown::Timeout(Duration::from_secs(5)),
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
    let leaf = sys.get::<Node>(&leaf_name).expect("leaf registered");

    let mid_id = mid.id().clone();
    let leaf_id = leaf.id().clone();

    top.stop(StopReason::Kill).await.unwrap();
    top.wait_stopped().await;

    assert!(
        wait_until(2_000, || {
            !matches!(sys.actor_status(&mid_id), Some(ActorStatus::Running))
        })
        .await,
        "mid must not be observably Running after the cascade"
    );
    assert!(
        wait_until(2_000, || {
            !matches!(sys.actor_status(&leaf_id), Some(ActorStatus::Running))
        })
        .await,
        "leaf must not be observably Running after the cascade"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_cooperative_subtree_tears_down_without_reaper_rescue() {
    let sys = ActorSystem::create(uname("sys")).unwrap();
    let mid_name = uname("mid");
    let leaf_name = uname("leaf");

    let top = Node::new(false, recorder())
        .spawn()
        .named(uname("top"))
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();

    top.send(NodeMsg::SpawnChild {
        name: mid_name.clone(),
        as_supervisor: true,
        shutdown: Shutdown::Timeout(Duration::from_secs(5)),
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

    // Both cooperative descendants (mid is itself a supervisor, so its own
    // grace is the larger of the two constants) must be FULLY torn down -
    // name released, task exited - well under even the smaller of the two
    // grace windows. Reaching that bound would mean the reaper, not
    // cooperative observation of Kill, is what actually reaped them.
    let all_gone = wait_until(50, || {
        sys.get::<Node>(&mid_name).is_none() && sys.get::<Node>(&leaf_name).is_none()
    })
    .await;
    assert!(
        all_gone,
        "a cooperative nested subtree must tear down well within either grace \
         window, not by reaper rescue"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wedged_child_is_aborted_only_after_its_grace() {
    let sys = ActorSystem::create(uname("sys")).unwrap();
    let leaf_name = uname("leaf");

    let sup = Node::new(false, recorder())
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();

    sup.send(NodeMsg::SpawnChild {
        name: leaf_name.clone(),
        as_supervisor: false,
        shutdown: Shutdown::Timeout(Duration::from_secs(5)),
        restart_type: RestartType::Permanent,
        veto: false,
    })
    .await
    .unwrap();

    let leaf = sys.get::<Node>(&leaf_name).expect("leaf registered");
    leaf.notify(NodeMsg::Wedge).await.unwrap();
    // Let the wedge actually take hold before signalling anything.
    sleep(Duration::from_millis(30)).await;

    let t0 = Instant::now();
    sup.stop(StopReason::Kill).await.unwrap();
    sup.wait_stopped().await;

    assert!(
        wait_until(2_000, || sys.get::<Node>(&leaf_name).is_none()).await,
        "a wedged leaf must eventually be aborted by the reaper"
    );
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(90),
        "the reaper must honor the leaf's grace window before aborting, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the reaper must not take drastically longer than the grace window, took {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn escalated_timeout_kill_cascades_the_same_as_configured_kill() {
    let sys = ActorSystem::create(uname("sys")).unwrap();
    let mid_name = uname("mid");
    let leaf_name = uname("leaf");

    let top = Node::new(false, recorder())
        .spawn()
        .named(uname("top"))
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();

    // `mid` vetoes every stoppable stop forever and has a short Shutdown
    // timeout, so `top`'s own child-stop ladder escalates it to Kill
    // instead of waiting for a cooperative stop that never comes.
    top.send(NodeMsg::SpawnChild {
        name: mid_name.clone(),
        as_supervisor: true,
        shutdown: Shutdown::Timeout(Duration::from_millis(50)),
        restart_type: RestartType::Permanent,
        veto: true,
    })
    .await
    .unwrap();
    let mid = sys.get::<Node>(&mid_name).expect("mid registered");

    // `leaf` ALSO vetoes forever and carries a Shutdown policy long enough
    // to be unmistakable: if `mid`'s own teardown ever awaited it under
    // that policy instead of cascading Kill directly, this test would see
    // it take seconds, not milliseconds.
    mid.send(NodeMsg::SpawnChild {
        name: leaf_name.clone(),
        as_supervisor: false,
        shutdown: Shutdown::Timeout(Duration::from_secs(2)),
        restart_type: RestartType::Permanent,
        veto: true,
    })
    .await
    .unwrap();

    // A graceful top-level stop, NOT Kill: this is what drives `top`'s own
    // child-stop ladder, which is where the Timeout -> Kill escalation on
    // `mid` actually happens.
    top.stop(StopReason::Graceful).await.unwrap();
    top.wait_stopped().await;

    assert!(
        wait_until(500, || sys.get::<Node>(&leaf_name).is_none()).await,
        "leaf must be torn down promptly by the inverted cascade under mid's \
         escalated Kill, not left waiting out its own unrelated Shutdown policy"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restarted_incarnation_still_cascades_on_a_later_parent_kill() {
    let sys = ActorSystem::create(uname("sys")).unwrap();
    let worker_name = uname("worker");

    let sup = Node::new(false, recorder())
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();

    sup.send(NodeMsg::SpawnChild {
        name: worker_name.clone(),
        as_supervisor: false,
        shutdown: Shutdown::Timeout(Duration::from_secs(5)),
        restart_type: RestartType::Permanent,
        veto: false,
    })
    .await
    .unwrap();

    let worker = wait_responsive(&sys, &worker_name, 5_000).await;
    worker.notify(NodeMsg::Crash).await.unwrap();

    // A FRESH incarnation re-registers under the same name; only it can
    // answer a Ping (its crashed predecessor cannot).
    let restarted = wait_responsive(&sys, &worker_name, 5_000).await;
    restarted.notify(NodeMsg::Wedge).await.unwrap();
    sleep(Duration::from_millis(30)).await;

    sup.stop(StopReason::Kill).await.unwrap();
    sup.wait_stopped().await;

    // The fresh incarnation's own link guard - minted during the restart,
    // not inherited from the original spawn - must still be the one that
    // eventually reaps it.
    assert!(
        wait_until(2_000, || sys.get::<Node>(&worker_name).is_none()).await,
        "the restarted incarnation must still be cascaded and reaped after its \
         supervisor dies"
    );
}
