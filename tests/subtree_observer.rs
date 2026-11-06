//! The subtree observer: `ActorSystem::observe_subtree`,
//! `ActorHandle::observe_subtree`, `ActorSystem::kill_by_id_with_observer` and
//! `SubtreeObserver::wait_ended`.
//!
//! `kill_by_id` reports the actor it killed; the actors below it end after it.
//! These tests kill a supervisor whose child cannot end yet (its `Drop` waits
//! on a gate, or its handler waits on a notification) and check that the
//! observer answers only when the child is gone, with the child's names and
//! ids free.
//!
//! Every gate opens on drop, so a failed assertion never leaves a worker
//! blocked, every wait carries its own timeout, and no step is ordered by a
//! sleep: a timeout only reports a hang, or marks the window in which an
//! answer must not come.

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    types::ActorId,
    ActorConfig, ActorHandle, ActorResult, ActorStatus, ActorSystem, RestartType, Shutdown,
    SpawnError, StopOutcome, SubtreeEnd, SubtreeObserver, SupervisionConfig,
};

mod common;

/// How long a call that must return takes at most.
const BOUND: Duration = Duration::from_secs(10);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// How long an answer that must not come is given to come.
const PENDING: Duration = Duration::from_millis(100);

/// A kill grace no wait in these tests comes near: an abort after it cannot be
/// what ends an actor here.
const FAR_GRACE: Duration = Duration::from_secs(30);

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

/// A gate a `Drop` waits on.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *(self.0).0.lock().unwrap() = true;
        (self.0).1.notify_all();
    }

    /// Blocks the calling thread until the gate opens, or until the hang bound
    /// passes.
    fn wait(&self) {
        let (lock, changed) = &*self.0;
        let started = Instant::now();
        let mut open = lock.lock().unwrap();
        while !*open {
            let left = HANG_BOUND.saturating_sub(started.elapsed());
            if left.is_zero() {
                return;
            }
            open = changed.wait_timeout(open, left).unwrap().0;
        }
    }
}

/// Opens the gate when dropped.
struct OpenOnDrop(Gate);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// What a node's `Drop` does: wait on the gate, then set the flag.
#[derive(Clone)]
struct Probe {
    gate: Gate,
    dropped: Arc<AtomicBool>,
}

/// One actor of every tree below: it spawns the children it is told to, parks
/// in its handler, waits on an observer, or crashes.
struct Node {
    probe: Option<Probe>,
    starts: Option<Arc<AtomicUsize>>,
}

impl Node {
    fn plain() -> Self {
        Self {
            probe: None,
            starts: None,
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(probe) = self.probe.take() {
            probe.gate.wait();
            probe.dropped.store(true, Ordering::SeqCst);
        }
    }
}

/// A child to spawn.
#[derive(Clone)]
struct Spec {
    name: String,
    supervisor: bool,
    shutdown: Shutdown,
    grace: Duration,
    restart: RestartType,
    probe: Option<Probe>,
    starts: Option<Arc<AtomicUsize>>,
}

impl Spec {
    fn worker(name: &str) -> Self {
        Self {
            name: name.to_string(),
            supervisor: false,
            shutdown: Shutdown::Timeout(Duration::from_secs(5)),
            grace: FAR_GRACE,
            restart: RestartType::Permanent,
            probe: None,
            starts: None,
        }
    }

    fn supervisor(name: &str) -> Self {
        Self {
            supervisor: true,
            ..Self::worker(name)
        }
    }

    fn grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    fn shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }

    fn probe(mut self, gate: &Gate, dropped: &Arc<AtomicBool>) -> Self {
        self.probe = Some(Probe {
            gate: gate.clone(),
            dropped: dropped.clone(),
        });
        self
    }

    fn starts(mut self, starts: &Arc<AtomicUsize>) -> Self {
        self.starts = Some(starts.clone());
        self
    }
}

enum Cmd {
    /// Spawn a child.
    Spawn(Spec),
    /// Enter the handler, say so, and stay in it until released.
    Park {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    },
    /// Wait for the end of the observed subtree from inside the handler, and
    /// set the flag if the wait returns.
    Wait {
        observer: SubtreeObserver,
        entered: Arc<Notify>,
        returned: Arc<AtomicBool>,
    },
    /// Panic.
    Crash,
}

impl Actor for Node {
    type Message = Cmd;
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(starts) = &self.starts {
            starts.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn handle(&mut self, cmd: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match cmd {
            Cmd::Spawn(spec) => {
                let (probe, starts) = (spec.probe.clone(), spec.starts.clone());
                let mut config = ActorConfig::default();
                if spec.supervisor {
                    config = config.supervisor();
                }
                ctx.spawn_child(move || Node {
                    probe: probe.clone(),
                    starts: starts.clone(),
                })
                .named(spec.name)
                .shutdown(spec.shutdown)
                .restart_type(spec.restart)
                .kill_grace(spec.grace)
                .with_config(config)
                .await?;
                Ok(())
            }
            Cmd::Park { entered, release } => {
                entered.notify_one();
                release.notified().await;
                Ok(())
            }
            Cmd::Wait {
                observer,
                entered,
                returned,
            } => {
                entered.notify_one();
                observer.wait_ended().await;
                returned.store(true, Ordering::SeqCst);
                Ok(())
            }
            Cmd::Crash => panic!("the node crashed on command"),
        }
    }
}

/// A root supervisor with a restart budget no test comes near.
async fn root(sys: &Arc<ActorSystem>, name: &str, grace: Duration) -> ActorHandle<Node> {
    Node::plain()
        .spawn()
        .named(name)
        .on_system(sys)
        .kill_grace(grace)
        .with_supervision(
            SupervisionConfig::one_for_one().max_restarts(20, Duration::from_secs(60)),
        )
        .await
        .expect("the root spawns")
}

fn new_system(prefix: &str) -> Arc<ActorSystem> {
    ActorSystem::create(unique(prefix)).expect("the system is created")
}

/// A boss with one leaf whose `Drop` waits on the gate.
struct Tree {
    sys: Arc<ActorSystem>,
    boss: ActorHandle<Node>,
    leaf: String,
    gate: Gate,
    _open: OpenOnDrop,
    dropped: Arc<AtomicBool>,
}

async fn gated_tree(leaf_grace: Duration) -> Tree {
    let sys = new_system("observe");
    let gate = Gate::default();
    let open = OpenOnDrop(gate.clone());
    let dropped = Arc::new(AtomicBool::new(false));
    let boss = root(&sys, &unique("boss"), Duration::from_millis(120)).await;
    let leaf = unique("leaf");
    boss.send(Cmd::Spawn(
        Spec::worker(&leaf).probe(&gate, &dropped).grace(leaf_grace),
    ))
    .await
    .expect("the leaf spawns");
    Tree {
        sys,
        boss,
        leaf,
        gate,
        _open: open,
        dropped,
    }
}

/// The answer of `fut` if it is ready at its first poll, else `None`.
fn ready_now<F: Future>(fut: F) -> Option<F::Output> {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(answer) => Some(answer),
        Poll::Pending => None,
    }
}

/// Polls `ready` until it holds or the hang bound passes.
async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < HANG_BOUND,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// The window of a wait that ran out.
#[derive(Debug)]
struct Elapsed;

/// [`timeout`] measured in real time and polled by hand: the future is polled
/// every few milliseconds on the calling thread, and no timer of the runtime
/// is in the window. A worker held inside a `Drop` can leave a multi-thread
/// runtime without a thread that drives its timers, and a window measured by
/// that timer then lasts as long as the `Drop`.
async fn real_timeout<F: Future>(window: Duration, fut: F) -> Result<F::Output, Elapsed> {
    let mut fut = Box::pin(fut);
    let mut cx = Context::from_waker(Waker::noop());
    let started = Instant::now();
    loop {
        if let Poll::Ready(output) = fut.as_mut().poll(&mut cx) {
            return Ok(output);
        }
        if started.elapsed() >= window {
            return Err(Elapsed);
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Spawns a root actor under `name`: it fails with `NameTaken` while another
/// actor holds the name.
async fn respawn(sys: &Arc<ActorSystem>, name: &str) -> Result<ActorHandle<Node>, SpawnError> {
    Node::plain().spawn().named(name).on_system(sys).await
}

// ---------------------------------------------------------------------------
// kill_by_id, then the observer
// ---------------------------------------------------------------------------

#[test]
fn wait_ended_waits_for_a_child_stuck_in_its_drop_after_kill_by_id() {
    common::run_multi_thread_only(2, async {
        for round in 0..10 {
            let tree = gated_tree(FAR_GRACE).await;
            let id = tree.boss.id().clone();
            let observer = tree
                .sys
                .observe_subtree(&id)
                .expect("the boss is registered");
            let report = real_timeout(BOUND, tree.sys.kill_by_id(&id))
                .await
                .expect("kill_by_id returns while the leaf is blocked")
                .expect("the boss was registered");
            assert!(
                matches!(
                    report.outcome,
                    StopOutcome::Killed | StopOutcome::Aborted | StopOutcome::Graceful
                ),
                "round {round}: the boss ended: {report:?}"
            );
            assert!(
                !tree.dropped.load(Ordering::SeqCst),
                "round {round}: the leaf is still in its Drop"
            );
            assert!(
                tree.sys.observe_subtree(&id).is_none(),
                "round {round}: the killed boss left the registry"
            );

            let mut wait = Box::pin(observer.wait_ended());
            assert!(
                real_timeout(PENDING, &mut wait).await.is_err(),
                "round {round}: wait_ended answered while the leaf was still running"
            );
            tree.gate.open();
            let end = real_timeout(HANG_BOUND, &mut wait)
                .await
                .expect("the wait answers once the leaf is gone");
            assert_eq!(end, SubtreeEnd::Ended, "round {round}");
            assert!(
                tree.dropped.load(Ordering::SeqCst),
                "round {round}: Ended came after the leaf's Drop"
            );
            assert!(tree.sys.actor_ids().is_empty(), "round {round}");
        }
    });
}

/// A killed supervisor's id leaves the registry while a child below it still
/// runs in its handler: a lookup made after the kill reads the same as an id
/// nobody registered, and the child's name is still taken.
#[test]
fn a_lookup_after_the_kill_cannot_tell_a_running_subtree_from_an_unknown_id() {
    common::run_multi_thread_only(2, async {
        let sys = new_system("lookup");
        let boss = root(&sys, &unique("boss"), Duration::from_millis(120)).await;
        let leaf_name = unique("leaf");
        boss.send(Cmd::Spawn(Spec::worker(&leaf_name)))
            .await
            .expect("the leaf spawns");
        let leaf = sys.get::<Node>(&leaf_name).expect("the leaf is registered");
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        leaf.notify(Cmd::Park {
            entered: entered.clone(),
            release: release.clone(),
        })
        .await
        .unwrap();
        timeout(BOUND, entered.notified())
            .await
            .expect("the leaf is in its handler");

        let id = boss.id().clone();
        let observer = sys.observe_subtree(&id).expect("the boss is registered");
        timeout(BOUND, sys.kill_by_id(&id))
            .await
            .expect("kill_by_id returns")
            .expect("the boss was registered");

        assert!(
            sys.observe_subtree(&id).is_none(),
            "a lookup after the kill finds nothing"
        );
        assert_eq!(
            sys.actor_ids(),
            vec![leaf.id().clone()],
            "the leaf still runs in its handler"
        );
        let mut wait = Box::pin(observer.wait_ended());
        assert!(
            timeout(PENDING, &mut wait).await.is_err(),
            "wait_ended answered while the leaf was still running"
        );
        assert!(
            matches!(
                respawn(&sys, &leaf_name).await,
                Err(SpawnError::NameTaken { .. })
            ),
            "the running leaf still holds its name"
        );

        release.notify_one();
        assert_eq!(
            timeout(BOUND, &mut wait).await.expect("the wait answers"),
            SubtreeEnd::Ended
        );
        respawn(&sys, &leaf_name)
            .await
            .expect("Ended means the leaf's name is free");
        assert!(sys.actor_ids().len() <= 1, "only the respawned actor");
    });
}

/// A handle taken after the end names the same incarnation, and a call made
/// after the subtree ended answers at once.
#[test]
fn a_handle_observer_taken_after_the_end_waits_for_the_subtree() {
    common::run_multi_thread_only(2, async {
        let tree = gated_tree(FAR_GRACE).await;
        let id = tree.boss.id().clone();
        real_timeout(BOUND, tree.sys.kill_by_id(&id))
            .await
            .expect("kill_by_id returns")
            .expect("the boss was registered");
        real_timeout(BOUND, tree.boss.wait_stopped())
            .await
            .expect("the boss ended");

        let late = tree.boss.observe_subtree();
        let mut wait = Box::pin(late.wait_ended());
        assert!(
            real_timeout(PENDING, &mut wait).await.is_err(),
            "a handle observer taken after the boss ended waits for the leaf"
        );
        tree.gate.open();
        assert_eq!(
            real_timeout(HANG_BOUND, &mut wait).await.expect("answers"),
            SubtreeEnd::Ended
        );
        assert!(tree.dropped.load(Ordering::SeqCst));

        assert_eq!(
            ready_now(late.wait_ended()),
            Some(SubtreeEnd::Ended),
            "a call after the subtree ended answers at once"
        );
        assert_eq!(
            ready_now(tree.boss.observe_subtree().wait_ended()),
            Some(SubtreeEnd::Ended),
            "so does a new observer"
        );
    });
}

#[test]
fn a_handle_observer_taken_before_the_end_works_without_the_system() {
    common::run_multi_thread_only(2, async {
        let tree = gated_tree(FAR_GRACE).await;
        let observer = tree.boss.observe_subtree();
        let mut wait = Box::pin(observer.wait_ended());
        assert!(
            real_timeout(PENDING, &mut wait).await.is_err(),
            "the boss still runs"
        );
        real_timeout(BOUND, tree.sys.kill_by_id(tree.boss.id()))
            .await
            .expect("kill_by_id returns");
        assert!(real_timeout(PENDING, &mut wait).await.is_err());
        tree.gate.open();
        assert_eq!(
            real_timeout(HANG_BOUND, &mut wait).await.expect("answers"),
            SubtreeEnd::Ended
        );
    });
}

// ---------------------------------------------------------------------------
// Ended means the names and ids are free
// ---------------------------------------------------------------------------

/// On a single thread nothing else runs between two statements that do not
/// await, so this is the order the end releases things in, with no help from
/// parallelism.
#[test]
fn wait_ended_ends_on_a_current_thread_runtime_with_names_and_ids_free() {
    common::run_current_thread_only(async {
        for round in 0..50 {
            let sys = new_system("single");
            let open = Gate::default();
            open.open();
            let dropped = Arc::new(AtomicBool::new(false));
            let boss_name = unique("boss");
            let boss = root(&sys, &boss_name, Duration::from_millis(120)).await;
            let leaf = unique("leaf");
            boss.send(Cmd::Spawn(
                Spec::worker(&leaf).probe(&open, &dropped).grace(FAR_GRACE),
            ))
            .await
            .expect("the leaf spawns");
            let id = boss.id().clone();
            let observer = sys.observe_subtree(&id).expect("registered");

            timeout(BOUND, sys.kill_by_id(&id))
                .await
                .expect("kill_by_id returns")
                .expect("the boss was registered");
            let end = timeout(BOUND, observer.wait_ended())
                .await
                .expect("the wait answers");
            assert_eq!(end, SubtreeEnd::Ended, "round {round}");
            assert!(dropped.load(Ordering::SeqCst), "round {round}: Drop ran");
            assert!(sys.actor_ids().is_empty(), "round {round}");
            assert!(sys.observe_subtree(&id).is_none(), "round {round}");
            respawn(&sys, &leaf)
                .await
                .unwrap_or_else(|err| panic!("round {round}: the leaf's name is taken: {err}"));
            respawn(&sys, &boss_name)
                .await
                .unwrap_or_else(|err| panic!("round {round}: the boss's name is taken: {err}"));
        }
    });
}

/// Three levels, a middle supervisor under a bounded and under an unbounded
/// window, and a leaf whose `Drop` waits on a gate: `Ended` comes after the
/// leaf's `Drop`, and the names of all three levels are free.
async fn depth_case(middle: Shutdown) {
    let sys = new_system("depth");
    let gate = Gate::default();
    let _open = OpenOnDrop(gate.clone());
    let dropped = Arc::new(AtomicBool::new(false));
    let (top_name, mid_name, leaf_name) = (unique("top"), unique("mid"), unique("leaf"));
    let top = root(&sys, &top_name, Duration::from_millis(120)).await;
    top.send(Cmd::Spawn(Spec::supervisor(&mid_name).shutdown(middle)))
        .await
        .expect("the middle supervisor spawns");
    let mid = sys
        .get::<Node>(&mid_name)
        .expect("the middle is registered");
    mid.send(Cmd::Spawn(Spec::worker(&leaf_name).probe(&gate, &dropped)))
        .await
        .expect("the leaf spawns");

    let id = top.id().clone();
    let observer = sys.observe_subtree(&id).expect("registered");
    real_timeout(BOUND, sys.kill_by_id(&id))
        .await
        .expect("kill_by_id returns")
        .expect("the top was registered");
    assert!(!dropped.load(Ordering::SeqCst));
    let mut wait = Box::pin(observer.wait_ended());
    assert!(
        real_timeout(PENDING, &mut wait).await.is_err(),
        "wait_ended answered while the leaf, two levels down, was still running ({middle:?})"
    );
    gate.open();
    assert_eq!(
        real_timeout(HANG_BOUND, &mut wait).await.expect("answers"),
        SubtreeEnd::Ended,
        "{middle:?}"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "Ended came after the leaf's Drop"
    );
    assert!(sys.actor_ids().is_empty(), "{middle:?}");
    for name in [&top_name, &mid_name, &leaf_name] {
        respawn(&sys, name)
            .await
            .unwrap_or_else(|err| panic!("{middle:?}: the name {name} is taken: {err}"));
    }
}

#[test]
fn wait_ended_covers_a_subtree_three_levels_deep_under_a_bounded_middle() {
    common::run_multi_thread_only(2, depth_case(Shutdown::Timeout(Duration::from_secs(5))));
}

#[test]
fn wait_ended_covers_a_subtree_three_levels_deep_under_an_unbounded_middle() {
    common::run_multi_thread_only(2, depth_case(Shutdown::Infinity));
}

/// The names are free on a multi-thread runtime too, after an end by `Kill`
/// and after an end by abort, where the task never publishes `Stopped`.
#[test]
fn names_are_free_when_wait_ended_answers_after_a_kill_and_after_an_abort() {
    common::run_multi_thread_only(2, async {
        for aborted in [false, true] {
            for round in 0..10 {
                let sys = new_system("names");
                let (top_name, mid_name, leaf_name) =
                    (unique("top"), unique("mid"), unique("leaf"));
                let grace = if aborted {
                    Duration::ZERO
                } else {
                    Duration::from_millis(120)
                };
                let top = root(&sys, &top_name, grace).await;
                top.send(Cmd::Spawn(Spec::supervisor(&mid_name).grace(grace)))
                    .await
                    .unwrap();
                let mid = sys.get::<Node>(&mid_name).expect("registered");
                mid.send(Cmd::Spawn(Spec::worker(&leaf_name).grace(grace)))
                    .await
                    .unwrap();
                let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
                if aborted {
                    top.notify(Cmd::Park {
                        entered: entered.clone(),
                        release: release.clone(),
                    })
                    .await
                    .unwrap();
                    timeout(BOUND, entered.notified())
                        .await
                        .expect("the top is in its handler");
                }
                let id = top.id().clone();
                let observer = sys.observe_subtree(&id).expect("registered");
                let report = timeout(BOUND, sys.kill_by_id(&id))
                    .await
                    .expect("kill_by_id returns")
                    .expect("registered");
                if aborted {
                    assert_eq!(report.outcome, StopOutcome::Aborted, "round {round}");
                }
                assert_eq!(
                    timeout(HANG_BOUND, observer.wait_ended())
                        .await
                        .expect("the wait answers"),
                    SubtreeEnd::Ended,
                    "aborted {aborted}, round {round}"
                );
                for name in [&top_name, &mid_name, &leaf_name] {
                    respawn(&sys, name).await.unwrap_or_else(|err| {
                        panic!("aborted {aborted}, round {round}: {name} is taken: {err}")
                    });
                }
                drop(release);
            }
        }
    });
}

/// An observed actor that ends by abort never publishes `Stopped`: the plane
/// closing is its end.
#[test]
fn an_observed_actor_that_ends_by_abort_is_ended_when_its_plane_closes() {
    common::run_multi_thread_only(2, async {
        let sys = new_system("abort");
        let node = Node::plain()
            .spawn()
            .named(unique("held"))
            .on_system(&sys)
            .kill_grace(Duration::ZERO)
            .await
            .expect("spawns");
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        node.notify(Cmd::Park {
            entered: entered.clone(),
            release,
        })
        .await
        .unwrap();
        timeout(BOUND, entered.notified())
            .await
            .expect("the actor is held in its handler");
        let observer = sys.observe_subtree(node.id()).expect("registered");
        let report = timeout(BOUND, sys.kill_by_id(node.id()))
            .await
            .expect("kill_by_id returns")
            .expect("registered");
        assert_eq!(
            report.outcome,
            StopOutcome::Aborted,
            "the task ended without publishing Stopped"
        );
        assert_eq!(
            timeout(BOUND, observer.wait_ended())
                .await
                .expect("the wait answers"),
            SubtreeEnd::Ended
        );
    });
}

// ---------------------------------------------------------------------------
// Registered or not
// ---------------------------------------------------------------------------

#[test]
fn an_unregistered_id_has_no_observer() {
    common::run_multi_thread_only(2, async {
        let sys = new_system("none");
        assert!(sys.observe_subtree(&"nobody".into()).is_none());

        let tree = gated_tree(FAR_GRACE).await;
        assert!(tree.sys.observe_subtree(&"nobody".into()).is_none());
        let id = tree.boss.id().clone();
        real_timeout(BOUND, tree.sys.kill_by_id(&id))
            .await
            .expect("returns");
        tree.gate.open();
        real_timeout(HANG_BOUND, tree.boss.observe_subtree().wait_ended())
            .await
            .expect("the subtree ends");
        assert!(
            tree.sys.observe_subtree(&id).is_none(),
            "after the actor and its subtree ended"
        );
        assert!(tree
            .sys
            .observe_subtree(&tree.leaf.as_str().into())
            .is_none());

        let live = root(&sys, &unique("live"), Duration::from_millis(120)).await;
        let live_id = live.id().clone();
        assert!(sys.observe_subtree(&live_id).is_some());
        sys.shutdown().await;
        assert!(
            sys.observe_subtree(&live_id).is_none(),
            "after the system's shutdown"
        );
    });
}

// ---------------------------------------------------------------------------
// One incarnation
// ---------------------------------------------------------------------------

#[test]
fn an_observer_names_one_incarnation() {
    common::run_multi_thread_only(2, async {
        let sys = new_system("incarnation");
        let top = root(&sys, &unique("top"), Duration::from_millis(120)).await;
        let starts = Arc::new(AtomicUsize::new(0));
        let name = unique("x");
        top.send(Cmd::Spawn(Spec::worker(&name).starts(&starts)))
            .await
            .unwrap();
        let id = ActorId::from(name.as_str());
        // A spawn does not wait for the child's start: the kill below must
        // find a child that has started, or the first incarnation never
        // counts.
        wait_until("the first start", || starts.load(Ordering::SeqCst) == 1).await;
        let x = sys.get::<Node>(&name).expect("x is registered");
        let first = sys.observe_subtree(&id).expect("registered");

        // The first incarnation is killed by id and the supervisor starts a
        // second.
        timeout(BOUND, sys.kill_by_id(&id))
            .await
            .expect("kill_by_id returns")
            .expect("registered");
        wait_until("the restart", || {
            starts.load(Ordering::SeqCst) == 2
                && sys.actor_status(&id) == Some(ActorStatus::Running)
        })
        .await;

        assert_eq!(
            timeout(BOUND, first.wait_ended())
                .await
                .expect("the first incarnation's subtree ended"),
            SubtreeEnd::Ended
        );
        assert_eq!(
            sys.actor_status(&id),
            Some(ActorStatus::Running),
            "the new incarnation runs"
        );
        let second = sys.observe_subtree(&id).expect("registered");
        assert!(
            timeout(PENDING, second.wait_ended()).await.is_err(),
            "an observer of the new incarnation waits for it"
        );
        // A handle kept across the restart names the earlier incarnation.
        assert_eq!(
            ready_now(x.observe_subtree().wait_ended()),
            Some(SubtreeEnd::Ended)
        );
    });
}

/// `kill_by_id_with_observer` takes the observer and the kill from one lookup:
/// the observer names the incarnation the report names, while an observer
/// taken before a restart that happened in between names the earlier one.
#[test]
fn kill_by_id_with_observer_names_the_incarnation_it_killed() {
    common::run_multi_thread_only(2, async {
        let sys = new_system("paired");
        let top = root(&sys, &unique("top"), Duration::from_millis(120)).await;
        let starts = Arc::new(AtomicUsize::new(0));
        let name = unique("x");
        top.send(Cmd::Spawn(Spec::supervisor(&name).starts(&starts)))
            .await
            .unwrap();
        let id = ActorId::from(name.as_str());

        // An observer taken now names the first incarnation.
        let stale = sys.observe_subtree(&id).expect("registered");
        let x = sys.get::<Node>(&name).expect("registered");
        let _ = x.send(Cmd::Crash).await;
        wait_until("the second incarnation", || {
            starts.load(Ordering::SeqCst) == 2
                && sys.actor_status(&id) == Some(ActorStatus::Running)
        })
        .await;

        // The second incarnation has a child that cannot end yet.
        let x2 = sys.get::<Node>(&name).expect("registered");
        let leaf_name = unique("leaf");
        x2.send(Cmd::Spawn(Spec::worker(&leaf_name))).await.unwrap();
        let leaf = sys.get::<Node>(&leaf_name).expect("registered");
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        leaf.notify(Cmd::Park {
            entered: entered.clone(),
            release: release.clone(),
        })
        .await
        .unwrap();
        timeout(BOUND, entered.notified())
            .await
            .expect("the leaf is in its handler");

        let (report, observer) = timeout(BOUND, sys.kill_by_id_with_observer(&id))
            .await
            .expect("returns")
            .expect("registered");
        assert!(matches!(
            report.outcome,
            StopOutcome::Killed | StopOutcome::Aborted
        ));
        let mut wait = Box::pin(observer.wait_ended());
        assert!(
            timeout(PENDING, &mut wait).await.is_err(),
            "the observer of the killed incarnation waits for its leaf"
        );
        assert_eq!(
            timeout(BOUND, stale.wait_ended())
                .await
                .expect("the earlier incarnation's subtree ended before the restart began"),
            SubtreeEnd::Ended,
            "an observer taken before the restart names the earlier incarnation"
        );
        release.notify_one();
        assert_eq!(
            timeout(BOUND, &mut wait).await.expect("the wait answers"),
            SubtreeEnd::Ended
        );
        respawn(&sys, &leaf_name)
            .await
            .expect("Ended means the leaf's name is free");
    });
}

#[test]
fn kill_by_id_with_observer_answers_none_where_kill_by_id_does() {
    common::run_multi_thread_only(2, async {
        let sys = new_system("paired-none");
        assert!(sys
            .kill_by_id_with_observer(&"nobody".into())
            .await
            .is_none());
        assert!(sys.kill_by_id(&"nobody".into()).await.is_none());
    });
}

// ---------------------------------------------------------------------------
// Only reads
// ---------------------------------------------------------------------------

#[test]
fn dropping_the_wait_or_the_observer_cancels_nothing() {
    common::run_multi_thread_only(2, async {
        let tree = gated_tree(FAR_GRACE).await;
        let id = tree.boss.id().clone();
        let observer = tree.sys.observe_subtree(&id).expect("registered");

        // Dropped in its first part, while the boss runs.
        {
            let mut wait = Box::pin(observer.wait_ended());
            assert!(real_timeout(PENDING, &mut wait).await.is_err());
        }
        real_timeout(BOUND, tree.sys.kill_by_id(&id))
            .await
            .expect("the kill proceeds")
            .expect("registered");
        // Dropped in its second part, while the leaf is in its Drop.
        {
            let mut wait = Box::pin(observer.wait_ended());
            assert!(real_timeout(PENDING, &mut wait).await.is_err());
        }
        // Observers dropped without being awaited.
        drop(tree.boss.observe_subtree());
        drop(observer);

        assert!(
            !tree.dropped.load(Ordering::SeqCst),
            "nothing was cancelled or finished early"
        );
        tree.gate.open();
        wait_until("the leaf's end", || tree.sys.actor_ids().is_empty()).await;
        assert_eq!(
            real_timeout(HANG_BOUND, tree.boss.observe_subtree().wait_ended())
                .await
                .expect("a fresh wait answers"),
            SubtreeEnd::Ended
        );
        assert!(tree.dropped.load(Ordering::SeqCst));
    });
}

#[test]
fn a_caller_runtime_that_ends_mid_wait_changes_nothing() {
    let target = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let tree = target.block_on(gated_tree(FAR_GRACE));
    let id = tree.boss.id().clone();
    let observer = tree.sys.observe_subtree(&id).expect("registered");
    target
        .block_on(async { real_timeout(BOUND, tree.sys.kill_by_id(&id)).await })
        .expect("kill_by_id returns")
        .expect("registered");

    // The wait runs as a task of a second runtime, which then ends.
    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    caller.block_on(async {
        let waiting = observer.clone();
        let task = tokio::spawn(async move { waiting.wait_ended().await });
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(!task.is_finished(), "the leaf is still in its Drop");
    });
    drop(caller);

    assert!(!tree.dropped.load(Ordering::SeqCst));
    tree.gate.open();
    let fresh = tree.boss.observe_subtree();
    let end = target
        .block_on(async { real_timeout(HANG_BOUND, fresh.wait_ended()).await })
        .expect("a new observer answers");
    assert_eq!(end, SubtreeEnd::Ended);
    assert!(tree.dropped.load(Ordering::SeqCst));
    assert!(tree.sys.actor_ids().is_empty());
}

// ---------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------

/// A child that cannot end even when aborted (its `Drop` blocks a thread) is
/// reported unresponsive once the stop budgets of the subtree have run out,
/// and a later call answers `Ended` when it is gone.
#[test]
fn a_descendant_stuck_in_its_drop_is_unresponsive_then_ended_after_release() {
    common::run_multi_thread_only(2, async {
        let tree = gated_tree(Duration::ZERO).await;
        let id = tree.boss.id().clone();
        let observer = tree.sys.observe_subtree(&id).expect("registered");
        real_timeout(BOUND, tree.sys.kill_by_id(&id))
            .await
            .expect("kill_by_id returns")
            .expect("registered");

        let first = real_timeout(HANG_BOUND, observer.wait_ended())
            .await
            .expect("the verdict comes before the hang bound");
        assert_eq!(first, SubtreeEnd::Unresponsive);
        assert!(
            !tree.dropped.load(Ordering::SeqCst),
            "the wait stopped nothing: the Drop is still blocked"
        );

        tree.gate.open();
        let second = real_timeout(HANG_BOUND, observer.wait_ended())
            .await
            .expect("a later call answers");
        assert_eq!(second, SubtreeEnd::Ended);
        assert!(tree.dropped.load(Ordering::SeqCst));
    });
}

// ---------------------------------------------------------------------------
// Called from inside the tree
// ---------------------------------------------------------------------------

/// Called from a callback of the observed actor, the wait is for the caller's
/// own end: it stays pending until the kill drops the callback.
#[test]
fn a_wait_made_by_the_observed_actor_s_handler_never_answers() {
    common::run_multi_thread_only(2, async {
        let sys = new_system("self-wait");
        let node = Node::plain()
            .spawn()
            .named(unique("self"))
            .on_system(&sys)
            .kill_grace(Duration::ZERO)
            .await
            .expect("spawns");
        let observer = node.observe_subtree();
        let (entered, returned) = (Arc::new(Notify::new()), Arc::new(AtomicBool::new(false)));
        node.notify(Cmd::Wait {
            observer: observer.clone(),
            entered: entered.clone(),
            returned: returned.clone(),
        })
        .await
        .unwrap();
        timeout(BOUND, entered.notified())
            .await
            .expect("the handler is waiting");
        assert!(
            timeout(PENDING, observer.wait_ended()).await.is_err(),
            "the observed actor's end has not come"
        );
        assert!(!returned.load(Ordering::SeqCst));

        timeout(BOUND, sys.kill_by_id(node.id()))
            .await
            .expect("returns")
            .expect("registered");
        assert_eq!(
            timeout(BOUND, observer.wait_ended())
                .await
                .expect("the actor ended"),
            SubtreeEnd::Ended
        );
        assert!(
            !returned.load(Ordering::SeqCst),
            "the callback's own wait never answered: the kill dropped it"
        );
    });
}

/// Called from a callback of an actor below the observed one, the subtree the
/// wait is for contains the caller, so the call is dropped with the caller.
#[test]
fn a_wait_made_by_a_descendant_s_handler_never_answers() {
    common::run_multi_thread_only(2, async {
        let sys = new_system("descendant-wait");
        let boss = root(&sys, &unique("boss"), Duration::from_millis(120)).await;
        let leaf_name = unique("leaf");
        boss.send(Cmd::Spawn(Spec::worker(&leaf_name).grace(Duration::ZERO)))
            .await
            .unwrap();
        let leaf = sys.get::<Node>(&leaf_name).expect("registered");
        let observer = boss.observe_subtree();
        let (entered, returned) = (Arc::new(Notify::new()), Arc::new(AtomicBool::new(false)));
        leaf.notify(Cmd::Wait {
            observer: observer.clone(),
            entered: entered.clone(),
            returned: returned.clone(),
        })
        .await
        .unwrap();
        timeout(BOUND, entered.notified())
            .await
            .expect("the leaf's handler is waiting");
        assert!(!returned.load(Ordering::SeqCst));

        timeout(BOUND, sys.kill_by_id(boss.id()))
            .await
            .expect("returns")
            .expect("registered");
        assert_eq!(
            timeout(HANG_BOUND, observer.wait_ended())
                .await
                .expect("the subtree ended once the leaf's callback was cut"),
            SubtreeEnd::Ended
        );
        assert!(
            !returned.load(Ordering::SeqCst),
            "the leaf's own wait never answered"
        );
        assert!(sys.actor_ids().is_empty());
    });
}

// ---------------------------------------------------------------------------
// A budget too large to count
// ---------------------------------------------------------------------------

/// A kill grace of `Duration::MAX` is the documented way to say a worker is
/// never aborted. No supervisor child under `Infinity` lives anywhere, and
/// the wait has no bound all the same: it ends when the worker does.
#[test]
fn a_kill_grace_too_large_to_count_leaves_the_wait_without_a_bound() {
    common::run_paused(async {
        let sys = new_system("unbounded");
        let boss = root(&sys, &unique("boss"), Duration::from_millis(120)).await;
        let leaf_name = unique("leaf");
        boss.send(Cmd::Spawn(Spec::worker(&leaf_name).grace(Duration::MAX)))
            .await
            .unwrap();
        let leaf = sys.get::<Node>(&leaf_name).expect("registered");
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        leaf.notify(Cmd::Park {
            entered: entered.clone(),
            release: release.clone(),
        })
        .await
        .unwrap();
        entered.notified().await;

        let id = boss.id().clone();
        let observer = sys.observe_subtree(&id).expect("registered");
        sys.kill_by_id(&id).await.expect("registered");

        let mut wait = Box::pin(observer.wait_ended());
        for year in 1..=5 {
            assert!(
                timeout(Duration::from_secs(365 * 86_400), &mut wait)
                    .await
                    .is_err(),
                "year {year}: a worker that is never aborted is no verdict"
            );
        }
        release.notify_one();
        assert_eq!(wait.await, SubtreeEnd::Ended);
    });
}

// ---------------------------------------------------------------------------
// The types
// ---------------------------------------------------------------------------

#[test]
fn the_observer_is_clone_send_sync_debug_and_its_wait_is_send() {
    fn bounds<T: Clone + Send + Sync + std::fmt::Debug + 'static>() {}
    fn plain<T: Copy + Eq + std::fmt::Debug + Send + Sync + 'static>() {}
    bounds::<SubtreeObserver>();
    plain::<SubtreeEnd>();
    common::run_multi_thread_only(2, async {
        let sys = new_system("types");
        let node = root(&sys, &unique("types"), Duration::from_millis(120)).await;
        let observer = node.observe_subtree();
        assert_eq!(format!("{observer:?}"), "SubtreeObserver { .. }");
        let waiting = observer.clone();
        let task = tokio::spawn(async move { waiting.wait_ended().await });
        timeout(BOUND, sys.kill_by_id(node.id()))
            .await
            .expect("returns");
        assert_eq!(
            timeout(BOUND, task)
                .await
                .expect("the task answers")
                .expect("the task did not panic"),
            SubtreeEnd::Ended
        );
        let from_the_system = sys.observe_subtree(node.id());
        assert!(from_the_system.is_none());
    });
}
