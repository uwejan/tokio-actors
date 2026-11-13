#![allow(dead_code)]
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

// ---------------------------------------------------------------------------
// Ended means the names and ids are free
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Registered or not
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// One incarnation
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// A budget too large to count
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The types
// ---------------------------------------------------------------------------
