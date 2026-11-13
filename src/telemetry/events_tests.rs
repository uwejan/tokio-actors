//! The events of the supervision and lifecycle sites: the budget charge, the
//! death of a child with the decision taken, the verdict, the end of every
//! incarnation and the stop requests of callers. Each scenario reads the
//! events through a collecting sink on the system's mode, on both runtime
//! flavors.

use std::future::{Future, IntoFuture};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::time::{sleep, Instant};

use super::{ActorRef, EventKind, KindSet, RaiseOutcome, TelemetryEvent, TestSinkGuard};
use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::subtree::AbortRecord;
use crate::actor::supervision::slot::{await_cell, CellOutcome, Verdicts};
use crate::actor::supervision::OwnStop;
use crate::actor::supervision::{slot_pair_for, SupervisionConfig};
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult, SpawnError};
use crate::system::{read_status, ActorSystem};
use crate::types::StopLane;
use crate::types::{ActorId, ActorStatus, RestartType, Shutdown, StopReason, SupervisionAction};

static SEQ: AtomicU64 = AtomicU64::new(0);

type Events = Vec<TelemetryEvent<'static>>;

/// A stop lane whose record names `system`, published into the slot of `io`
/// as its incarnation's lane, so a verdict written through the slot finds
/// the system. The plane's sender and the lane's receiver stay with the
/// returned guards.
fn publish_lane(
    io: &crate::actor::supervision::SlotIo,
    system: &Arc<ActorSystem>,
) -> (AbortRecord, Box<dyn std::any::Any>) {
    let (lane, rx) = StopLane::new();
    lane.abort_record().set_system(system);
    let (tx, status) = lane.abort_record().status_channel();
    io.publish(lane.clone(), status, OwnStop::default(), None);
    (lane.abort_record().clone(), Box::new((tx, rx)))
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A system of its own with a collecting sink on its mode.
struct Rig {
    system: Arc<ActorSystem>,
    sink: TestSinkGuard,
    hold: Hold,
}

/// Parks the turn of a supervisor: `entered` is set when the turn begins and
/// the turn ends when `release` is notified.
#[derive(Clone, Default)]
struct Hold {
    entered: Arc<AtomicBool>,
    release: Arc<tokio::sync::Notify>,
}

impl Rig {
    fn new(tag: &str, kinds: KindSet) -> Rig {
        let name = format!("evt-{tag}-{}", SEQ.fetch_add(1, Ordering::Relaxed));
        let system = ActorSystem::create(name).expect("a fresh system name");
        let sink = system.telemetry_mode().collect(kinds);
        Rig {
            system,
            sink,
            hold: Hold::default(),
        }
    }

    fn events(&self) -> Events {
        self.sink.events()
    }

    /// Waits until `pred` holds over the events, up to ten seconds.
    async fn until(&self, what: &str, pred: impl Fn(&Events) -> bool) -> Events {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let events = self.events();
            if pred(&events) {
                return events;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; events so far: {events:#?}"
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    /// The events once they have stopped coming: waits for `pred`, then a
    /// settle time, and reads again.
    async fn settled(&self, what: &str, pred: impl Fn(&Events) -> bool) -> Events {
        self.until(what, pred).await;
        sleep(Duration::from_millis(150)).await;
        self.events()
    }
}

/// What a probe does when told to.
#[derive(Clone)]
enum Cmd {
    Ping,
    Fail,
    Panic,
    Park,
}

/// A worker that fails, panics or parks on command, and can refuse or park
/// its start.
#[derive(Default)]
struct Probe {
    fail_start: bool,
    park_start: bool,
    entered: Arc<AtomicBool>,
}

impl Actor for Probe {
    type Message = Cmd;
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.fail_start {
            return Err(ActorError::user("the start is refused"));
        }
        if self.park_start {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Cmd, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Cmd::Ping => Ok(()),
            Cmd::Fail => Err(ActorError::user("failed on command")),
            Cmd::Panic => panic!("panicked on command"),
            Cmd::Park => {
                self.entered.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok(())
            }
        }
    }
}

/// One child a supervisor starts.
#[derive(Clone)]
struct Kid {
    name: &'static str,
    restart: RestartType,
    /// Counts the instances the factory built; every instance after the
    /// first refuses its start.
    flaky: Option<Arc<AtomicUsize>>,
}

impl Kid {
    fn new(name: &'static str) -> Kid {
        Kid {
            name,
            restart: RestartType::Permanent,
            flaky: None,
        }
    }
}

/// A supervisor that starts its children when it starts.
struct Sup {
    kids: Vec<Kid>,
    hold: Hold,
}

impl Actor for Sup {
    /// Holds the turn that handles it until the rig's hold is released.
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for kid in self.kids.clone() {
            let flaky = kid.flaky.clone();
            ctx.spawn_child(move || Probe {
                fail_start: flaky
                    .as_ref()
                    .is_some_and(|built| built.fetch_add(1, Ordering::SeqCst) >= 1),
                ..Probe::default()
            })
            .named(kid.name)
            .restart_type(kid.restart)
            .await?;
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.hold.entered.store(true, Ordering::SeqCst);
        self.hold.release.notified().await;
        Ok(())
    }
}

async fn spawn_sup(
    rig: &Rig,
    name: &str,
    config: SupervisionConfig,
    kids: Vec<Kid>,
) -> ActorHandle<Sup> {
    Sup {
        kids,
        hold: rig.hold.clone(),
    }
    .spawn()
    .named(name)
    .on_system(&rig.system)
    .with_supervision(config)
    .await
    .expect("the supervisor starts")
}

/// A running worker of this name, once the supervisor has (re)started it.
async fn worker(rig: &Rig, name: &str) -> ActorHandle<Probe> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(handle) = rig.system.get::<Probe>(name) {
            if handle.send(Cmd::Ping).await.is_ok() {
                return handle;
            }
        }
        assert!(Instant::now() < deadline, "no running worker named {name}");
        sleep(Duration::from_millis(10)).await;
    }
}

type End<'e> = (&'e ActorRef, &'e StopReason, Option<Duration>);

fn ended(events: &Events) -> Vec<End<'_>> {
    events
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::ActorEnded {
                actor,
                reason,
                lifetime,
            } => Some((actor, reason, *lifetime)),
            _ => None,
        })
        .collect()
}

fn ended_of<'e>(events: &'e Events, id: &str) -> Vec<End<'e>> {
    ended(events)
        .into_iter()
        .filter(|(actor, _, _)| actor.id().as_str() == id)
        .collect()
}

fn requests(events: &Events) -> Vec<(&ActorRef, &StopReason, RaiseOutcome)> {
    events
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::StopRequested {
                actor,
                reason,
                outcome,
                ..
            } => Some((actor, reason, *outcome)),
            _ => None,
        })
        .collect()
}

/// `(accepted, used, max)` of each budget event, in order.
fn budgets(events: &Events) -> Vec<(bool, u32, u32)> {
    events
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::RestartBudget {
                accepted,
                used,
                max,
                ..
            } => Some((*accepted, *used, *max)),
            _ => None,
        })
        .collect()
}

fn deaths(events: &Events) -> Vec<(&ActorRef, &ActorRef, SupervisionAction)> {
    events
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::ChildDied {
                supervisor,
                child,
                decision,
                ..
            } => Some((supervisor, child, *decision)),
            _ => None,
        })
        .collect()
}

/// The kinds of the budget and death events, in order: `B` for a budget
/// event and `D` for a death.
fn shape(events: &Events) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::RestartBudget { .. } => Some('B'),
            TelemetryEvent::ChildDied { .. } => Some('D'),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The end of every incarnation: one event, whichever path ends it
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum EndPath {
    Orderly,
    Failure,
    Panic,
    Kill,
    Abort,
    StartFailure,
    StartTimeout,
}

impl EndPath {
    const ALL: [EndPath; 7] = [
        EndPath::Orderly,
        EndPath::Failure,
        EndPath::Panic,
        EndPath::Kill,
        EndPath::Abort,
        EndPath::StartFailure,
        EndPath::StartTimeout,
    ];

    fn reason_fits(self, reason: &StopReason) -> bool {
        match self {
            EndPath::Orderly => matches!(reason, StopReason::Graceful),
            EndPath::Failure | EndPath::StartFailure => {
                matches!(reason, StopReason::Failure(ActorError::User(_)))
            }
            EndPath::Panic => matches!(reason, StopReason::Failure(ActorError::Panic(_))),
            EndPath::Kill | EndPath::Abort | EndPath::StartTimeout => {
                matches!(reason, StopReason::Kill)
            }
        }
    }
}

async fn run_end_path(path: EndPath) {
    let rig = Rig::new("end", KindSet::of(&[EventKind::ActorEnded]));
    let name = format!("{path:?}");
    match path {
        EndPath::Orderly | EndPath::Failure | EndPath::Panic | EndPath::Kill => {
            let handle = Probe::default()
                .spawn()
                .named(&name)
                .on_system(&rig.system)
                .await
                .expect("the probe starts");
            match path {
                EndPath::Orderly => handle.stop(StopReason::Graceful).await.unwrap(),
                EndPath::Failure => handle.notify(Cmd::Fail).await.unwrap(),
                EndPath::Panic => handle.notify(Cmd::Panic).await.unwrap(),
                _ => handle.stop(StopReason::Kill).await.unwrap(),
            }
        }
        EndPath::Abort => {
            let entered = Arc::new(AtomicBool::new(false));
            let handle = Probe {
                entered: entered.clone(),
                ..Probe::default()
            }
            .spawn()
            .named(&name)
            .on_system(&rig.system)
            .kill_grace(Duration::from_millis(50))
            .await
            .expect("the probe starts");
            handle.notify(Cmd::Park).await.unwrap();
            while !entered.load(Ordering::SeqCst) {
                sleep(Duration::from_millis(5)).await;
            }
            handle.stop(StopReason::Kill).await.unwrap();
        }
        EndPath::StartFailure => {
            let started = Probe {
                fail_start: true,
                ..Probe::default()
            }
            .spawn()
            .named(&name)
            .on_system(&rig.system)
            .await;
            assert!(started.is_err(), "a refused start fails the spawn");
        }
        EndPath::StartTimeout => {
            let started = Probe {
                park_start: true,
                ..Probe::default()
            }
            .spawn()
            .named(&name)
            .on_system(&rig.system)
            .start_timeout(Duration::from_millis(100))
            .await;
            assert!(
                matches!(started, Err(SpawnError::StartTimeout)),
                "a start past its bound fails the spawn"
            );
        }
    }
    let events = rig
        .settled(&format!("the end of {path:?}"), |events| {
            !ended(events).is_empty()
        })
        .await;
    let ends = ended(&events);
    assert_eq!(
        ends.len(),
        1,
        "{path:?}: exactly one end event: {events:#?}"
    );
    let (actor, reason, lifetime) = ends[0];
    assert_eq!(actor.id().as_str(), name, "{path:?}: the incarnation's id");
    assert!(
        path.reason_fits(reason),
        "{path:?}: the end carries the reason the actor ended with, got {reason:?}"
    );
    assert!(
        lifetime.is_some(),
        "{path:?}: armed at spawn, so a lifetime"
    );
    assert_eq!(
        actor.ancestors().count(),
        0,
        "{path:?}: a root has no ancestry"
    );
}

/// A runtime that ends while an actor runs: an actor that ended before it has
/// its one event, and the one the runtime cut off has at most one, `Kill`.
fn a_runtime_end_adds_no_second_event(multi_thread: bool) {
    let rt = if multi_thread {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    };
    let rig = rt.block_on(async {
        let rig = Rig::new("rt-end", KindSet::of(&[EventKind::ActorEnded]));
        let done = Probe::default()
            .spawn()
            .named("done")
            .on_system(&rig.system)
            .await
            .unwrap();
        done.stop(StopReason::Graceful).await.unwrap();
        rig.until("the first actor's end", |events| {
            !ended_of(events, "done").is_empty()
        })
        .await;
        Probe::default()
            .spawn()
            .named("cut")
            .on_system(&rig.system)
            .await
            .unwrap();
        rig
    });
    drop(rt);
    let events = rig.events();
    let done = ended_of(&events, "done");
    assert_eq!(done.len(), 1, "the actor that ended in time: {events:#?}");
    assert!(matches!(done[0].1, StopReason::Graceful));
    let cut = ended_of(&events, "cut");
    assert!(
        cut.len() <= 1,
        "no second event for a cut task: {events:#?}"
    );
    if let Some((_, reason, _)) = cut.first() {
        assert!(matches!(reason, StopReason::Kill), "{reason:?}");
    }
}

/// The lifetime is recorded only for an actor spawned while `ActorEnded` was
/// armed, and an actor spawned while no sink took events raises no end event.
async fn lifetime_follows_the_gate_at_spawn() {
    let name = format!("evt-life-{}", SEQ.fetch_add(1, Ordering::Relaxed));
    let system = ActorSystem::create(name).unwrap();
    let quiet = Probe::default()
        .spawn()
        .named("quiet")
        .on_system(&system)
        .await
        .unwrap();
    let other = system
        .telemetry_mode()
        .collect(KindSet::of(&[EventKind::HandlerDetached]));
    let before = Probe::default()
        .spawn()
        .named("before")
        .on_system(&system)
        .await
        .unwrap();
    let sink = system
        .telemetry_mode()
        .collect(KindSet::of(&[EventKind::ActorEnded]));
    let after = Probe::default()
        .spawn()
        .named("after")
        .on_system(&system)
        .await
        .unwrap();
    quiet.stop(StopReason::Graceful).await.unwrap();
    before.stop(StopReason::Graceful).await.unwrap();
    after.stop(StopReason::Graceful).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while sink.events().len() < 3 {
        assert!(Instant::now() < deadline, "the three ends arrive");
        sleep(Duration::from_millis(10)).await;
    }
    sleep(Duration::from_millis(150)).await;
    let events = sink.events();
    assert_eq!(events.len(), 3, "one end for each: {events:#?}");
    assert_eq!(
        ended_of(&events, "before")[0].2,
        None,
        "armed after the spawn"
    );
    assert!(
        ended_of(&events, "after")[0].2.is_some(),
        "armed at the spawn"
    );
    assert_eq!(
        ended_of(&events, "quiet")[0].2,
        None,
        "spawned with no sink: identified from the registry at its end, with no birth time"
    );
    drop(other);
}

// ---------------------------------------------------------------------------
// The budget and the death of a child
// ---------------------------------------------------------------------------

/// A child that crashes past its supervisor's budget: each death is preceded
/// by the budget event of its charge, the decision follows the charge, the
/// third death is refused beside `BudgetExhausted`, and the supervisor ends.
async fn exhaustion_beside_its_death() {
    let rig = Rig::new("budget", KindSet::all());
    let sup = spawn_sup(
        &rig,
        "sup",
        SupervisionConfig::one_for_one().max_restarts(2, Duration::from_secs(60)),
        vec![Kid::new("c")],
    )
    .await;
    for round in 1..=3usize {
        let handle = worker(&rig, "c").await;
        handle.notify(Cmd::Panic).await.unwrap();
        rig.until(&format!("death {round}"), |events| {
            deaths(events).len() >= round
        })
        .await;
    }
    let events = rig
        .settled("the supervisor's end", |events| {
            !ended_of(events, "sup").is_empty()
        })
        .await;

    assert_eq!(
        budgets(&events),
        vec![(true, 1, 2), (true, 2, 2), (false, 2, 2)],
        "the charge as the supervisor made it: used counts the window after the charge, and a refusal reads the maximum"
    );
    assert_eq!(
        shape(&events),
        "BDBDBD",
        "each budget event precedes its death"
    );
    let decisions: Vec<_> = deaths(&events).iter().map(|d| d.2).collect();
    assert_eq!(
        decisions,
        vec![
            SupervisionAction::RestartInitiated,
            SupervisionAction::RestartInitiated,
            SupervisionAction::BudgetExhausted
        ],
        "the decision is derived from the charge"
    );

    let sup_ref = ended_of(&events, "sup")[0].0.clone();
    assert_eq!(sup_ref.id().as_str(), sup.id().as_str());
    assert!(matches!(
        ended_of(&events, "sup")[0].1,
        StopReason::ParentRequest
    ));
    let child_ends = ended_of(&events, "c");
    assert_eq!(child_ends.len(), 3, "one end per incarnation: {events:#?}");
    let incarnations: Vec<u64> = child_ends.iter().map(|e| e.0.incarnation()).collect();
    assert!(
        incarnations.windows(2).all(|pair| pair[0] < pair[1]),
        "a restart is a new incarnation of the same id: {incarnations:?}"
    );
    // A restart spawns the incarnation under the sequence the supervisor's
    // ledger drew for it: the reference the incarnation ends with is the one
    // its predecessor's death report was followed by, not a fresh draw.
    let seen_deaths = deaths(&events);
    for n in 1..child_ends.len() {
        assert_eq!(
            child_ends[n].0.incarnation(),
            seen_deaths[n].1.incarnation(),
            "restart {n}: the incarnation's own reference carries the restart's sequence"
        );
        assert!(
            child_ends[n].0.incarnation() > child_ends[n - 1].0.incarnation(),
            "restart {n}: a later sequence than the incarnation it follows"
        );
    }
    for (n, (supervisor, child, _)) in deaths(&events).iter().enumerate() {
        assert_eq!(**supervisor, sup_ref, "the supervisor keeps its reference");
        assert_eq!(
            **child, *child_ends[n].0,
            "the death names the incarnation that ended, not the id alone"
        );
        assert_eq!(
            child.ancestors().next().map(|(id, inc)| (id.clone(), inc)),
            Some((ActorId::from("sup"), sup_ref.incarnation())),
            "the child's ancestry names its supervisor's incarnation"
        );
    }
    // The child's end is raised by the child, before the supervisor reads
    // its death.
    for ends in &child_ends {
        let end_at = events
            .iter()
            .position(|e| matches!(e, TelemetryEvent::ActorEnded { actor, .. } if actor == ends.0))
            .unwrap();
        let death_at = events
            .iter()
            .position(|e| matches!(e, TelemetryEvent::ChildDied { child, .. } if child == ends.0))
            .unwrap();
        assert!(end_at < death_at, "an end comes before its death report");
    }
}

/// A death that makes no restart charges nothing: no budget event, the
/// decision is `Removed`.
async fn a_death_without_a_restart_charges_nothing() {
    let rig = Rig::new("nocharge", KindSet::all());
    let _sup = spawn_sup(
        &rig,
        "sup",
        SupervisionConfig::one_for_one(),
        vec![Kid {
            restart: RestartType::Temporary,
            ..Kid::new("t")
        }],
    )
    .await;
    worker(&rig, "t").await.notify(Cmd::Panic).await.unwrap();
    let events = rig
        .settled("the death", |events| !deaths(events).is_empty())
        .await;
    assert_eq!(budgets(&events), vec![], "no charge for a Temporary death");
    let seen = deaths(&events);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].2, SupervisionAction::Removed);
}

/// A group restart whose chain fails to start a member lays the group out
/// again and charges the budget once more: the third caller of the charge.
/// One crash, several budget events, the last refused beside its death.
async fn a_chain_retry_charges_once_more() {
    let rig = Rig::new("chain", KindSet::all());
    let built = Arc::new(AtomicUsize::new(0));
    let _sup = spawn_sup(
        &rig,
        "sup",
        SupervisionConfig::one_for_all().max_restarts(3, Duration::from_secs(60)),
        vec![
            Kid::new("a"),
            Kid {
                flaky: Some(built.clone()),
                ..Kid::new("b")
            },
        ],
    )
    .await;
    worker(&rig, "b").await.notify(Cmd::Panic).await.unwrap();
    let events = rig
        .settled("the supervisor's end", |events| {
            !ended_of(events, "sup").is_empty()
        })
        .await;
    let charges = budgets(&events);
    assert!(
        charges.len() >= 2,
        "one crash, and the retry charged again: {charges:?}"
    );
    let used: Vec<u32> = charges.iter().map(|c| c.1).collect();
    assert_eq!(
        used,
        (1..=charges.len() as u32)
            .map(|n| n.min(3))
            .collect::<Vec<_>>(),
        "each charge counts the window after it, a refusal reads the maximum"
    );
    assert!(charges[..charges.len() - 1].iter().all(|c| c.0));
    assert!(!charges[charges.len() - 1].0, "the last charge is refused");
    let seen = deaths(&events);
    assert_eq!(
        seen.last().map(|d| d.2),
        Some(SupervisionAction::BudgetExhausted),
        "the refusal is followed by its death with the exhausted decision"
    );
}

/// A supervisor that stops while a death is pending delivers it once, with
/// `SupervisorStopping`, and charges nothing for it.
async fn a_stopping_supervisor_charges_nothing() {
    let rig = Rig::new("stopping", KindSet::all());
    let sup = spawn_sup(
        &rig,
        "sup",
        SupervisionConfig::one_for_one(),
        vec![Kid::new("x"), Kid::new("y")],
    )
    .await;
    let x = worker(&rig, "x").await;
    worker(&rig, "y").await;
    // A held turn keeps the supervisor from reading the child's death; the
    // stop request reaches it at the turn's end, ahead of the death plane.
    sup.notify(()).await.unwrap();
    while !rig.hold.entered.load(Ordering::SeqCst) {
        sleep(Duration::from_millis(5)).await;
    }
    x.notify(Cmd::Panic).await.unwrap();
    rig.until("the child's end", |events| {
        !ended_of(events, "x").is_empty()
    })
    .await;
    sup.stop(StopReason::Graceful).await.unwrap();
    rig.hold.release.notify_one();
    let events = rig
        .settled("the supervisor's end", |events| {
            !ended_of(events, "sup").is_empty()
        })
        .await;
    assert_eq!(
        budgets(&events),
        vec![],
        "a stopping supervisor charges nothing"
    );
    let seen = deaths(&events);
    assert_eq!(
        seen.len(),
        1,
        "the pending death is delivered once: {events:#?}"
    );
    assert_eq!(seen[0].1.id().as_str(), "x");
    assert_eq!(seen[0].2, SupervisionAction::SupervisorStopping);
    assert_eq!(
        *seen[0].1,
        *ended_of(&events, "x")[0].0,
        "the death names the incarnation that ended"
    );
    assert_eq!(ended_of(&events, "y").len(), 1, "orderly stopped with it");
}

// ---------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------

/// The verdict of one incarnation is reported once, whichever of the three
/// writers (the ladder, a wait on the closed cell, a wait for the subtree)
/// reaches it first, with the cause of that writer.
#[tokio::test(flavor = "current_thread")]
async fn the_verdict_is_reported_once_per_incarnation() {
    let name = format!("evt-verdict-{}", SEQ.fetch_add(1, Ordering::Relaxed));
    let system = ActorSystem::create(name).unwrap();
    let sink = system
        .telemetry_mode()
        .collect(KindSet::of(&[EventKind::Unresponsive]));
    // A child with no recorded identity: the supervisor reads as its id,
    // a root with the unknown incarnation.
    let supervisor =
        ActorRef::new_root(ActorId::from("sup"), crate::telemetry::UNKNOWN_INCARNATION);
    let verdicts: Arc<Verdicts> = Arc::default();
    verdicts.set_supervisor(ActorId::from("sup"));

    // The ladder writes first; the closed cell and the subtree wait find the
    // flag set.
    let (ctl, io) = slot_pair_for(verdicts.clone(), Some((ActorId::from("kid"), 7)));
    let (record, _keep) = publish_lane(&io, &system);
    io.write_verdict();
    assert!(!ctl.lost_flag().set_for(Some(&record)));
    assert!(!ctl.lost_flag().set_for_subtree(Some(&record)));
    // The subtree wait writes first; the ladder finds the flag set.
    let (ctl, io) = slot_pair_for(verdicts.clone(), Some((ActorId::from("kid"), 8)));
    let (record, _keep2) = publish_lane(&io, &system);
    assert!(ctl.lost_flag().set_for_subtree(Some(&record)));
    io.write_verdict();
    assert!(!ctl.lost_flag().set_for(Some(&record)));
    // A slot with no key names no incarnation and reports nothing.
    let (ctl, _io) = slot_pair_for(verdicts, None);
    assert!(ctl.lost_flag().set());

    // The events reach the sink through the pump; a duplicate would arrive
    // within the settle time.
    let deadline = Instant::now() + Duration::from_secs(10);
    while sink.events().len() < 2 {
        assert!(Instant::now() < deadline, "the two verdicts arrive");
        sleep(Duration::from_millis(10)).await;
    }
    sleep(Duration::from_millis(150)).await;
    let events = sink.events();
    assert_eq!(events.len(), 2, "one event per incarnation: {events:#?}");
    let causes: Vec<(String, u64, String)> = events
        .iter()
        .map(|event| match event {
            TelemetryEvent::Unresponsive {
                supervisor: sup,
                child,
                cause,
            } => {
                assert_eq!(*sup, supervisor);
                (
                    child.id().to_string(),
                    child.incarnation(),
                    cause.to_string(),
                )
            }
            other => panic!("not a verdict: {other:?}"),
        })
        .collect();
    assert_eq!(causes[0].0, "kid");
    assert_eq!(causes[0].1, 7);
    assert_eq!(causes[1].1, 8);
    assert_ne!(causes[0].2, causes[1].2, "each carries its writer's cause");
}

// ---------------------------------------------------------------------------
// Stop requests of callers
// ---------------------------------------------------------------------------

/// A request reports the lane's answer: a `Kill` is recorded, a later
/// `Graceful` is superseded by it, and after the end the lane refuses.
async fn stop_requests_report_the_lanes_answer() {
    let rig = Rig::new("stop", KindSet::all());
    let entered = Arc::new(AtomicBool::new(false));
    let handle = Probe {
        entered: entered.clone(),
        ..Probe::default()
    }
    .spawn()
    .named("busy")
    .on_system(&rig.system)
    .kill_grace(Duration::from_millis(300))
    .await
    .unwrap();
    handle.notify(Cmd::Park).await.unwrap();
    while !entered.load(Ordering::SeqCst) {
        sleep(Duration::from_millis(5)).await;
    }
    handle.stop(StopReason::Kill).await.unwrap();
    handle.stop(StopReason::Graceful).await.unwrap();
    rig.settled("the abort", |events| !ended_of(events, "busy").is_empty())
        .await;
    assert!(handle.stop(StopReason::Graceful).await.is_err());
    assert!(handle.stop(StopReason::Kill).await.is_err());
    let events = rig
        .settled("the four requests", |events| requests(events).len() >= 4)
        .await;
    let seen: Vec<(StopReason, RaiseOutcome)> = requests(&events)
        .iter()
        .map(|(_, reason, outcome)| ((*reason).clone(), *outcome))
        .collect();
    assert_eq!(
        seen,
        vec![
            (StopReason::Kill, RaiseOutcome::Recorded),
            (StopReason::Graceful, RaiseOutcome::Superseded),
            (StopReason::Graceful, RaiseOutcome::Refused),
            (StopReason::Kill, RaiseOutcome::Refused),
        ],
        "the lane's own answer to each call, in call order"
    );
    let end = ended_of(&events, "busy");
    assert_eq!(end.len(), 1, "one end for the aborted actor: {events:#?}");
    for (actor, _, _) in requests(&events) {
        assert_eq!(
            actor, end[0].0,
            "a request names the incarnation it was made on"
        );
    }
}

/// The system's own stop, kill and kill by id, and the bounded stop, each
/// report their request once.
async fn system_calls_report_their_requests() {
    let rig = Rig::new("syscalls", KindSet::of(&[EventKind::StopRequested]));
    for name in ["s1", "s2", "s3", "s4"] {
        Probe::default()
            .spawn()
            .named(name)
            .on_system(&rig.system)
            .await
            .unwrap();
    }
    rig.system.stop("s1").await.unwrap();
    rig.system.kill("s2").await.unwrap();
    let s3 = rig.system.get::<Probe>("s3").unwrap();
    rig.system.kill_by_id(s3.id()).await.unwrap();
    let s4 = rig.system.get::<Probe>("s4").unwrap();
    s4.stop_within(StopReason::Graceful, Duration::from_secs(5))
        .await
        .unwrap();
    // Gone from the registry: nothing to request, nothing reported. The plain
    // stop returns when the request is recorded, so wait for the end first.
    let waited = Instant::now();
    while rig.system.get::<Probe>("s1").is_some() {
        assert!(
            waited.elapsed() < Duration::from_secs(20),
            "s1 left the registry"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(rig.system.stop("s1").await.is_err());
    // A bounded stop of an actor that ended is refused and says so.
    assert!(s4
        .stop_within(StopReason::Graceful, Duration::from_millis(50))
        .await
        .is_err());
    let events = rig
        .settled("the five requests", |events| requests(events).len() >= 5)
        .await;
    let mut seen: Vec<(String, StopReason, RaiseOutcome)> = requests(&events)
        .iter()
        .map(|(actor, reason, outcome)| (actor.id().to_string(), (*reason).clone(), *outcome))
        .collect();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        seen,
        vec![
            ("s1".into(), StopReason::Graceful, RaiseOutcome::Recorded),
            ("s2".into(), StopReason::Kill, RaiseOutcome::Recorded),
            ("s3".into(), StopReason::Kill, RaiseOutcome::Recorded),
            ("s4".into(), StopReason::Graceful, RaiseOutcome::Recorded),
            ("s4".into(), StopReason::Graceful, RaiseOutcome::Refused),
        ],
        "{events:#?}"
    );
}

// ---------------------------------------------------------------------------
// Nothing without a sink; nothing under a guard
// ---------------------------------------------------------------------------

/// Runs the supervision, stop and abort scenarios once.
async fn every_scenario(rig: &Rig) {
    let _sup = spawn_sup(
        rig,
        "sup",
        SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)),
        vec![Kid::new("c")],
    )
    .await;
    worker(rig, "c").await.notify(Cmd::Panic).await.unwrap();
    worker(rig, "c").await.notify(Cmd::Panic).await.unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let busy = Probe {
        entered: entered.clone(),
        ..Probe::default()
    }
    .spawn()
    .named("busy")
    .on_system(&rig.system)
    .kill_grace(Duration::from_millis(50))
    .await
    .unwrap();
    busy.notify(Cmd::Park).await.unwrap();
    while !entered.load(Ordering::SeqCst) {
        sleep(Duration::from_millis(5)).await;
    }
    rig.system.kill("busy").await.unwrap();
    let plain = Probe::default()
        .spawn()
        .named("plain")
        .on_system(&rig.system)
        .await
        .unwrap();
    rig.system.stop("plain").await.unwrap();
    plain.wait_stopped().await;
    busy.wait_stopped().await;
    sleep(Duration::from_millis(200)).await;
}

/// With a sink armed for other kinds only, none of these events is
/// built or delivered, whatever the actors do.
async fn nothing_is_emitted_with_the_kinds_off() {
    let rig = Rig::new(
        "off",
        KindSet::of(&[EventKind::KillStep, EventKind::HelperEnded]),
    );
    every_scenario(&rig).await;
    let events = rig.events();
    assert!(
        events.iter().all(|e| !matches!(
            e.kind(),
            Some(
                EventKind::StopRequested
                    | EventKind::ActorEnded
                    | EventKind::ChildDied
                    | EventKind::RestartBudget
                    | EventKind::Unresponsive
            )
        )),
        "{events:#?}"
    );
    let mode = rig.system.telemetry_mode();
    for kind in [
        EventKind::StopRequested,
        EventKind::ActorEnded,
        EventKind::ChildDied,
        EventKind::RestartBudget,
        EventKind::Unresponsive,
    ] {
        assert!(!mode.armed(kind), "{kind:?} stays off");
    }
}

/// The sink of the guard row: on every event it asks a second thread to read
/// the registry, ask a status, and claim a name (the subject's own for a
/// request that is still registered). A thread that waits on a guard the
/// emitter holds does not finish, and the sink counts it. The actors the
/// sink itself claims names for raise events too; for those events it reads
/// the registry but claims nothing, so its own claims do not feed back into
/// the pump faster than the pump delivers.
const CLAIM_PREFIX: &str = "probe-claim-";

fn guard_probe(
    system: Arc<ActorSystem>,
    handle: tokio::runtime::Handle,
    calls: Arc<AtomicUsize>,
    stuck: Arc<AtomicUsize>,
    kinds: Arc<Mutex<Vec<EventKind>>>,
) -> impl Fn(&TelemetryEvent<'static>) + Send + Sync + 'static {
    move |event| {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        if let Some(kind) = event.kind() {
            kinds.lock().unwrap().push(kind);
        }
        let claim = match event {
            TelemetryEvent::StopRequested { actor, outcome, .. }
                if *outcome != RaiseOutcome::Refused =>
            {
                actor.id().to_string()
            }
            _ => format!("{CLAIM_PREFIX}{n}-{}", SEQ.fetch_add(1, Ordering::Relaxed)),
        };
        let own = event
            .subject()
            .is_some_and(|actor| actor.id().to_string().starts_with(CLAIM_PREFIX));
        let (done, finished) = std::sync::mpsc::channel();
        let system = system.clone();
        let handle = handle.clone();
        std::thread::spawn(move || {
            let _entered = handle.enter();
            let _ = system.registered();
            for id in system.actor_ids() {
                let _ = system.actor_status(&id);
                let _ = system.get_by_id::<Probe>(&id);
            }
            if own {
                let _ = done.send(());
                return;
            }
            let mut spawn = Box::pin(
                Probe::default()
                    .spawn()
                    .named(claim)
                    .on_system(&system)
                    .into_future(),
            );
            let mut cx = Context::from_waker(Waker::noop());
            // Polled once: the registry answers (taken, or claimed) before
            // the start is awaited. The future is leaked, not dropped, so the
            // probe's actor is left as it started.
            let _ = matches!(spawn.as_mut().poll(&mut cx), Poll::Ready(_));
            std::mem::forget(spawn);
            let _ = done.send(());
        });
        if finished.recv_timeout(Duration::from_secs(30)).is_err() {
            stuck.fetch_add(1, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// A handler never runs on a path that still owes the crate a step
// ---------------------------------------------------------------------------

/// A gate a handler blocks on until the test opens it. The wait is capped, so
/// a failed test does not hold its runtime for longer than that.
struct Gate {
    open: Mutex<bool>,
    changed: std::sync::Condvar,
    entered: AtomicBool,
}

impl Gate {
    fn new() -> Arc<Gate> {
        Arc::new(Gate {
            open: Mutex::new(false),
            changed: std::sync::Condvar::new(),
            entered: AtomicBool::new(false),
        })
    }

    /// What the handler calls: marks the gate entered and blocks its thread
    /// until the gate opens.
    fn block(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        while !*open {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return;
            }
            open = self
                .changed
                .wait_timeout(open, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    fn release(&self) {
        *self.open.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.changed.notify_all();
    }

    fn is_open(&self) -> bool {
        *self.open.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Waits until a handler is blocked on the gate.
    async fn entered(&self) {
        while !self.entered.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(5)).await;
        }
    }
}

/// Opens its gate when it drops, so a failed assertion never leaves a handler
/// blocking the runtime.
struct Release(Arc<Gate>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match tokio::time::timeout(Duration::from_secs(10), fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

/// A system with a handler for `kind` that blocks on a gate.
fn blocked_system(tag: &str, kind: EventKind) -> (Arc<ActorSystem>, Arc<Gate>, Release) {
    let name = format!("evt-{tag}-{}", SEQ.fetch_add(1, Ordering::Relaxed));
    let system = ActorSystem::create(name).expect("a fresh system name");
    let gate = Gate::new();
    let blocker = gate.clone();
    assert!(system.attach_telemetry("block", KindSet::of(&[kind]), move |_| blocker.block()));
    (system, gate.clone(), Release(gate))
}

/// A handler for `ActorEnded` that blocks does not delay what the actor's end
/// still owes: the `Stopped` status, the answer of a failed start, and the
/// restart of a supervised child (whose successor waits for that end), nor
/// does it earn the child a false `Unresponsive` verdict. Each row has its
/// own system and gate. The pump's helper is the one that blocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_end_handler_delays_no_step_the_end_owes() {
    {
        let (sys, gate, _release) = blocked_system("blocked-status", EventKind::ActorEnded);
        let handle = Probe::default()
            .spawn()
            .named("a")
            .on_system(&sys)
            .await
            .unwrap();
        handle.stop(StopReason::Graceful).await.unwrap();
        within("the handler is entered", gate.entered()).await;
        let rx = handle.status_rx();
        within("Stopped is published while the handler blocks", async {
            while read_status(&rx) != ActorStatus::Stopped {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(!gate.is_open(), "the handler was still blocked");
    }
    {
        let (sys, gate, _release) = blocked_system("blocked-ack", EventKind::ActorEnded);
        let on = sys.clone();
        let spawn = tokio::spawn(async move {
            Probe {
                fail_start: true,
                ..Probe::default()
            }
            .spawn()
            .named("b")
            .on_system(&on)
            .await
            .map(|_| ())
        });
        within("the handler is entered", gate.entered()).await;
        let answer = within("the failed start answers while the handler blocks", spawn)
            .await
            .unwrap();
        assert!(answer.is_err(), "the refused start fails the spawn");
        assert!(!gate.is_open(), "the handler was still blocked");
    }
    {
        let (sys, gate, _release) = blocked_system("blocked-restart", EventKind::ActorEnded);
        let rig = Rig {
            sink: sys
                .telemetry_mode()
                .collect(KindSet::of(&[EventKind::Unresponsive])),
            system: sys,
            hold: Hold::default(),
        };
        spawn_sup(
            &rig,
            "sup",
            SupervisionConfig::one_for_one(),
            vec![Kid::new("k")],
        )
        .await;
        let old = worker(&rig, "k").await;
        old.stop(StopReason::Graceful).await.unwrap();
        within("the handler is entered", gate.entered()).await;
        within("the child restarts while the handler blocks", async {
            loop {
                if !old.is_alive() {
                    if let Some(next) = rig.system.get::<Probe>("k") {
                        if next.send(Cmd::Ping).await.is_ok() {
                            break;
                        }
                    }
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(!gate.is_open(), "the handler was still blocked");
        gate.release();
        sleep(Duration::from_millis(300)).await;
        assert!(
            rig.events().is_empty(),
            "no verdict follows: {:#?}",
            rig.events()
        );
    }
}

/// Mail whose drop is noted.
struct Tracked(Arc<AtomicBool>);

impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

enum Notice {
    Monitor(ActorHandle<Quiet>),
    Link(ActorHandle<Quiet>),
    /// Holds the turn until the gate has a permit.
    Park(Arc<tokio::sync::Semaphore>),
    Mail(#[allow(dead_code)] Tracked),
    Down(#[allow(dead_code)] crate::types::Down),
    Exit(#[allow(dead_code)] crate::types::LinkExit),
}

/// Watches or links an actor and notes when a notice reaches `handle` with
/// `Stopping` visible.
struct Deliverer {
    parked: Arc<AtomicBool>,
    final_delivery: Arc<AtomicBool>,
}

impl Actor for Deliverer {
    type Message = Notice;
    type Response = ();

    async fn handle(&mut self, msg: Notice, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Notice::Monitor(target) => {
                ctx.monitor(&target, Notice::Down).expect("running");
            }
            Notice::Link(target) => {
                ctx.trap_exit(Notice::Exit);
                ctx.link(&target).expect("running");
            }
            Notice::Park(gate) => {
                self.parked.store(true, Ordering::SeqCst);
                let _ = gate.acquire().await;
            }
            Notice::Mail(_) => {}
            Notice::Down(_) | Notice::Exit(_) => {
                if ctx.status() == ActorStatus::Stopping {
                    self.final_delivery.store(true, Ordering::SeqCst);
                }
            }
        }
        Ok(())
    }
}

/// The actor that is watched and linked.
struct Quiet;

impl Actor for Quiet {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// The `TurnFinished` of a final delivery (a `Down` or a `LinkExit` still
/// pending when the actor stops, delivered through `handle` with `Stopping`
/// visible) runs no handler on the actor's task: a handler for turn events
/// that blocks delays neither `Stopped` nor the drop of the mail left in the
/// queue. Rows: the notice queued in the mailbox or waiting for room, for a
/// monitor and for a link. The handler blocks only for the turn of a notice
/// delivered while the status reads `Stopping`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_turn_handler_delays_no_step_a_final_delivery_leaves_owed() {
    for (link, queued) in [(false, false), (false, true), (true, false), (true, true)] {
        let row = format!("link {link}, queued {queued}");
        let name = format!("evt-final-turn-{}", SEQ.fetch_add(1, Ordering::Relaxed));
        let sys = ActorSystem::create(name).expect("a fresh system name");
        let gate = Gate::new();
        let _release = Release(gate.clone());
        let final_delivery = Arc::new(AtomicBool::new(false));
        let (blocker, armed) = (gate.clone(), final_delivery.clone());
        assert!(sys.attach_telemetry(
            "block",
            KindSet::of(&[EventKind::TurnFinished]),
            move |_| {
                if armed.load(Ordering::SeqCst) {
                    blocker.block();
                }
            }
        ));
        let parked = Arc::new(AtomicBool::new(false));
        // A full mailbox keeps the notice from queueing.
        let capacity = if queued { 64 } else { 1 };
        let watcher = Deliverer {
            parked: parked.clone(),
            final_delivery: final_delivery.clone(),
        }
        .spawn()
        .named("watcher")
        .with_config(crate::ActorConfig::default().with_mailbox_capacity(capacity))
        .on_system(&sys)
        .await
        .unwrap();
        let target = Quiet.spawn().named("target").on_system(&sys).await.unwrap();
        watcher
            .send(if link {
                Notice::Link(target.clone())
            } else {
                Notice::Monitor(target.clone())
            })
            .await
            .unwrap();
        let hold = Arc::new(tokio::sync::Semaphore::new(0));
        watcher.notify(Notice::Park(hold.clone())).await.unwrap();
        within(&format!("{row}: the turn is parked"), async {
            while !parked.load(Ordering::SeqCst) {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        let dropped = Arc::new(AtomicBool::new(false));
        watcher
            .notify(Notice::Mail(Tracked(dropped.clone())))
            .await
            .unwrap();
        target.stop(StopReason::Graceful).await.unwrap();
        within(&format!("{row}: the target ends"), target.wait_stopped()).await;
        if queued {
            within(&format!("{row}: the notice is queued"), async {
                while watcher.mailbox_len() < 2 {
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
        }
        watcher.stop(StopReason::Graceful).await.unwrap();
        hold.add_permits(1);
        within(&format!("{row}: the handler is entered"), gate.entered()).await;
        let rx = watcher.status_rx();
        within(
            &format!("{row}: Stopped is published while the handler blocks"),
            async {
                while read_status(&rx) != ActorStatus::Stopped {
                    sleep(Duration::from_millis(5)).await;
                }
            },
        )
        .await;
        assert!(
            dropped.load(Ordering::SeqCst),
            "{row}: the queued mail was dropped"
        );
        assert!(!gate.is_open(), "{row}: the handler was still blocked");
    }
}

/// A handler for `Unresponsive` that blocks does not delay the verdict's
/// publication: the cell a supervisor waits on settles while the handler is
/// still blocked, and the writer of the verdict returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_verdict_handler_delays_neither_the_writer_nor_the_cell() {
    let (sys, gate, _release) = blocked_system("blocked-verdict", EventKind::Unresponsive);
    let verdicts: Arc<Verdicts> = Arc::default();
    verdicts.set_supervisor(ActorId::from("sup"));
    let (ctl, io) = slot_pair_for(verdicts, Some((ActorId::from("kid"), 7)));
    let (_record, _keep) = publish_lane(&io, &sys);
    let mut watch = ctl.watch();
    let started = std::time::Instant::now();
    io.write_verdict();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the writer returned only after {:?}: it ran the handler",
        started.elapsed()
    );
    within("the handler is entered", gate.entered()).await;
    let outcome = within(
        "the cell settles while the handler blocks",
        await_cell(&mut watch, 7),
    )
    .await;
    assert!(matches!(outcome, CellOutcome::Unresponsive), "{outcome:?}");
    assert!(!gate.is_open(), "the handler was still blocked");
}

/// A crew (a supervisor with two members) spawned while no sink took events
/// raises its events once a sink arrives: the supervisor is named with its
/// registered id and its real incarnation number in every event, each member's
/// end, stop request and restart carries the right ancestry, and the numbers
/// agree across events and with the registry. The identity of an incarnation
/// spawned with no sink is read from the registry by the one pass the sink's
/// arrival runs: a label and the ends of actors with no identity walk the
/// registry never, the sink's arrival once, and events about actors that
/// ended before it not again.
#[tokio::test(flavor = "current_thread")]
async fn a_crew_spawned_with_no_sink_raises_its_events_once_a_sink_arrives() {
    let rig = Rig::new("late-sink", KindSet::of(&[]));
    let sup = spawn_sup(
        &rig,
        "sup",
        SupervisionConfig::one_for_one(),
        vec![
            Kid::new("c"),
            Kid {
                restart: RestartType::Temporary,
                ..Kid::new("d")
            },
        ],
    )
    .await;
    let first = worker(&rig, "c").await;
    let other = worker(&rig, "d").await;
    for handle in [&first, &other] {
        assert!(
            super::identity_of(&rig.system, handle.stop_lane().abort_record()).is_none(),
            "nothing is recorded for an incarnation spawned with no sink"
        );
    }
    // A labelled actor makes the system hold an identity; actors with none
    // end beside it, and nothing walks the registry.
    let labelled = Quiet.spawn().on_system(&rig.system).await.unwrap();
    crate::telemetry::set_label(
        labelled.stop_lane().abort_record(),
        Some(&rig.system),
        labelled.id(),
        Arc::from("labelled"),
    );
    let mut gone = Vec::new();
    for _ in 0..3 {
        let quiet = Quiet.spawn().on_system(&rig.system).await.unwrap();
        quiet.stop(StopReason::Graceful).await.unwrap();
        quiet.wait_stopped().await;
        gone.push(quiet);
    }
    assert_eq!(rig.system.identity_walks(), 0, "a label starts no pass");
    let sink = rig.system.telemetry_mode().collect(KindSet::all());
    assert_eq!(
        rig.system.identity_walks(),
        1,
        "the sink's arrival runs the pass"
    );

    // A stop request on one member, then its end.
    other.stop(StopReason::Graceful).await.unwrap();
    // A crash of the other: its end, the budget charge, the death with its
    // decision, and a restart.
    first.notify(Cmd::Panic).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let events = sink.events();
        if deaths(&events)
            .iter()
            .any(|(_, child, _)| child.id().as_str() == "c")
            && !budgets(&events).is_empty()
            && !ended_of(&events, "c").is_empty()
            && !ended_of(&events, "d").is_empty()
        {
            break;
        }
        assert!(Instant::now() < deadline, "the events arrive: {events:#?}");
        sleep(Duration::from_millis(10)).await;
    }

    let sup_identity = super::identity_of(&rig.system, sup.stop_lane().abort_record())
        .expect("the supervisor was identified by the first event that named it");
    let sup_inc = sup_identity.incarnation();
    assert_ne!(sup_inc, crate::telemetry::UNKNOWN_INCARNATION);
    assert!(sup_identity.actor().ancestors().next().is_none());
    let above = |actor: &ActorRef| -> Vec<(String, u64)> {
        actor
            .ancestors()
            .map(|(id, n)| (id.to_string(), n))
            .collect()
    };
    let under_sup = vec![("sup".to_string(), sup_inc)];

    // Settle: the restart's own events follow the death.
    let second = worker(&rig, "c").await;
    sleep(Duration::from_millis(100)).await;
    let events = sink.events();

    // The death names the supervisor with its real incarnation, and the child
    // with the incarnation the supervisor holds.
    let (supervisor, child, _) = deaths(&events)
        .into_iter()
        .find(|(_, child, _)| child.id().as_str() == "c")
        .expect("the death of the crashed member");
    assert_eq!(supervisor.id().as_str(), "sup");
    assert_eq!(supervisor.incarnation(), sup_inc);
    assert!(supervisor.ancestors().next().is_none());
    assert_eq!(child.id().as_str(), "c");
    assert_eq!(above(child), under_sup);

    // The budget event names the same supervisor.
    let budget_supervisors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::RestartBudget { supervisor, .. } => Some(supervisor),
            _ => None,
        })
        .collect();
    assert!(!budget_supervisors.is_empty());
    for supervisor in budget_supervisors {
        assert_eq!(supervisor.incarnation(), sup_inc);
        assert_eq!(supervisor.id().as_str(), "sup");
    }

    // The end of the first incarnation of the crashed member: one event, the
    // incarnation the death names, the supervisor above it.
    let ends = ended_of(&events, "c");
    assert_eq!(
        ends.len(),
        1,
        "one end for the first incarnation: {events:#?}"
    );
    assert_eq!(ends[0].0.incarnation(), child.incarnation());
    assert_eq!(above(ends[0].0), under_sup);
    assert!(matches!(ends[0].1, StopReason::Failure(_)));
    assert_eq!(ends[0].2, None, "no lifetime: it was born before the sink");

    // The stopped member: its request and its end carry the same ancestry.
    let asked: Vec<_> = requests(&events)
        .into_iter()
        .filter(|(actor, _, _)| actor.id().as_str() == "d")
        .collect();
    assert_eq!(asked.len(), 1, "{events:#?}");
    assert_eq!(above(asked[0].0), under_sup);
    assert_eq!(asked[0].2, RaiseOutcome::Recorded);
    let ends = ended_of(&events, "d");
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].0.incarnation(), asked[0].0.incarnation());
    assert_eq!(above(ends[0].0), under_sup);

    // The restart, spawned with a sink armed, lists the supervisor with its
    // real number, not the unknown one.
    let identity = super::identity_of(&rig.system, second.stop_lane().abort_record())
        .expect("an incarnation spawned with a sink records its identity");
    assert_ne!(identity.incarnation(), child.incarnation());
    assert_eq!(above(identity.actor()), under_sup);
    second.stop(StopReason::Graceful).await.unwrap();
    within("the restart's end", async {
        while ended_of(&sink.events(), "c").len() < 2 {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let events = sink.events();
    let ends = ended_of(&events, "c");
    assert_eq!(ends.len(), 2);
    assert_eq!(ends[1].0.incarnation(), identity.incarnation());
    assert_eq!(above(ends[1].0), under_sup);
    assert!(
        ended_of(&events, "sup").is_empty(),
        "the supervisor still runs"
    );
    // Requests on actors that ended before the sink find no identity and
    // walk nothing more: the pass ran once for this engagement.
    for quiet in &gone {
        let _ = quiet.stop(StopReason::Graceful).await;
    }
    assert_eq!(rig.system.identity_walks(), 1, "one pass per engagement");
}

// ---------------------------------------------------------------------------
// The stops the crate makes itself
// ---------------------------------------------------------------------------

/// A supervisor with one worker of its own name and shutdown policy; the
/// worker parks inside its first turn when it is told to.
struct Crew {
    kid: &'static str,
    shutdown: Shutdown,
    entered: Arc<AtomicBool>,
}

impl Actor for Crew {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let entered = self.entered.clone();
        ctx.spawn_child(move || Probe {
            entered: entered.clone(),
            ..Probe::default()
        })
        .named(self.kid)
        .restart_type(RestartType::Temporary)
        .shutdown(self.shutdown)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A crew named `name` whose worker is `kid`, with the worker parked in a turn
/// when `park` is set.
async fn crew(
    rig: &Rig,
    name: &'static str,
    kid: &'static str,
    shutdown: Shutdown,
    park: bool,
) -> ActorHandle<Crew> {
    let entered = Arc::new(AtomicBool::new(false));
    let handle = Crew {
        kid,
        shutdown,
        entered: entered.clone(),
    }
    .spawn()
    .named(name)
    .on_system(&rig.system)
    .with_supervision(SupervisionConfig::one_for_one())
    .await
    .expect("the crew starts");
    let worker = worker(rig, kid).await;
    if park {
        worker.notify(Cmd::Park).await.unwrap();
        while !entered.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(5)).await;
        }
    }
    handle
}

/// The `Raised` steps of the actors named `id`.
fn raised_steps(events: &Events, id: &str) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                TelemetryEvent::KillStep { actor, step: super::KillStepKind::Raised, .. }
                    if actor.id().as_str() == id
            )
        })
        .count()
}

/// The reasons and answers of the requests made on the actor named `id`.
fn requests_on(events: &Events, id: &str) -> Vec<(StopReason, RaiseOutcome)> {
    requests(events)
        .into_iter()
        .filter(|(actor, _, _)| actor.id().as_str() == id)
        .map(|(_, reason, outcome)| (reason.clone(), outcome))
        .collect()
}

/// Every stop the crate raises itself queues a request event with the lane's
/// answer, and every `Kill` it raises a `Raised` step: the shutdown walk's
/// request to a root, a supervisor's first signal to a child, its ladder's
/// `Kill`, the link guard of a child whose supervisor was killed, and the
/// cut of a start that timed out.
async fn the_stops_the_crate_makes_are_reported() {
    let rig = Rig::new(
        "own-stops",
        KindSet::of(&[EventKind::StopRequested, EventKind::KillStep]),
    );

    // A supervisor whose child has the `Kill` policy: its stop raises `Kill`
    // on the child as its first signal.
    let brutal = crew(&rig, "brutal-sup", "brutal-kid", Shutdown::Kill, false).await;
    brutal.stop(StopReason::Graceful).await.unwrap();
    // A child that ignores its orderly stop: the ladder raises `Kill` after the
    // window.
    let slow = crew(
        &rig,
        "slow-sup",
        "slow-kid",
        Shutdown::Timeout(Duration::from_millis(60)),
        true,
    )
    .await;
    slow.stop(StopReason::Graceful).await.unwrap();
    // A supervisor killed outright: its link guards stop the child.
    let doomed = crew(&rig, "doomed-sup", "doomed-kid", Shutdown::Infinity, false).await;
    rig.system.kill("doomed-sup").await.unwrap();
    // A start cut by its bound.
    let cut = Probe {
        park_start: true,
        ..Probe::default()
    }
    .spawn()
    .named("cut-start")
    .on_system(&rig.system)
    .start_timeout(Duration::from_millis(100))
    .await;
    assert!(matches!(cut, Err(SpawnError::StartTimeout)));
    // A root stopped by the shutdown walk.
    Probe::default()
        .spawn()
        .named("walked")
        .on_system(&rig.system)
        .await
        .unwrap();

    let events = rig
        .until("the five reports", |events| {
            requests_on(events, "brutal-kid")
                .iter()
                .any(|(r, _)| *r == StopReason::Kill)
                && requests_on(events, "slow-kid")
                    .iter()
                    .any(|(r, _)| *r == StopReason::Kill)
                && requests_on(events, "doomed-kid")
                    .iter()
                    .any(|(r, _)| *r == StopReason::Kill)
                && requests_on(events, "cut-start")
                    .iter()
                    .any(|(r, _)| *r == StopReason::Kill)
        })
        .await;
    drop((brutal, slow, doomed));
    assert!(
        requests_on(&events, "brutal-kid").contains(&(StopReason::Kill, RaiseOutcome::Recorded)),
        "the supervisor's first signal to a Kill-policy child: {events:#?}"
    );
    assert!(raised_steps(&events, "brutal-kid") >= 1, "{events:#?}");
    let slow_kid = requests_on(&events, "slow-kid");
    assert_eq!(
        slow_kid.first().map(|(reason, _)| reason.clone()),
        Some(StopReason::ParentRequest),
        "the orderly first signal comes first: {slow_kid:?}"
    );
    assert!(
        slow_kid.contains(&(StopReason::Kill, RaiseOutcome::Recorded)),
        "the ladder's Kill after the window: {slow_kid:?}"
    );
    assert!(raised_steps(&events, "slow-kid") >= 1, "{events:#?}");
    assert!(
        requests_on(&events, "doomed-kid").contains(&(StopReason::Kill, RaiseOutcome::Recorded)),
        "the link guard's Kill: {events:#?}"
    );
    assert!(raised_steps(&events, "doomed-kid") >= 1, "{events:#?}");
    assert!(
        requests_on(&events, "cut-start").contains(&(StopReason::Kill, RaiseOutcome::Recorded)),
        "the cut of a start: {events:#?}"
    );
    assert!(raised_steps(&events, "cut-start") >= 1, "{events:#?}");

    // The shutdown walk comes last: it asks every root still registered.
    rig.system.shutdown().await;
    let events = rig
        .until("the walk's request", |events| {
            requests_on(events, "walked")
                .iter()
                .any(|(r, _)| *r == StopReason::ParentRequest)
        })
        .await;
    assert!(
        requests_on(&events, "walked")
            .contains(&(StopReason::ParentRequest, RaiseOutcome::Recorded)),
        "{events:#?}"
    );
}

// ---------------------------------------------------------------------------
// Events lost to a sink that armed late
// ---------------------------------------------------------------------------

/// A request made through a handle to an incarnation that ended before the
/// sink armed names no incarnation the registry or the table holds, so no
/// event can be built: the loss is counted with the other drops.
async fn an_event_that_cannot_name_its_incarnation_is_counted() {
    let rig = Rig::new("late-count", KindSet::of(&[]));
    let handle = Probe::default()
        .spawn()
        .named("early")
        .on_system(&rig.system)
        .await
        .unwrap();
    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;
    let sink = rig
        .system
        .telemetry_mode()
        .collect(KindSet::of(&[EventKind::StopRequested]));
    let before = rig.system.telemetry_mode().pump.dropped_for_test();
    assert!(handle.stop(StopReason::Graceful).await.is_err());
    assert_eq!(
        rig.system.telemetry_mode().pump.dropped_for_test(),
        before + 1,
        "the event that could not be built is counted"
    );
    sleep(Duration::from_millis(100)).await;
    assert!(sink.events().is_empty(), "no event names the ended actor");
}

// ---------------------------------------------------------------------------
// Where a stop request came from
// ---------------------------------------------------------------------------

/// What the watcher of the origin rows is told to do.
enum OriginCmd {
    StopSelf,
    Watch(ActorHandle<Probe>),
    /// Traps exits through a map that panics, then links to the target.
    Trap(ActorHandle<Probe>),
    /// Sends the target a request whose reply map panics.
    Ask(ActorHandle<Probe>),
    /// Monitors a group through a map that panics.
    Group(crate::actor::pg::PgScope),
    Link(ActorHandle<Probe>),
    Gone,
}

/// An actor that stops itself, makes a signal map that panics (a monitor, a
/// trap map, a request or a group monitor), or links to a target without
/// trapping exits.
struct Origin;

impl Actor for Origin {
    type Message = OriginCmd;
    type Response = ();

    async fn handle(&mut self, msg: OriginCmd, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            OriginCmd::StopSelf => ctx
                .stop(StopReason::Graceful)
                .expect("the lane takes the actor's own stop"),
            OriginCmd::Watch(target) => {
                ctx.monitor(&target, |_| -> OriginCmd {
                    std::panic::resume_unwind(Box::new("the map panics"))
                })
                .expect("the monitor is made");
            }
            OriginCmd::Trap(target) => {
                ctx.trap_exit(|_| -> OriginCmd {
                    std::panic::resume_unwind(Box::new("the map panics"))
                });
                ctx.link(&target).expect("the link is made");
            }
            OriginCmd::Ask(target) => {
                ctx.request(&target, Cmd::Ping, |_| -> OriginCmd {
                    std::panic::resume_unwind(Box::new("the map panics"))
                })
                .expect("the request is made");
            }
            OriginCmd::Group(scope) => {
                ctx.monitor_group(&scope, "origin-group", |_| -> OriginCmd {
                    std::panic::resume_unwind(Box::new("the map panics"))
                })
                .expect("the group monitor is made");
            }
            OriginCmd::Link(target) => ctx.link(&target).expect("the link is made"),
            OriginCmd::Gone => {}
        }
        Ok(())
    }
}

/// The origin rows: the actor's own stop, a map that panics for each of the
/// four signals that have one, and a `Kill` an exit signal raises with the
/// peer's reason as its cause.
#[derive(Clone, Copy, Debug)]
enum OriginRow {
    OwnStop,
    MapPanicMonitor,
    MapPanicTrap,
    MapPanicRequest,
    MapPanicGroup,
    ExitCause,
}

async fn a_request_reports_where_it_came_from() {
    for row in [
        OriginRow::OwnStop,
        OriginRow::MapPanicMonitor,
        OriginRow::MapPanicTrap,
        OriginRow::MapPanicRequest,
        OriginRow::MapPanicGroup,
        OriginRow::ExitCause,
    ] {
        let rig = Rig::new(
            "origin",
            KindSet::of(&[EventKind::StopRequested, EventKind::ActorEnded]),
        );
        let name = format!("origin-{row:?}");
        let watcher = Origin
            .spawn()
            .named(name.clone())
            .on_system(&rig.system)
            .await
            .unwrap();
        let target = Probe::default()
            .spawn()
            .named(format!("target-{row:?}"))
            .on_system(&rig.system)
            .await
            .unwrap();
        match row {
            OriginRow::OwnStop => watcher.send(OriginCmd::StopSelf).await.unwrap(),
            OriginRow::MapPanicMonitor => {
                watcher
                    .send(OriginCmd::Watch(target.clone()))
                    .await
                    .unwrap();
                target.notify(Cmd::Fail).await.unwrap();
            }
            OriginRow::MapPanicTrap => {
                watcher.send(OriginCmd::Trap(target.clone())).await.unwrap();
                target.notify(Cmd::Fail).await.unwrap();
            }
            OriginRow::MapPanicRequest => {
                watcher.send(OriginCmd::Ask(target.clone())).await.unwrap();
            }
            OriginRow::MapPanicGroup => {
                let scope = rig.system.pg();
                watcher.send(OriginCmd::Group(scope.clone())).await.unwrap();
                scope.join("origin-group", &target).unwrap();
            }
            OriginRow::ExitCause => {
                watcher.send(OriginCmd::Link(target.clone())).await.unwrap();
                target.notify(Cmd::Fail).await.unwrap();
            }
        }
        let _ = watcher.notify(OriginCmd::Gone).await;
        let of_watcher = |events: &Events| -> Vec<TelemetryEvent<'static>> {
            events
                .iter()
                .filter(|event| {
                    matches!(event, TelemetryEvent::StopRequested { actor, .. }
                        if actor.id().as_str() == name)
                })
                .cloned()
                .collect()
        };
        let events = rig
            .until(&format!("{row:?}: the watcher's request"), |events| {
                !of_watcher(events).is_empty()
            })
            .await;
        let seen = of_watcher(&events);
        let TelemetryEvent::StopRequested {
            reason,
            outcome,
            origin,
            cause,
            ..
        } = &seen[0]
        else {
            unreachable!("filtered on the kind");
        };
        assert_eq!(*outcome, RaiseOutcome::Recorded, "{row:?}: {seen:#?}");
        match row {
            OriginRow::OwnStop => {
                assert_eq!(*reason, StopReason::Graceful, "{row:?}");
                assert_eq!(*origin, super::StopOrigin::Requested, "{row:?}");
                assert!(cause.is_none(), "{row:?}: {cause:?}");
            }
            OriginRow::MapPanicMonitor
            | OriginRow::MapPanicTrap
            | OriginRow::MapPanicRequest
            | OriginRow::MapPanicGroup => {
                assert!(
                    matches!(reason, StopReason::Failure(ActorError::Panic(_))),
                    "{row:?}: {reason:?}"
                );
                assert_eq!(*origin, super::StopOrigin::MapPanic, "{row:?}");
                assert!(cause.is_none(), "{row:?}: {cause:?}");
            }
            OriginRow::ExitCause => {
                assert_eq!(*reason, StopReason::Kill, "{row:?}");
                assert_eq!(*origin, super::StopOrigin::Requested, "{row:?}");
                assert!(
                    matches!(cause, Some(StopReason::Failure(_))),
                    "{row:?}: the cause is the peer's reason: {cause:?}"
                );
                // The end event reads the cause as the reason, as the status
                // plane does.
                let events = rig
                    .until(&format!("{row:?}: the watcher's end"), |events| {
                        !ended_of(events, &name).is_empty()
                    })
                    .await;
                let ends = ended_of(&events, &name);
                assert_eq!(ends.len(), 1, "{row:?}: {ends:#?}");
                assert!(
                    matches!(ends[0].1, StopReason::Failure(_)),
                    "{row:?}: the end event reads the peer's reason: {:?}",
                    ends[0].1
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_reports_where_it_came_from_multi_thread() {
    a_request_reports_where_it_came_from().await;
}

// ---------------------------------------------------------------------------
// A task dropped before its first poll
// ---------------------------------------------------------------------------

/// A task aborted before it was first polled has its context dropped first,
/// with the registry guard's drop after it: the context writes the reason
/// and reports the one end event, and has sealed the signal inbox by the time
/// the guard frees the name, so a signal pushed there is refused.
///
/// Current thread only: nothing polls the task before the test awaits.
#[tokio::test(flavor = "current_thread")]
async fn a_task_dropped_before_its_first_poll_reports_one_end_and_refuses_later_signals() {
    use crate::actor::link::ExitPush;
    use crate::actor::runtime::{end_hooks, spawn_actor, ActorConfig};
    use crate::actor::supervision::DEFAULT_KILL_GRACE;
    use crate::types::PeerRef;

    let rig = Rig::new("unpolled", KindSet::of(&[EventKind::ActorEnded]));
    let id = ActorId::from("unpolled");
    let (handle, join, _subtree) = spawn_actor(
        id.clone(),
        Probe::default(),
        ActorConfig::default(),
        None,
        Some(rig.system.clone()),
        None,
        true,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the actor spawns");
    let pushed: Arc<Mutex<Option<ExitPush>>> = Arc::default();
    {
        let record = handle.stop_lane_ref().abort_record().clone();
        let pushed = pushed.clone();
        end_hooks::attach(end_hooks::NAME_FREED, id.clone(), move || {
            let peer = PeerRef {
                actor_id: ActorId::from("peer"),
                incarnation: record.incarnation(),
            };
            *pushed.lock().unwrap() =
                Some(record.push_exit_or_trap(peer, StopReason::Failure(ActorError::user("x"))));
        });
    }
    join.abort();
    let _ = join.await;
    assert_eq!(handle.wait_stopped().await, StopReason::Kill);
    let events = rig
        .settled("the end event", |events| !ended(events).is_empty())
        .await;
    let ends = ended_of(&events, "unpolled");
    assert_eq!(ends.len(), 1, "exactly one end event: {events:#?}");
    assert_eq!(ends[0].1, &StopReason::Kill);
    assert_eq!(
        *pushed.lock().unwrap(),
        Some(ExitPush::Sealed),
        "a signal pushed when the name is freed is refused"
    );
}
