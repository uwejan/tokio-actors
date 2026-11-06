#![allow(dead_code)]
//! A child whose first `pre_start` or `on_started` fails ends with `Failure`
//! and is restarted under the supervisor's budget like any crash (a
//! `Temporary` child is removed), while the supervisor's own start succeeds.
//! `spawn_child` returns `Ok` before the child's initialization finishes,
//! after startup as well, and the restart budget is charged.
//!
//! The child's initialization waits at a gate the test opens only after the
//! supervisor's spawn (or the late `spawn_child`) has answered, so an answer
//! that waited for the initialization would never arrive.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorStatus, ActorSystem, ChildEvent, RestartType,
    StopReason, SupervisionAction, SupervisionConfig,
};

/// Bounds every wait, so a hang fails the test.
const BOUND: Duration = Duration::from_secs(20);

/// The hook of the child that fails.
#[derive(Clone, Copy, Debug)]
enum FailsIn {
    PreStart,
    OnStarted,
}

/// A child whose chosen hook waits at the gate and then returns an error.
struct Refuses {
    gate: Arc<Semaphore>,
    fails_in: FailsIn,
}

impl Refuses {
    async fn fail(&self) -> ActorResult<()> {
        self.gate
            .acquire()
            .await
            .expect("the gate stays open")
            .forget();
        Err(ActorError::user("refused"))
    }
}

impl Actor for Refuses {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match self.fails_in {
            FailsIn::PreStart => self.fail().await,
            FailsIn::OnStarted => Ok(()),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match self.fails_in {
            FailsIn::PreStart => Ok(()),
            FailsIn::OnStarted => self.fail().await,
        }
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

enum Cmd {
    /// Spawns the failing child after startup and answers `Spawned(is_ok)`.
    SpawnLate,
    /// Answers `Children(n)`: the specs the supervisor holds.
    Children,
}

#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Spawned(bool),
    Children(usize),
}

/// Supervises one failing child, spawned in `on_started` or on `SpawnLate`,
/// and forwards the events of its children to the test.
struct Boss {
    builds: Arc<AtomicUsize>,
    gate: Arc<Semaphore>,
    fails_in: FailsIn,
    kind: RestartType,
    spawn_at_start: bool,
    events: mpsc::UnboundedSender<ChildEvent>,
    spawn_answers: Option<mpsc::UnboundedSender<bool>>,
}

impl Boss {
    async fn spawn_failing_child(&self, ctx: &mut ActorContext<Self>) -> bool {
        let builds = self.builds.clone();
        let gate = self.gate.clone();
        let fails_in = self.fails_in;
        ctx.spawn_child(move || {
            builds.fetch_add(1, Ordering::SeqCst);
            Refuses {
                gate: gate.clone(),
                fails_in,
            }
        })
        .named("refuses")
        .restart_type(self.kind)
        .await
        .is_ok()
    }
}

impl Actor for Boss {
    type Message = Cmd;
    type Response = Reply;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.spawn_at_start {
            let ok = self.spawn_failing_child(ctx).await;
            if let Some(answers) = &self.spawn_answers {
                let _ = answers.send(ok);
            }
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        Ok(match msg {
            Cmd::SpawnLate => Reply::Spawned(self.spawn_failing_child(ctx).await),
            Cmd::Children => Reply::Children(ctx.children().len()),
        })
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.events.send(event.clone());
        Ok(())
    }
}

struct Rig {
    builds: Arc<AtomicUsize>,
    gate: Arc<Semaphore>,
    events: mpsc::UnboundedReceiver<ChildEvent>,
    boss: ActorHandle<Boss>,
}

impl Rig {
    /// Lets every initialization of the failing child go on and fail.
    fn open_gate(&self) {
        self.gate.add_permits(1 << 16);
    }

    async fn next_event(&mut self) -> ChildEvent {
        timeout(BOUND, self.events.recv())
            .await
            .expect("an event arrives")
            .expect("the supervisor is alive")
    }
}

/// Starts a supervisor that allows two restarts in a minute and, with
/// `spawn_at_start`, spawns the failing child in its `on_started`. The gate
/// is closed when this returns: the supervisor's start has been answered
/// while the child's first initialization has not finished.
async fn start(kind: RestartType, fails_in: FailsIn, spawn_at_start: bool) -> Rig {
    let sys = ActorSystem::create(format!("first-init-{}", unique_number())).unwrap();
    let builds = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Semaphore::new(0));
    let (events_tx, events) = mpsc::unbounded_channel();
    let (answers_tx, mut answers) = mpsc::unbounded_channel();
    let boss = timeout(
        BOUND,
        Boss {
            builds: builds.clone(),
            gate: gate.clone(),
            fails_in,
            kind,
            spawn_at_start,
            events: events_tx,
            spawn_answers: Some(answers_tx),
        }
        .spawn()
        .on_system(&sys)
        .with_supervision(
            SupervisionConfig::one_for_one().max_restarts(2, Duration::from_secs(60)),
        ),
    )
    .await
    .expect("the supervisor's start does not wait for its child's initialization")
    .expect("the supervisor's own start succeeds");
    if spawn_at_start {
        let ok = timeout(BOUND, answers.recv())
            .await
            .expect("the spawn answered")
            .expect("the supervisor reported");
        assert!(
            ok,
            "spawn_child returned Ok before the child's initialization finished"
        );
    }
    Rig {
        builds,
        gate,
        events,
        boss,
    }
}

fn unique_number() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn is_failure(reason: &StopReason) -> bool {
    matches!(reason, StopReason::Failure(_))
}

/// Opens the gate and follows the failing child until the budget is spent:
/// each failure but the last is restarted, and the supervisor then ends.
async fn expect_restarts_then_exhaustion(mut rig: Rig) {
    rig.open_gate();
    for attempt in 1..=2 {
        let event = rig.next_event().await;
        assert!(
            is_failure(&event.reason),
            "attempt {attempt}: {:?}",
            event.reason
        );
        assert_eq!(
            event.action,
            SupervisionAction::RestartInitiated,
            "attempt {attempt} is restarted under the budget"
        );
    }
    let last = rig.next_event().await;
    assert!(is_failure(&last.reason), "{:?}", last.reason);
    assert_eq!(
        last.action,
        SupervisionAction::BudgetExhausted,
        "the third failure meets the spent budget"
    );
    assert_eq!(
        timeout(BOUND, rig.boss.wait_stopped())
            .await
            .expect("the supervisor ends"),
        StopReason::ParentRequest
    );
    assert_eq!(
        rig.builds.load(Ordering::SeqCst),
        3,
        "the first incarnation and two restarts"
    );
}

async fn expect_removal(mut rig: Rig) {
    rig.open_gate();
    let event = rig.next_event().await;
    assert!(is_failure(&event.reason), "{:?}", event.reason);
    assert_eq!(event.action, SupervisionAction::Removed);
    assert_eq!(
        rig.builds.load(Ordering::SeqCst),
        1,
        "a Temporary child is not restarted"
    );
    assert_eq!(
        timeout(BOUND, rig.boss.send(Cmd::Children))
            .await
            .unwrap()
            .unwrap(),
        Reply::Children(0),
        "its spec is gone"
    );
    assert_eq!(
        rig.boss.status(),
        ActorStatus::Running,
        "the supervisor goes on"
    );
}

macro_rules! first_init_cases {
    ($($name:ident: $kind:expr, $fails_in:expr, $outcome:ident;)*) => {$(
        #[test]
        fn $name() {
            common::run(2, async {
                $outcome(start($kind, $fails_in, true).await).await;
            });
        }
    )*};
}

first_init_cases! {
    a_permanent_child_failing_in_on_started_is_restarted_until_the_budget_ends:
        RestartType::Permanent, FailsIn::OnStarted, expect_restarts_then_exhaustion;
    a_temporary_child_failing_in_on_started_is_removed:
        RestartType::Temporary, FailsIn::OnStarted, expect_removal;
}
