//! A wait that can give the Unresponsive verdict never gives it before the
//! abort that could end the actor it waits for, nor in the moment between
//! an owed actor's own end and the guards of its children.
//!
//! Both scenarios terminate a supervisor child under `Shutdown::Kill` whose
//! own child is left to its link guard: `ParentKilled` at once, `Kill` at
//! the end of its `Shutdown` window, and the abort one kill grace after that
//! `Kill`. The terminating call waits for that subtree. The runtime's only
//! thread is then stalled (a blocking sleep inside a task) across both the
//! instant the `Kill` is due and the deadline of the terminating call's
//! wait, as a host that stops the process would stall it. When the thread
//! runs again, the `Kill` fires late and its abort is still owed, one kill
//! grace away, so the wait holds its verdict for it.
//!
//! In the first scenario, the child left to its guard is parked at an
//! `.await` inside a turn: it ends at the late abort, and the call succeeds.
//! In the second, that child is a supervisor stopping its own parked worker
//! in order when the late `Kill` reaches it: it obeys the `Kill` and ends on
//! its own, which hands the worker to its guard, whose `Kill` and abort
//! follow only once the runtime drops the worker's runner. The end of an
//! owed actor counts as its abort, so the wait holds the settle time for
//! that cascade instead of giving its verdict in the moment between the end
//! and the guard, which a multi-thread runtime polls first.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, ActorStatus, ActorSystem, RestartType, Shutdown, SupervisionConfig,
};

mod common;

/// The parked supervisor's `Shutdown` window, in the first scenario.
const WINDOW: Duration = Duration::from_millis(600);
/// The parked supervisor's kill grace, in the first scenario.
const GRACE: Duration = Duration::from_millis(200);
/// The stopping supervisor's `Shutdown` window, in the second scenario.
const INNER_WINDOW: Duration = Duration::from_millis(300);
/// The stopping supervisor's kill grace, in the second scenario.
const INNER_GRACE: Duration = Duration::from_millis(200);
/// The parked worker's kill grace, in the second scenario.
const WORKER_GRACE: Duration = Duration::from_millis(100);
/// The parked worker's `Shutdown` window: longer than the test, so its
/// supervisor is still stopping it in order when the late `Kill` arrives.
const WORKER_WINDOW: Duration = Duration::from_secs(10);
/// How long after the terminated child ended the thread stalls.
const STALL_AFTER: Duration = Duration::from_millis(100);
/// Bounds every wait of the test; only a hang reaches it.
const BOUND: Duration = Duration::from_secs(10);

/// What the test reads of the parked actor.
#[derive(Default)]
struct Parked {
    /// Set when its turn parked.
    parked: AtomicBool,
    /// Set when its parked turn was torn down.
    ended: AtomicBool,
}

/// Sets `ended` when the parked turn's future is dropped.
struct EndsTheTurn(Arc<Parked>);

impl Drop for EndsTheTurn {
    fn drop(&mut self) {
        self.0.ended.store(true, Ordering::SeqCst);
    }
}

/// An actor whose one message parks its turn at an `.await` that never
/// completes: a supervisor with no children in the first scenario, a worker
/// in the second.
struct Leaf {
    probe: Arc<Parked>,
}

impl Actor for Leaf {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _ends = EndsTheTurn(self.probe.clone());
        self.probe.parked.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// A supervisor whose one child is the parked `Leaf` worker: when its own
/// supervisor is killed it stops the worker in order, which the worker's
/// window makes a long wait.
struct Inner {
    leaf: String,
    probe: Arc<Parked>,
}

impl Actor for Inner {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let probe = self.probe.clone();
        ctx.spawn_child(move || Leaf {
            probe: probe.clone(),
        })
        .named(self.leaf.clone())
        .shutdown(Shutdown::Timeout(WORKER_WINDOW))
        .kill_grace(WORKER_GRACE)
        .await
        .map_err(|e| tokio_actors::ActorError::user(e.to_string()))?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// What the terminated child has below it.
#[derive(Clone, Copy)]
enum Below {
    /// The parked `Leaf`, as a supervisor with no children.
    ParkedSupervisor,
    /// The `Inner` supervisor, with the parked `Leaf` as its worker.
    StoppingSupervisor,
}

impl Below {
    /// The bound of the terminating call's wait for the subtree below the
    /// terminated child: the longest chain of stop budgets in it, each
    /// followed by a settle time of 100 ms, plus one more settle time.
    fn wait_bound(self) -> Duration {
        let settle = Duration::from_millis(100);
        match self {
            Below::ParkedSupervisor => WINDOW + GRACE + settle + settle,
            Below::StoppingSupervisor => {
                INNER_WINDOW + INNER_GRACE + settle + WORKER_GRACE + settle + settle
            }
        }
    }

    /// How long the thread stalls: past the `Kill` and past the wait's
    /// deadline, well short of the cap, the wait's own bound counted once
    /// more from that deadline.
    fn stall(self) -> Duration {
        self.wait_bound() + Duration::from_millis(200)
    }
}

/// The supervisor terminated under `Shutdown::Kill`: it spawns what `below`
/// names.
struct Mid {
    below: Below,
    inner: String,
    leaf: String,
    probe: Arc<Parked>,
}

impl Actor for Mid {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let probe = self.probe.clone();
        let spawned = match self.below {
            Below::ParkedSupervisor => ctx
                .spawn_child(move || Leaf {
                    probe: probe.clone(),
                })
                .named(self.leaf.clone())
                .with_config(
                    ActorConfig::default().with_supervision(SupervisionConfig::one_for_one()),
                )
                .shutdown(Shutdown::Timeout(WINDOW))
                .kill_grace(GRACE)
                .await
                .map(|_| ()),
            Below::StoppingSupervisor => {
                let leaf = self.leaf.clone();
                ctx.spawn_child(move || Inner {
                    leaf: leaf.clone(),
                    probe: probe.clone(),
                })
                .named(self.inner.clone())
                .with_config(
                    ActorConfig::default().with_supervision(SupervisionConfig::one_for_one()),
                )
                .shutdown(Shutdown::Timeout(INNER_WINDOW))
                .kill_grace(INNER_GRACE)
                .await
                .map(|_| ())
            }
        };
        spawned.map_err(|e| tokio_actors::ActorError::user(e.to_string()))
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// The root supervisor: it spawns the `Mid` under `Shutdown::Kill` and
/// terminates it on request.
struct Top {
    below: Below,
    mid: String,
    inner: String,
    leaf: String,
    probe: Arc<Parked>,
}

impl Actor for Top {
    type Message = ();
    type Response = Result<(), String>;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (below, inner, leaf, probe) = (
            self.below,
            self.inner.clone(),
            self.leaf.clone(),
            self.probe.clone(),
        );
        ctx.spawn_child(move || Mid {
            below,
            inner: inner.clone(),
            leaf: leaf.clone(),
            probe: probe.clone(),
        })
        .named(self.mid.clone())
        .with_config(ActorConfig::default().with_supervision(SupervisionConfig::one_for_one()))
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Kill)
        .await
        .map_err(|e| tokio_actors::ActorError::user(e.to_string()))?;
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: (),
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Result<(), String>> {
        Ok(ctx
            .terminate_child(self.mid.clone())
            .await
            .map_err(|e| e.to_string()))
    }
}

async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + BOUND;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(1)).await;
    }
}

/// What a terminating call across a stall ended with.
struct Terminated {
    /// The call's answer.
    answer: Result<(), String>,
    /// The instant the call returned.
    returned: Instant,
    /// The instant the stall ended.
    stall_end: Instant,
    /// Whether the parked turn was torn down by the time the call returned.
    leaf_ended: bool,
}

/// Builds the tree, parks the leaf, terminates the `Mid` while the thread
/// stalls across the guard's `Kill` and the wait's deadline, and returns
/// what the call ended with.
async fn terminate_across_a_stall(below: Below) -> Terminated {
    let id = uuid::Uuid::new_v4();
    let sys = ActorSystem::create(format!("verdict-floor-{id}")).unwrap();
    let (mid, inner, leaf) = (
        format!("mid-{id}"),
        format!("inner-{id}"),
        format!("leaf-{id}"),
    );
    let probe = Arc::new(Parked::default());
    let top = Top {
        below,
        mid: mid.clone(),
        inner,
        leaf: leaf.clone(),
        probe: probe.clone(),
    }
    .spawn()
    .named(format!("top-{id}"))
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();
    // A child spawn returns before the child's own initialization, so the
    // tree below the `Mid` is still being built when the root's spawn acks.
    let mid_handle = sys.get::<Mid>(&mid).expect("the child supervisor runs");
    let mut leaf_handle = None;
    wait_until("the parked actor to be registered", || {
        leaf_handle = sys.get::<Leaf>(&leaf);
        leaf_handle.is_some()
    })
    .await;
    leaf_handle
        .expect("the parked actor runs")
        .notify(())
        .await
        .unwrap();
    wait_until("the turn to park", || probe.parked.load(Ordering::SeqCst)).await;

    // Stalls the thread once the terminated child has ended and the call
    // waits for its subtree.
    let stall = below.stall();
    let stalled = Arc::new(Mutex::new(None::<(Instant, Instant)>));
    let staller = {
        let stalled = stalled.clone();
        tokio::spawn(async move {
            wait_until("the terminated child to end", || {
                mid_handle.status() == ActorStatus::Stopped
            })
            .await;
            sleep(STALL_AFTER).await;
            let from = Instant::now();
            std::thread::sleep(stall);
            *stalled.lock().unwrap() = Some((from, Instant::now()));
        })
    };

    let answer = tokio::time::timeout(BOUND, top.send(()))
        .await
        .expect("the terminating call returns")
        .expect("the supervisor answers");
    let returned = Instant::now();
    let leaf_ended = probe.ended.load(Ordering::SeqCst);
    staller.await.unwrap();
    let (from, to) = stalled.lock().unwrap().expect("the thread stalled");
    assert!(
        to.duration_since(from) >= stall,
        "the stall lasted {:?}",
        to.duration_since(from)
    );
    Terminated {
        answer,
        returned,
        stall_end: to,
        leaf_ended,
    }
}

/// The stall needs a runtime with one thread: on it, a blocking sleep stops
/// every task, the system's reaper included.
#[test]
fn a_subtree_wait_holds_its_verdict_for_an_abort_a_stall_made_late() {
    common::run_current_thread_only(async {
        let ended = terminate_across_a_stall(Below::ParkedSupervisor).await;
        assert_eq!(
            ended.answer,
            Ok(()),
            "no verdict before the abort that ends the parked actor"
        );
        assert!(ended.leaf_ended, "the abort ended the parked turn");
        // The abort came one kill grace after the late `Kill`, so the call
        // returned no earlier than that.
        assert!(
            ended.returned >= ended.stall_end + GRACE,
            "returned {:?} after the stall",
            ended.returned.saturating_duration_since(ended.stall_end)
        );
    });
}

/// The one worker thread of a multi-thread runtime: a blocking sleep on it
/// stalls every task, the reaper included, and a task that a finishing task
/// wakes is the next one it polls, ahead of the runner that same task
/// cancelled. That order is what puts the wait's verdict before the guard of
/// the ended supervisor's worker, unless the end holds the wait.
#[test]
fn a_subtree_wait_holds_its_verdict_for_the_cascade_of_a_supervisor_that_ended_while_owed() {
    common::run_multi_thread_only(1, async {
        let ended = terminate_across_a_stall(Below::StoppingSupervisor).await;
        assert_eq!(
            ended.answer,
            Ok(()),
            "no verdict between the supervisor's end and its worker's abort"
        );
        assert!(ended.leaf_ended, "the abort ended the parked turn");
        // The worker's abort came one kill grace after its guard fired, which
        // the supervisor's end after the stall did, so the call returned no
        // earlier than that.
        assert!(
            ended.returned >= ended.stall_end + WORKER_GRACE,
            "returned {:?} after the stall",
            ended.returned.saturating_duration_since(ended.stall_end)
        );
    });
}
