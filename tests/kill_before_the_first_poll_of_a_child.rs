//! A `Kill` that reaches a supervised child before its task was first polled
//! ends that incarnation without running either start hook, whichever path
//! made the incarnation: the initial `spawn_child` (which is not awaited), a
//! restart under the restart type, or a manual `restart_child`. The supervisor
//! then handles the end like any other `Kill`: the restart type decides, the
//! restart budget is charged and a retry follows. A counter in `pre_start`
//! therefore reads fewer runs than the factory built instances, so the number
//! of an incarnation comes from the factory.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorStatus, ActorStatusInfo, ActorSystem, ChildEvent, RestartType,
    StopReason, SupervisionAction, SupervisionConfig,
};

mod common;

const BOUND: Duration = Duration::from_secs(20);
const START_TIMEOUT: Duration = Duration::from_millis(50);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "kill-first-poll-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// What the incarnations of one child did, shared with the test body.
#[derive(Default)]
struct Trace {
    /// The factory's number of each incarnation whose `pre_start` began, in
    /// the order they began.
    began: Mutex<Vec<u64>>,
    /// The number of incarnations the factory built.
    built: AtomicU64,
    /// The reason and the action of each end the supervisor was told about.
    stopped: Mutex<Vec<(StopReason, SupervisionAction)>>,
    /// Signalled for each entry added to `stopped`.
    stop_seen: Notify,
    /// Signalled when an incarnation other than the first completed its start.
    restarted: Notify,
}

impl Trace {
    fn began(&self) -> Vec<u64> {
        self.began.lock().unwrap().clone()
    }

    fn built(&self) -> u64 {
        self.built.load(Ordering::SeqCst)
    }

    fn stopped(&self) -> Vec<(StopReason, SupervisionAction)> {
        self.stopped.lock().unwrap().clone()
    }

    /// Waits until the supervisor was told of at least `count` ends and
    /// returns all it was told of.
    async fn stopped_events(&self, count: usize) -> Vec<(StopReason, SupervisionAction)> {
        timeout(BOUND, async {
            loop {
                let seen = self.stopped();
                if seen.len() >= count {
                    return seen;
                }
                self.stop_seen.notified().await;
            }
        })
        .await
        .expect("the supervisor is told of the child's end")
    }

    /// Waits until an incarnation other than the first completed its start.
    async fn restarted(&self) {
        timeout(BOUND, self.restarted.notified())
            .await
            .expect("the child is restarted and starts");
    }
}

/// The child. Its number comes from the factory, so it does not depend on
/// whether an earlier `pre_start` ran.
struct Numbered {
    number: u64,
    /// Every incarnation but the first parks in `pre_start`, so only the
    /// child's start timeout ends it.
    park_restarts: bool,
    trace: Arc<Trace>,
}

impl Actor for Numbered {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.trace.began.lock().unwrap().push(self.number);
        if self.park_restarts && self.number > 0 {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.number > 0 {
            self.trace.restarted.notify_one();
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// What the supervisor does with the child.
#[derive(Clone)]
struct Plan {
    restart_type: RestartType,
    /// See `Numbered::park_restarts`.
    park_restarts: bool,
    /// The supervisor kills the first incarnation from its own `on_started`,
    /// right after spawning it, so on a single thread the `Kill` is stored
    /// before the incarnation's task is first polled.
    kill_at_once: bool,
    /// Incarnations whose `Kill` is queued by the factory: it spawns a task
    /// that kills the child by name, and the incarnation is spawned right
    /// after, so on a single thread that task runs before the incarnation's
    /// task is first polled.
    kill_before_first_poll: Vec<u64>,
    max_restarts: u32,
}

impl Plan {
    fn new(restart_type: RestartType) -> Self {
        Self {
            restart_type,
            park_restarts: false,
            kill_at_once: false,
            kill_before_first_poll: Vec::new(),
            max_restarts: 2,
        }
    }
}

#[derive(Clone, Copy)]
enum Cmd {
    Terminate,
    Restart,
}

/// Spawns the child from `on_started` without awaiting its start.
struct Parent {
    system: Arc<ActorSystem>,
    child_name: String,
    plan: Plan,
    trace: Arc<Trace>,
}

impl Actor for Parent {
    type Message = Cmd;
    type Response = Result<(), String>;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let trace = Arc::clone(&self.trace);
        let system = Arc::clone(&self.system);
        let child_name = self.child_name.clone();
        let plan = self.plan.clone();
        let mut child = ctx
            .spawn_child(move || {
                let number = trace.built.fetch_add(1, Ordering::SeqCst);
                if plan.kill_before_first_poll.contains(&number) {
                    let (system, child_name) = (Arc::clone(&system), child_name.clone());
                    tokio::spawn(async move {
                        let _ = system.kill(&child_name).await;
                    });
                }
                Numbered {
                    number,
                    park_restarts: plan.park_restarts,
                    trace: Arc::clone(&trace),
                }
            })
            .named(self.child_name.clone())
            .restart_type(self.plan.restart_type);
        if self.plan.park_restarts {
            // Only a parked restart needs the bound; a restart that starts is
            // never cut by it.
            child = child.start_timeout(START_TIMEOUT);
        }
        child.await.expect("the child is spawned");
        if self.plan.kill_at_once {
            self.system
                .kill(&self.child_name)
                .await
                .expect("the Kill is accepted");
        }
        Ok(())
    }

    async fn handle(
        &mut self,
        cmd: Cmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Result<(), String>> {
        Ok(match cmd {
            Cmd::Terminate => ctx
                .terminate_child(self.child_name.clone())
                .await
                .map_err(|e| e.to_string()),
            Cmd::Restart => ctx
                .restart_child(self.child_name.clone())
                .map_err(|e| e.to_string()),
        })
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.trace
            .stopped
            .lock()
            .unwrap()
            .push((event.reason.clone(), event.action));
        self.trace.stop_seen.notify_one();
        Ok(())
    }
}

/// A running supervisor with its child.
struct Run {
    system: Arc<ActorSystem>,
    parent: ActorHandle<Parent>,
    child_name: String,
    trace: Arc<Trace>,
}

async fn start(plan: Plan) -> Run {
    let system = ActorSystem::create(uname("system")).expect("create system");
    let trace = Arc::new(Trace::default());
    let child_name = uname("child");
    let supervision =
        SupervisionConfig::one_for_one().max_restarts(plan.max_restarts, Duration::from_secs(10));
    let parent = Parent {
        system: Arc::clone(&system),
        child_name: child_name.clone(),
        plan,
        trace: Arc::clone(&trace),
    }
    .spawn()
    .named(uname("parent"))
    .on_system(&system)
    .with_supervision(supervision)
    .await
    .expect("spawn parent");
    Run {
        system,
        parent,
        child_name,
        trace,
    }
}

impl Run {
    /// The child's current incarnation.
    fn child(&self) -> ActorHandle<Numbered> {
        self.system
            .get::<Numbered>(&self.child_name)
            .expect("the child is registered")
    }

    /// Waits for the child's current incarnation to answer `get_status`, which
    /// it does only after its `pre_start` and `on_started` returned.
    async fn status(&self) -> ActorStatusInfo {
        timeout(BOUND, self.child().get_status())
            .await
            .expect("the child answers")
            .expect("the child has a status")
    }

    /// Kills the child's current incarnation from outside.
    async fn kill_child(&self) -> ActorHandle<Numbered> {
        let child = self.child();
        child
            .stop(StopReason::Kill)
            .await
            .expect("the Kill is accepted");
        child
    }

    async fn parent_ends(&self) {
        timeout(BOUND, self.parent.wait_stopped())
            .await
            .expect("the supervisor ends");
    }

    async fn stop_parent(&self) {
        self.parent
            .stop(StopReason::Graceful)
            .await
            .expect("the stop is accepted");
        self.parent_ends().await;
    }
}

/// The scenario every restart type shares: the child is killed before its
/// first poll, and its restarts, if any, park in `pre_start`.
fn killed_at_once(restart_type: RestartType) -> Plan {
    Plan {
        park_restarts: true,
        kill_at_once: true,
        ..Plan::new(restart_type)
    }
}

/// Runs a child whose restarts park until the restart budget ends the
/// supervisor.
async fn killed_at_once_until_the_budget_ends(restart_type: RestartType) -> Run {
    let run = start(killed_at_once(restart_type)).await;
    run.parent_ends().await;
    run
}

/// On a single thread nothing runs between the child's spawn and the `Kill`,
/// so the `Kill` is stored before the child's first poll: that incarnation
/// never begins `pre_start`, it ends with `Kill`, and a `Permanent` child is
/// restarted. Every restart parks in `pre_start` and is cut by the start
/// timeout, so the restart budget runs out and the supervisor ends.
#[test]
fn a_child_killed_before_its_first_poll_skips_pre_start_and_is_restarted() {
    common::run_current_thread_only(async {
        let run = killed_at_once_until_the_budget_ends(RestartType::Permanent).await;
        let (began, built, stopped) = (run.trace.began(), run.trace.built(), run.trace.stopped());
        assert!(
            !began.contains(&0),
            "the first incarnation was killed before its first poll, so its pre_start never began: {began:?}"
        );
        assert!(
            built >= 2,
            "the child was restarted after the Kill: {built} incarnations built"
        );
        assert!(
            began.iter().all(|n| *n > 0),
            "only restarts began pre_start: {began:?}"
        );
        assert!(
            (began.len() as u64) < built,
            "a counter in pre_start reads fewer runs than the factory built instances: \
             {} runs, {built} built",
            began.len()
        );
        assert_eq!(
            stopped.first(),
            Some(&(StopReason::Kill, SupervisionAction::RestartInitiated)),
            "the first end is a Kill and starts a restart: {stopped:?}"
        );
        assert_eq!(
            stopped.last().map(|(_, action)| *action),
            Some(SupervisionAction::BudgetExhausted),
            "the parked restarts exhaust the budget: {stopped:?}"
        );
    });
}

/// A `Kill` is an abnormal end, so a `Transient` child killed before its first
/// poll is restarted like after any other `Kill`, and the supervisor is told
/// the reason is `Kill`.
#[test]
fn a_transient_child_killed_before_its_first_poll_is_restarted() {
    common::run_current_thread_only(async {
        let run = killed_at_once_until_the_budget_ends(RestartType::Transient).await;
        let (began, built, stopped) = (run.trace.began(), run.trace.built(), run.trace.stopped());
        assert!(
            !began.contains(&0),
            "the first incarnation never began pre_start: {began:?}"
        );
        assert!(
            built >= 2,
            "a Kill is not a normal end, so the child was restarted: {built} built"
        );
        assert_eq!(
            stopped.first(),
            Some(&(StopReason::Kill, SupervisionAction::RestartInitiated)),
            "the first end is a Kill and starts a restart: {stopped:?}"
        );
    });
}

/// A `Temporary` child is never restarted: killed before its first poll it is
/// removed, its `pre_start` never ran and the supervisor goes on.
#[test]
fn a_temporary_child_killed_before_its_first_poll_is_removed() {
    common::run_current_thread_only(async {
        let run = start(killed_at_once(RestartType::Temporary)).await;
        let stopped = run.trace.stopped_events(1).await;
        assert_eq!(
            stopped,
            vec![(StopReason::Kill, SupervisionAction::Removed)],
            "the only end is the Kill, answered with a removal"
        );
        assert_eq!(run.trace.built(), 1, "no second incarnation is built");
        assert!(
            run.trace.began().is_empty(),
            "the child's pre_start never began: {:?}",
            run.trace.began()
        );
        assert!(
            run.system.get::<Numbered>(&run.child_name).is_none(),
            "the removed child leaves no registration"
        );
        run.stop_parent().await;
    });
}

/// A restart killed before its first poll is handled like any failed restart:
/// the end is charged to the budget and the child is restarted again. The
/// factory queues the `Kill` of incarnation 1 ahead of that incarnation's task,
/// so it ends before `pre_start`; incarnation 2 starts, and a third `Kill` is
/// the one that exhausts a budget of two restarts.
#[test]
fn a_restart_killed_before_its_first_poll_is_charged_and_retried() {
    common::run_current_thread_only(async {
        let plan = Plan {
            kill_before_first_poll: vec![1],
            ..Plan::new(RestartType::Permanent)
        };
        let run = start(plan).await;
        run.status().await;
        run.kill_child().await;
        run.trace.restarted().await;

        assert_eq!(
            run.trace.began(),
            vec![0, 2],
            "incarnation 1 was killed before its first poll and never began pre_start"
        );
        assert_eq!(
            run.trace.built(),
            3,
            "the factory built all three incarnations"
        );

        run.kill_child().await;
        run.parent_ends().await;
        assert_eq!(
            run.trace.stopped(),
            vec![
                (StopReason::Kill, SupervisionAction::RestartInitiated),
                (StopReason::Kill, SupervisionAction::RestartInitiated),
                (StopReason::Kill, SupervisionAction::BudgetExhausted),
            ],
            "each Kill is charged, the one killed before its first poll included"
        );
    });
}

/// A manual restart returns before the incarnation is polled, so a `Kill`
/// queued by the factory ends it before `pre_start`. The failed start is
/// reported, the spec is kept and the next manual restart starts the child.
#[test]
fn a_manual_restart_killed_before_its_first_poll_is_reported_and_keeps_the_spec() {
    common::run_current_thread_only(async {
        let plan = Plan {
            kill_before_first_poll: vec![1],
            ..Plan::new(RestartType::Permanent)
        };
        let run = start(plan).await;
        run.status().await;

        run.parent
            .send(Cmd::Terminate)
            .await
            .expect("the supervisor answers")
            .expect("terminate_child succeeds");
        let stopped = run.trace.stopped_events(1).await;
        assert_eq!(
            stopped[0],
            (StopReason::ParentRequest, SupervisionAction::Removed)
        );

        run.parent
            .send(Cmd::Restart)
            .await
            .expect("the supervisor answers")
            .expect("restart_child returns before the child starts");
        let stopped = run.trace.stopped_events(2).await;
        assert_eq!(
            stopped[1],
            (StopReason::Kill, SupervisionAction::Removed),
            "the killed start is reported: {stopped:?}"
        );
        assert_eq!(
            run.trace.began(),
            vec![0],
            "incarnation 1 never began pre_start"
        );

        run.parent
            .send(Cmd::Restart)
            .await
            .expect("the supervisor answers")
            .expect("the spec is kept, so the child can be restarted");
        run.trace.restarted().await;
        assert_eq!(run.trace.began(), vec![0, 2]);
        assert_eq!(run.trace.built(), 3);
        run.stop_parent().await;
    });
}

/// A child that answered `get_status` has run `pre_start` and `on_started`, so
/// a `Kill` sent after the answer never precedes its first `pre_start`.
#[test]
fn a_kill_after_the_status_answer_never_precedes_the_first_pre_start() {
    common::run(0, async {
        let run = start(Plan::new(RestartType::Permanent)).await;
        let status = run.status().await;
        assert_eq!(status.status, ActorStatus::Running);
        assert_eq!(
            run.trace.began(),
            vec![0],
            "pre_start began before the answer"
        );

        run.kill_child().await;
        run.trace.restarted().await;
        assert_eq!(run.trace.began(), vec![0, 1]);
        assert_eq!(run.trace.built(), 2);
        run.stop_parent().await;
    });
}

/// The other ordering, on a single thread: a `Kill` sent after the status
/// answer finds the first incarnation started, so its `pre_start` ran, and
/// the restarts that park in `pre_start` begin it too until the start timeout
/// cuts them. Every incarnation the factory built began `pre_start`.
#[test]
fn a_kill_after_the_status_answer_leaves_the_pre_start_count_equal_to_the_factory_count() {
    common::run_current_thread_only(async {
        let plan = Plan {
            park_restarts: true,
            ..Plan::new(RestartType::Permanent)
        };
        let run = start(plan).await;
        run.status().await;
        run.kill_child().await;
        run.parent_ends().await;

        let (began, built) = (run.trace.began(), run.trace.built());
        assert!(built >= 2, "the child was restarted: {built} built");
        assert_eq!(
            began,
            (0..built).collect::<Vec<_>>(),
            "pre_start counted every incarnation the factory built"
        );
    });
}

/// With real parallelism the `Kill` lands on either side of the child's first
/// poll. The outcome does not depend on the side: the incarnation ends, the
/// child is restarted, a parked restart is cut by the start timeout and the
/// budget ends the supervisor.
#[test]
fn a_kill_racing_the_first_poll_of_a_child_is_never_lost() {
    common::run_multi_thread_only(4, async {
        for _ in 0..8 {
            let plan = Plan {
                park_restarts: true,
                ..Plan::new(RestartType::Permanent)
            };
            let run = start(plan).await;
            let child = run.kill_child().await;
            timeout(BOUND, child.wait_stopped())
                .await
                .expect("the killed incarnation ends");
            run.parent_ends().await;
            assert!(
                run.trace.built() >= 2,
                "the child was restarted: {} built",
                run.trace.built()
            );
        }
    });
}

/// Whichever side of the first poll the `Kill` lands on, the factory counts
/// every incarnation, while `pre_start` counts one fewer exactly when the first
/// incarnation was killed before its first poll. The restarted incarnation is
/// never killed, so it always begins.
#[test]
fn the_factory_counts_every_incarnation_whichever_side_of_the_first_poll_the_kill_lands() {
    common::run_multi_thread_only(4, async {
        for _ in 0..40 {
            let run = start(Plan::new(RestartType::Permanent)).await;
            run.kill_child().await;
            run.trace.restarted().await;
            run.stop_parent().await;

            let (began, built) = (run.trace.began(), run.trace.built());
            assert_eq!(built, 2, "the factory built both incarnations");
            assert_eq!(
                began.last(),
                Some(&1),
                "the restart begins pre_start: {began:?}"
            );
            let runs_expected = if began.contains(&0) { built } else { built - 1 };
            assert_eq!(
                began.len() as u64,
                runs_expected,
                "pre_start counted {began:?} of {built} incarnations"
            );
        }
    });
}
