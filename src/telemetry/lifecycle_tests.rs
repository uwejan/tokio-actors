//! The spawn events (an accepted spawn, a refused one) and the restart
//! attempt event with the supervisor's count of attempts in flight. Each
//! scenario reads the events through a collecting sink on the system's mode,
//! on both runtime flavors, and orders its steps by what it observes.

use std::future::IntoFuture;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{sleep, Instant};

use super::{AttemptOutcome, EventKind, KindSet, TelemetryEvent, TestSinkGuard};
use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::runtime::{end_hooks, ActorConfig};
use crate::actor::supervision::{AutoShutdown, SupervisionConfig};
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult, SpawnError};
use crate::system::ActorSystem;
use crate::types::{RestartType, Shutdown, StopReason};

static SEQ: AtomicU64 = AtomicU64::new(0);

type Events = Vec<TelemetryEvent<'static>>;

/// A system of its own with a collecting sink on its mode.
struct Rig {
    system: Arc<ActorSystem>,
    sink: TestSinkGuard,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        let name = format!("life-{tag}-{}", SEQ.fetch_add(1, Ordering::Relaxed));
        let system = ActorSystem::create(name).expect("a fresh system name");
        let sink = system.telemetry_mode().collect(KindSet::of(&[
            EventKind::ActorSpawned,
            EventKind::SpawnRefused,
            EventKind::RestartAttempt,
            EventKind::RestartBudget,
            EventKind::ChildDied,
            EventKind::ActorEnded,
        ]));
        Rig { system, sink }
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
            sleep(Duration::from_millis(5)).await;
        }
    }

    /// The events once they have stopped coming: waits for `pred`, then a
    /// settle time, and reads again.
    async fn settled(&self, what: &str, pred: impl Fn(&Events) -> bool) -> Events {
        self.until(what, pred).await;
        sleep(Duration::from_millis(100)).await;
        self.events()
    }
}

/// The attempts of a supervisor that have begun and not settled, read from
/// the supervisor's identity.
fn in_flight<A: Actor>(system: &Arc<ActorSystem>, sup: &ActorHandle<A>) -> u32 {
    super::identity_of(system, sup.stop_lane_ref().abort_record())
        .map_or(0, |identity| identity.restarts_in_flight())
}

async fn until_in_flight<A: Actor>(system: &Arc<ActorSystem>, sup: &ActorHandle<A>, want: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while in_flight(system, sup) != want {
        assert!(
            Instant::now() < deadline,
            "attempts in flight stayed at {}, wanted {want}",
            in_flight(system, sup)
        );
        sleep(Duration::from_millis(5)).await;
    }
}

/// A worker whose start the test can refuse or hold.
#[derive(Default)]
struct Probe {
    fail_start: bool,
    hold_start: Option<Arc<Notify>>,
}

impl Actor for Probe {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(gate) = &self.hold_start {
            gate.notified().await;
        }
        if self.fail_start {
            return Err(ActorError::user("the start is refused"));
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("failed on command"))
    }
}

/// What the instances of the child after the first do at their start.
#[derive(Clone)]
enum Later {
    Hold(Arc<Notify>),
    Refuse,
    /// Holds the start on the gate, then refuses it.
    HoldThenRefuse(Arc<Notify>),
}

/// A supervisor with one permanent child, named `c`.
struct Sup {
    later: Later,
}

impl Actor for Sup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let built = Arc::new(AtomicUsize::new(0));
        let later = self.later.clone();
        ctx.spawn_child(move || {
            if built.fetch_add(1, Ordering::SeqCst) == 0 {
                return Probe::default();
            }
            match &later {
                Later::Hold(gate) => Probe {
                    hold_start: Some(gate.clone()),
                    ..Probe::default()
                },
                Later::Refuse => Probe {
                    fail_start: true,
                    ..Probe::default()
                },
                Later::HoldThenRefuse(gate) => Probe {
                    fail_start: true,
                    hold_start: Some(gate.clone()),
                },
            }
        })
        .named("c")
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(50)))
        .kill_grace(Duration::from_millis(50))
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn spawned_of<'e>(events: &'e Events, id: &str) -> Vec<&'e TelemetryEvent<'static>> {
    events
        .iter()
        .filter(|event| {
            matches!(event, TelemetryEvent::ActorSpawned { actor, .. } if actor.id().as_str() == id)
        })
        .collect()
}

fn refused(events: &Events) -> Vec<(Option<String>, SpawnError)> {
    events
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::SpawnRefused { name, error } => {
                Some((name.as_ref().map(|n| n.to_string()), error.clone()))
            }
            _ => None,
        })
        .collect()
}

fn attempts(
    events: &Events,
) -> Vec<(
    &crate::telemetry::ActorRef,
    &crate::telemetry::ActorRef,
    &AttemptOutcome,
    Duration,
)> {
    events
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::RestartAttempt {
                supervisor,
                child,
                outcome,
                took,
            } => Some((supervisor, child, outcome, *took)),
            _ => None,
        })
        .collect()
}

/// `B` for a budget event, `D` for a death, `A` for an attempt, in order.
fn shape(events: &Events) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::RestartBudget { .. } => Some('B'),
            TelemetryEvent::ChildDied { .. } => Some('D'),
            TelemetryEvent::RestartAttempt { .. } => Some('A'),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Spawns: one event for an accepted spawn, one for a refused one
// ---------------------------------------------------------------------------

/// How a spawn asks for its answer.
#[derive(Clone, Copy, Debug)]
enum Ask {
    Await,
    Detached,
    Timed,
}

async fn run_spawn(rig: &Rig, name: &str, config: ActorConfig, ask: Ask) -> Result<(), SpawnError> {
    let builder = Probe::default()
        .spawn()
        .named(name)
        .on_system(&rig.system)
        .with_config(config);
    let builder = match ask {
        Ask::Await => builder,
        Ask::Detached => builder.detached(),
        Ask::Timed => builder.start_timeout(Duration::from_secs(5)),
    };
    builder.into_future().await.map(|_| ())
}

async fn spawn_events() {
    let rig = Rig::new("spawn");
    let config = ActorConfig::default()
        .with_mailbox_capacity(7)
        .with_priority_lane(3);
    run_spawn(&rig, "first", config.clone(), Ask::Await)
        .await
        .expect("the first spawn is accepted");
    let events = rig
        .until("the spawn event", |events| {
            !spawned_of(events, "first").is_empty()
        })
        .await;
    match spawned_of(&events, "first")[..] {
        [TelemetryEvent::ActorSpawned {
            actor,
            name,
            mailbox_capacity,
            priority_capacity,
        }] => {
            assert_eq!(name.as_deref(), Some("first"));
            assert_eq!((*mailbox_capacity, *priority_capacity), (7, Some(3)));
            assert_eq!(actor.ancestors().count(), 0, "a root has no parent");
        }
        ref other => panic!("one spawn event expected, got {other:#?}"),
    }

    // Each refusal gives one event, whichever way the spawn asks for its
    // answer, and no spawn event for the name that was refused.
    let rows = [
        (
            "first",
            ActorConfig::default(),
            Ask::Await,
            SpawnError::NameTaken {
                name: "first".into(),
                system: rig.system.name().to_string(),
            },
        ),
        (
            "first",
            ActorConfig::default(),
            Ask::Detached,
            SpawnError::NameTaken {
                name: "first".into(),
                system: rig.system.name().to_string(),
            },
        ),
        (
            "first",
            ActorConfig::default(),
            Ask::Timed,
            SpawnError::NameTaken {
                name: "first".into(),
                system: rig.system.name().to_string(),
            },
        ),
        (
            "zero-a",
            ActorConfig::default().with_mailbox_capacity(0),
            Ask::Await,
            SpawnError::ZeroMailboxCapacity,
        ),
        (
            "zero-t",
            ActorConfig::default().with_mailbox_capacity(0),
            Ask::Timed,
            SpawnError::ZeroMailboxCapacity,
        ),
    ];
    for (round, (name, config, ask, want)) in rows.into_iter().enumerate() {
        let got = run_spawn(&rig, name, config, ask).await;
        assert_eq!(
            got,
            Err(want.clone()),
            "row {round} {ask:?}: the caller sees the refusal"
        );
        let events = rig
            .settled("the refusal event", |events| refused(events).len() > round)
            .await;
        let all = refused(&events);
        assert_eq!(
            all.len(),
            round + 1,
            "row {round} {ask:?}: exactly one event per refusal: {all:#?}"
        );
        assert_eq!(
            all[round],
            (Some(name.to_string()), want),
            "row {round} {ask:?}"
        );
        assert_eq!(
            spawned_of(&events, name).len(),
            usize::from(name == "first"),
            "row {round}: a refused name has no spawn event"
        );
    }
}

/// A child's spawn event names its supervisor as the parent.
async fn child_spawn_event() {
    let rig = Rig::new("child");
    let gate = Arc::new(Notify::new());
    let sup = Sup {
        later: Later::Hold(gate),
    }
    .spawn()
    .named("sup")
    .on_system(&rig.system)
    .with_supervision(SupervisionConfig::one_for_one())
    .await
    .expect("the supervisor starts");
    let events = rig
        .until("the child's spawn event", |events| {
            !spawned_of(events, "c").is_empty()
        })
        .await;
    let TelemetryEvent::ActorSpawned { actor, .. } = spawned_of(&events, "c")[0] else {
        unreachable!()
    };
    let parent: Vec<_> = actor.ancestors().map(|(id, _)| id.to_string()).collect();
    assert_eq!(parent, vec![sup.id().to_string()]);
}

// ---------------------------------------------------------------------------
// Restart attempts
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Row {
    /// The restarted child starts: adopted.
    Adopted,
    /// The restarted child refuses its start: failed.
    Failed,
    /// The supervisor is stopped while the attempt holds the start: abandoned.
    Abandoned,
    /// The supervisor is killed while the attempt holds the start: its state
    /// drops with the ticket unsettled, and the drop reports the attempt once
    /// as abandoned, the count having fallen first.
    Dropped,
}

async fn attempt_row(row: Row) {
    let rig = Rig::new("attempt");
    let gate = Arc::new(Notify::new());
    let later = match row {
        Row::Failed => Later::Refuse,
        _ => Later::Hold(gate.clone()),
    };
    let sup = Sup { later }
        .spawn()
        .named("sup")
        .on_system(&rig.system)
        .with_supervision(SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)))
        .await
        .expect("the supervisor starts");
    assert_eq!(
        in_flight(&rig.system, &sup),
        0,
        "{row:?}: nothing in flight at rest"
    );
    let child = loop {
        if let Some(handle) = rig.system.get::<Probe>("c") {
            break handle;
        }
        sleep(Duration::from_millis(5)).await;
    };
    child
        .notify(())
        .await
        .expect("the child takes the order to fail");

    match row {
        Row::Adopted | Row::Abandoned | Row::Dropped => {
            // The second instance exists and holds its start: the attempt is
            // in flight, counted once, and has no event yet.
            let events = rig
                .until("the second instance", |events| {
                    spawned_of(events, "c").len() == 2
                })
                .await;
            until_in_flight(&rig.system, &sup, 1).await;
            assert!(
                attempts(&events).is_empty(),
                "{row:?}: no event before the outcome"
            );
            match row {
                Row::Adopted => gate.notify_one(),
                Row::Abandoned => sup
                    .stop(StopReason::Graceful)
                    .await
                    .expect("the stop is taken"),
                _ => sup.stop(StopReason::Kill).await.expect("the stop is taken"),
            }
        }
        Row::Failed => {}
    }

    let events = match row {
        // The ticket is dropped with the supervisor's state, which may come
        // after its end: wait for the event, then for the stream to settle, so
        // a second report would show.
        Row::Dropped => {
            rig.settled("the attempt event", |events| !attempts(events).is_empty())
                .await
        }
        _ => {
            rig.until("the attempt event", |events| !attempts(events).is_empty())
                .await
        }
    };
    // Ordered by observation: the count is already down when the event is
    // seen.
    assert_eq!(
        in_flight(&rig.system, &sup),
        0,
        "{row:?}: the count falls before the event"
    );
    let all = attempts(&events);
    {
        {
            let (supervisor, child, outcome, _took) = all[0];
            assert_eq!(all.len(), 1, "{row:?}: one event per attempt");
            assert_eq!(supervisor.id().as_str(), "sup");
            assert_eq!(child.id().as_str(), "c");
            let spawned = spawned_of(&events, "c");
            let TelemetryEvent::ActorSpawned { actor: second, .. } = spawned[1] else {
                unreachable!()
            };
            assert_eq!(
                child.incarnation(),
                second.incarnation(),
                "{row:?}: the event names the attempt's incarnation, not the dead one's"
            );
            match (row, outcome) {
                (Row::Adopted, AttemptOutcome::Adopted)
                | (Row::Abandoned | Row::Dropped, AttemptOutcome::Abandoned) => {}
                (Row::Failed, AttemptOutcome::Failed { reason }) => {
                    assert!(
                        matches!(reason, StopReason::Failure(ActorError::User(_))),
                        "{reason:?}"
                    );
                }
                (row, outcome) => panic!("{row:?} gave {outcome:?}"),
            }
        }
    }
    if matches!(row, Row::Adopted | Row::Failed) {
        assert!(
            shape(&events).starts_with("BDA"),
            "{row:?}: the attempt event follows the budget and the death of its restart: {}",
            shape(&events)
        );
    }
}

async fn attempt_events() {
    for row in [Row::Adopted, Row::Failed, Row::Abandoned, Row::Dropped] {
        attempt_row(row).await;
    }
}

macro_rules! both_flavors {
    ($current:ident, $multi:ident, $scenario:ident) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $current() {
            $scenario().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $multi() {
            $scenario().await;
        }
    };
}

// ---------------------------------------------------------------------------
// An invalid supervision configuration, as a root and as a child
// ---------------------------------------------------------------------------

/// A configuration the spawn refuses: `auto_shutdown` under
/// `SimpleOneForOne`.
fn invalid_config() -> ActorConfig {
    ActorConfig::default().with_supervision(
        SupervisionConfig::simple_one_for_one().with_auto_shutdown(AutoShutdown::AnySignificant),
    )
}

/// A supervisor that makes four child spawns when it starts and keeps what
/// each answered.
struct Refuser {
    answers: Arc<Mutex<Vec<Result<(), SpawnError>>>>,
}

impl Actor for Refuser {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let mut answers = Vec::new();
        let named = ["kid", "kid", "significant-kid", "bad-kid"];
        for (round, name) in named.into_iter().enumerate() {
            let builder = ctx.spawn_child(Probe::default).named(name);
            let answer = match round {
                2 => builder.significant(true).await,
                3 => builder.with_config(invalid_config()).await,
                _ => builder.await,
            };
            answers.push(answer.map(|_| ()));
        }
        *self.answers.lock().unwrap() = answers;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// An invalid supervision configuration is refused with one event whether
/// the spawn is a root or a child. Two child refusals raise none: a name held
/// by the supervisor's own child specs (`DuplicateChild`) and `significant`
/// under `auto_shutdown` `Never`. A name taken in the system registry raises
/// one event, for a root and for a child alike.
async fn an_invalid_supervision_is_refused_with_one_event_for_a_root_and_a_child() {
    let rig = Rig::new("invalid");
    let want = SpawnError::InvalidSupervision("auto_shutdown with simple_one_for_one");
    let root = run_spawn(&rig, "bad-root", invalid_config(), Ask::Await).await;
    assert_eq!(root, Err(want.clone()));

    let answers = Arc::new(Mutex::new(Vec::new()));
    Refuser {
        answers: answers.clone(),
    }
    .spawn()
    .named("refuser")
    .on_system(&rig.system)
    .with_supervision(SupervisionConfig::one_for_one())
    .await
    .expect("the supervisor starts");
    let answers = answers.lock().unwrap().clone();
    assert_eq!(answers.len(), 4);
    assert_eq!(answers[0], Ok(()));
    assert!(matches!(answers[1], Err(SpawnError::DuplicateChild(_))));
    assert!(matches!(answers[2], Err(SpawnError::InvalidSupervision(_))));
    assert_eq!(answers[3], Err(want.clone()));

    let events = rig
        .settled("the refusal events", |events| refused(events).len() >= 2)
        .await;
    assert_eq!(
        refused(&events),
        vec![
            (Some("bad-root".to_string()), want.clone()),
            (Some("bad-kid".to_string()), want),
        ],
        "one event for the root and one for the child, none for the other child refusals"
    );
}

both_flavors!(
    invalid_supervision_event_current_thread,
    invalid_supervision_event_multi_thread,
    an_invalid_supervision_is_refused_with_one_event_for_a_root_and_a_child
);

// ---------------------------------------------------------------------------
// A supervisor that ends with an attempt that reached no outcome
// ---------------------------------------------------------------------------

/// Opens once, for a factory that runs on a worker thread and cannot await.
#[derive(Default)]
struct ThreadGate {
    open: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
}

impl ThreadGate {
    fn wait(&self) {
        let guard = self.open.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(30), |open| !*open)
            .unwrap();
    }

    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

/// Opens its gate when dropped, so a failed assertion never leaves a factory
/// holding a thread.
struct OpenOnDrop(Arc<ThreadGate>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// A supervisor with one permanent child, named `c`, whose factory, from its
/// second call, holds a worker thread on a gate: the restart attempt cannot
/// finish and cannot be cut.
struct Stuck {
    gate: Arc<ThreadGate>,
    inside: Arc<std::sync::atomic::AtomicBool>,
}

impl Actor for Stuck {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let built = Arc::new(AtomicUsize::new(0));
        let (gate, inside) = (self.gate.clone(), self.inside.clone());
        ctx.spawn_child(move || {
            if built.fetch_add(1, Ordering::SeqCst) > 0 {
                inside.store(true, Ordering::SeqCst);
                gate.wait();
            }
            Probe::default()
        })
        .named("c")
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(50)))
        .kill_grace(Duration::from_millis(50))
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Whether the events hold the end of the supervisor `sup`.
fn ended_sup(events: &Events) -> bool {
    events.iter().any(|event| {
        matches!(event, TelemetryEvent::ActorEnded { actor, .. } if actor.id().as_str() == "sup")
    })
}

/// A supervisor stopped while its restart attempt holds a thread ends with
/// the attempt unfinished and its ticket kept: the final collection reports
/// the attempt once, as abandoned, ahead of the supervisor's end event, while
/// the factory still holds its thread.
///
/// Multi-thread only: the factory holds a worker thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_supervisor_that_ends_with_an_attempt_in_flight_reports_it_before_its_end() {
    let name = format!("life-unsettled-{}", SEQ.fetch_add(1, Ordering::Relaxed));
    let system = ActorSystem::create(name).expect("a fresh system name");
    let sink = system.telemetry_mode().collect(KindSet::of(&[
        EventKind::RestartAttempt,
        EventKind::ActorEnded,
    ]));
    let gate = Arc::new(ThreadGate::default());
    let _open = OpenOnDrop(gate.clone());
    let inside = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sup = Stuck {
        gate: gate.clone(),
        inside: inside.clone(),
    }
    .spawn()
    .named("sup")
    .on_system(&system)
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(3, Duration::from_secs(60)))
    .await
    .expect("the supervisor starts");
    let child = loop {
        if let Some(handle) = system.get::<Probe>("c") {
            break handle;
        }
        sleep(Duration::from_millis(5)).await;
    };
    child.notify(()).await.expect("the child takes the order");
    let deadline = Instant::now() + Duration::from_secs(25);
    while !inside.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the restart never began");
        sleep(Duration::from_millis(5)).await;
    }
    until_in_flight(&system, &sup, 1).await;

    sup.stop(StopReason::Graceful)
        .await
        .expect("the stop is taken");
    // The final collection reports the attempt while the factory still holds
    // its thread; the supervisor's teardown then waits for that factory, so
    // the supervisor has not ended.
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let events = sink.events();
        if !attempts(&events).is_empty() {
            assert!(
                !ended_sup(&events),
                "the supervisor ended while the factory holds its thread: {events:#?}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the attempt was never reported; events: {events:#?}"
        );
        sleep(Duration::from_millis(5)).await;
    }

    gate.open();
    let deadline = Instant::now() + Duration::from_secs(25);
    let events = loop {
        let events = sink.events();
        if ended_sup(&events) {
            break events;
        }
        assert!(
            Instant::now() < deadline,
            "the supervisor never ended; events: {events:#?}"
        );
        sleep(Duration::from_millis(5)).await;
    };
    let order: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::RestartAttempt {
                outcome: AttemptOutcome::Abandoned,
                ..
            } => Some("attempt"),
            TelemetryEvent::RestartAttempt { .. } => Some("other attempt"),
            TelemetryEvent::ActorEnded { actor, .. } if actor.id().as_str() == "sup" => Some("end"),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        vec!["attempt", "end"],
        "one abandoned attempt, then the supervisor's end: {events:#?}"
    );
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        attempts(&sink.events()).len(),
        1,
        "the attempt is reported once, whatever happens to the factory"
    );
    assert_eq!(in_flight(&system, &sup), 0);
}

// ---------------------------------------------------------------------------
// An attempt that failed while its supervisor was busy
// ---------------------------------------------------------------------------

/// A supervisor like [`Sup`] whose `handle` holds its turn on a gate.
struct HeldSup {
    later: Later,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for HeldSup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let built = Arc::new(AtomicUsize::new(0));
        let later = self.later.clone();
        ctx.spawn_child(move || {
            if built.fetch_add(1, Ordering::SeqCst) == 0 {
                return Probe::default();
            }
            match &later {
                Later::HoldThenRefuse(gate) => Probe {
                    fail_start: true,
                    hold_start: Some(gate.clone()),
                },
                _ => Probe::default(),
            }
        })
        .named("c")
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(50)))
        .kill_grace(Duration::from_millis(50))
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

/// A restart attempt whose child refused its start while the supervisor was
/// inside a turn is collected when the supervisor ends, and it is reported as
/// the failure it was, not as an attempt the supervisor abandoned.
async fn a_failed_attempt_collected_at_the_supervisors_end_reports_its_failure() {
    let rig = Rig::new("collected");
    let start = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let sup = HeldSup {
        later: Later::HoldThenRefuse(start.clone()),
        entered: entered.clone(),
        release: release.clone(),
    }
    .spawn()
    .named("sup")
    .on_system(&rig.system)
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(3, Duration::from_secs(60)))
    .await
    .expect("the supervisor starts");
    let child = loop {
        if let Some(handle) = rig.system.get::<Probe>("c") {
            break handle;
        }
        sleep(Duration::from_millis(5)).await;
    };
    child.notify(()).await.expect("the child takes the order");
    rig.until("the second instance", |events| {
        spawned_of(events, "c").len() == 2
    })
    .await;
    until_in_flight(&rig.system, &sup, 1).await;
    // The attempt's child holds the name while its start is held.
    let attempt_child = rig
        .system
        .get::<Probe>("c")
        .expect("the attempt's child holds its name");

    // The supervisor takes a turn that holds, the attempt's child then
    // refuses its start, and the supervisor is stopped inside the turn.
    sup.notify(()).await.expect("the order is taken");
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("the supervisor's turn begins");
    start.notify_one();
    rig.until("the attempt's child to end", |events| {
        ended_of(events, "c") == 2
    })
    .await;
    // The end event comes before the child publishes `Stopped`, and the
    // supervisor's final collection takes an attempt's outcome only for a
    // child that has published it. The refusal this test pins is one that
    // completed, so the stop waits for the child's last status.
    tokio::time::timeout(Duration::from_secs(10), attempt_child.wait_stopped())
        .await
        .expect("the attempt's child publishes its end");
    sup.stop(StopReason::Graceful)
        .await
        .expect("the stop is taken");
    release.notify_one();

    let events = rig
        .until("the attempt event", |events| !attempts(events).is_empty())
        .await;
    let all = attempts(&events);
    assert_eq!(all.len(), 1, "one event for the attempt: {events:#?}");
    assert!(
        matches!(
            all[0].2,
            AttemptOutcome::Failed {
                reason: StopReason::Failure(ActorError::User(_))
            }
        ),
        "the failure the attempt ended with: {:?}",
        all[0].2
    );
}

fn ended_of(events: &Events, id: &str) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(event, TelemetryEvent::ActorEnded { actor, .. } if actor.id().as_str() == id)
        })
        .count()
}

// ---------------------------------------------------------------------------
// A child's own end, collected before or after the child published it
// ---------------------------------------------------------------------------

/// How the child ends on its own.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Case {
    /// The restart attempt's child refuses its start.
    Refused,
    /// The attempt's start timeout cuts its child's start, which then
    /// returns.
    TimedOut,
    /// The first incarnation fails on a command while it runs.
    Failed,
    /// The attempt's child starts, then fails on a command.
    AckedFailed,
}

/// How far the child has ended when its supervisor collects it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Found {
    /// The child's reason is written and its value still blocks in its drop.
    Dropping,
    /// The child published its end.
    Ended,
    /// As `Dropping`, and the drop then finishes and the child publishes its
    /// end while the collection is still running.
    DroppingThenEnded,
    /// As `Dropping`, with a short `Shutdown` for the child and a drop that
    /// outlasts the whole teardown: nothing opens the gate while the
    /// supervisor stops.
    Ignoring,
}

/// The `Shutdown` of a child that ignores its supervisor's stop, and the
/// factor of it the supervisor may take to end.
const IGNORING_SHUTDOWN: Duration = Duration::from_millis(500);
const IGNORING_FACTOR: u32 = 10;

/// A worker whose start the test refuses or holds, and whose value can block
/// a thread in its drop.
struct Dropper {
    refuse: bool,
    hold_start: Option<Arc<Notify>>,
    drop_gate: Option<Arc<ThreadGate>>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Dropper {
    fn drop(&mut self) {
        if let Some(gate) = &self.drop_gate {
            gate.wait();
        }
        self.dropped.store(true, Ordering::SeqCst);
    }
}

impl Actor for Dropper {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(gate) = &self.hold_start {
            gate.notified().await;
        }
        if self.refuse {
            return Err(ActorError::user("the start is refused"));
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("failed on command"))
    }
}

/// One final delivery: the child, the reason, the action, and whether the
/// child had finished its drop when it ran.
type Delivery = (String, StopReason, crate::types::SupervisionAction, bool);

/// A supervisor of one permanent child, named `child`, whose turn holds on
/// `release` and which records its final deliveries. The incarnation that
/// ends on its own (the first for `Case::Failed`, the restart attempt's
/// otherwise) carries the drop gate.
struct Collector {
    child: String,
    case: Case,
    /// Whether the child has the short `Shutdown` of a child that ignores
    /// its stop.
    bounded: bool,
    start: Arc<Notify>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    drop_gate: Option<Arc<ThreadGate>>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
    deliveries: Arc<std::sync::Mutex<Vec<Delivery>>>,
}

impl Actor for Collector {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let built = Arc::new(AtomicUsize::new(0));
        let (start, gate, dropped, case) = (
            self.start.clone(),
            self.drop_gate.clone(),
            self.dropped.clone(),
            self.case,
        );
        let mut child = ctx
            .spawn_child(move || {
                if built.fetch_add(1, Ordering::SeqCst) == 0 {
                    let own = case == Case::Failed;
                    return Dropper {
                        refuse: false,
                        hold_start: None,
                        drop_gate: if own { gate.clone() } else { None },
                        dropped: if own { dropped.clone() } else { Arc::default() },
                    };
                }
                Dropper {
                    refuse: case == Case::Refused,
                    hold_start: Some(start.clone()),
                    drop_gate: gate.clone(),
                    dropped: dropped.clone(),
                }
            })
            .named(self.child.clone())
            .restart_type(RestartType::Permanent);
        child = if self.bounded {
            child
                .shutdown(Shutdown::Timeout(IGNORING_SHUTDOWN))
                .kill_grace(Duration::from_millis(100))
        } else {
            // No step of the attempt's ladder comes due during the test.
            child.kill_grace(Duration::from_secs(30))
        };
        if self.case == Case::TimedOut {
            child = child.start_timeout(Duration::from_millis(20));
        }
        child.await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &crate::types::ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.deliveries.lock().unwrap().push((
            event.child_id.to_string(),
            event.reason.clone(),
            event.action,
            self.dropped.load(Ordering::SeqCst),
        ));
        Ok(())
    }
}

/// The events about the incarnation `incarnation` of `child`, in order.
fn trace_of(events: &Events, child: &str, incarnation: u64) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::ActorEnded { actor, reason, .. }
                if actor.id().as_str() == child && actor.incarnation() == incarnation =>
            {
                Some(format!("ended {reason:?}"))
            }
            TelemetryEvent::RestartAttempt {
                child: of, outcome, ..
            } if of.id().as_str() == child && of.incarnation() == incarnation => {
                Some(format!("attempt {outcome:?}"))
            }
            TelemetryEvent::ChildDied {
                child: of,
                reason,
                decision,
                ..
            } if of.id().as_str() == child && of.incarnation() == incarnation => {
                Some(format!("died {reason:?} {decision:?}"))
            }
            _ => None,
        })
        .collect()
}

/// A child's own end has one outcome when its supervisor is stopped inside a
/// turn, whether the final collection finds the child still dropping its
/// value or already ended: the same end event, attempt event, death event
/// and final delivery, for an attempt whose child's start failed (refused,
/// or cut by the start timeout), for a running child, and for an attempt
/// whose child started and then failed, also when the attempt has not read
/// its child's acknowledgment when the supervisor stops (`unread`). The
/// collection waits for no drop: the delivery runs before the child's value
/// finished dropping. A child whose drop finishes while the collection runs
/// is reported once. The supervisor ends only after the child's task ended.
async fn an_own_end_has_one_outcome(case: Case, found: Found, unread: bool) {
    let row = format!("{case:?}, {found:?}, unread {unread}");
    let rig = Rig::new("one-outcome");
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    // Names of their own: the end hooks are keyed by id across the process.
    let name = format!("c-one-outcome-{seq}");
    let (start, entered, release) = (
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
    );
    let gate = (found != Found::Ended).then(|| Arc::new(ThreadGate::default()));
    let _open = gate.clone().map(OpenOnDrop);
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let deliveries: Arc<std::sync::Mutex<Vec<Delivery>>> = Arc::default();
    let sup = Collector {
        child: name.clone(),
        case,
        bounded: found == Found::Ignoring,
        start: start.clone(),
        entered: entered.clone(),
        release: release.clone(),
        drop_gate: gate.clone(),
        dropped: dropped.clone(),
        deliveries: deliveries.clone(),
    }
    .spawn()
    .named(format!("sup-one-outcome-{seq}"))
    .on_system(&rig.system)
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(3, Duration::from_secs(60)))
    .await
    .expect("the supervisor starts");
    let first = loop {
        if let Some(handle) = rig.system.get::<Dropper>(&name) {
            break handle;
        }
        sleep(Duration::from_millis(5)).await;
    };
    // The attempt takes its child's acknowledgment only once the final
    // collection waits for it.
    let ack_hold = unread.then(|| Arc::new(Notify::new()));
    let (child, incarnation) = if case == Case::Failed {
        let events = rig
            .until("the child", |events| spawned_of(events, &name).len() == 1)
            .await;
        let TelemetryEvent::ActorSpawned { actor, .. } = spawned_of(&events, &name)[0] else {
            unreachable!()
        };
        let incarnation = actor.incarnation();
        // The supervisor's turn holds; the child then fails.
        sup.notify(()).await.expect("the order is taken");
        tokio::time::timeout(Duration::from_secs(10), entered.notified())
            .await
            .expect("the supervisor's turn begins");
        first.notify(()).await.expect("the child takes the order");
        (first, incarnation)
    } else {
        first.notify(()).await.expect("the child takes the order");
        let events = rig
            .until("the attempt's child", |events| {
                spawned_of(events, &name).len() == 2
            })
            .await;
        let TelemetryEvent::ActorSpawned { actor, .. } = spawned_of(&events, &name)[1] else {
            unreachable!()
        };
        let incarnation = actor.incarnation();
        until_in_flight(&rig.system, &sup, 1).await;
        let child = rig
            .system
            .get::<Dropper>(&name)
            .expect("the attempt's child holds its name");
        if let Some(ack_hold) = &ack_hold {
            end_hooks::attach_hold(end_hooks::ACK, child.id().clone(), ack_hold.clone());
            let release = ack_hold.clone();
            end_hooks::attach(end_hooks::ATTEMPT_WAIT, sup.id().clone(), move || {
                release.notify_one();
            });
        }

        // The supervisor's turn holds; the child's start then ends.
        sup.notify(()).await.expect("the order is taken");
        tokio::time::timeout(Duration::from_secs(10), entered.notified())
            .await
            .expect("the supervisor's turn begins");
        if case == Case::TimedOut {
            let deadline = Instant::now() + Duration::from_secs(10);
            while child.stop_lane_ref().reason() != Some(StopReason::Kill) {
                assert!(
                    Instant::now() < deadline,
                    "{row}: the start timeout never cut"
                );
                sleep(Duration::from_millis(5)).await;
            }
        }
        start.notify_one();
        if case == Case::AckedFailed {
            child.notify(()).await.expect("the child takes the order");
        }
        (child, incarnation)
    };
    // The child's final reason is what the collection reads. Its end event
    // can wait behind the drop: the wake of the event helper may sit on the
    // worker the drop blocks.
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.status_rx().borrow().reason.is_none() {
        assert!(Instant::now() < deadline, "{row}: the child never ended");
        sleep(Duration::from_millis(5)).await;
    }
    if found == Found::Ended {
        tokio::time::timeout(Duration::from_secs(10), child.wait_stopped())
            .await
            .expect("the child publishes its end");
    }

    // The teardown opens the gate once it waits for the child; the
    // supervisor's name is freed after that wait. For a drop that finishes
    // during the collection, the gate opens when the death is collected, and
    // the collection goes on once the child published its end.
    let freed_after_drop: Arc<std::sync::Mutex<Option<bool>>> = Arc::default();
    if let Some(gate) = gate.as_ref().filter(|_| found != Found::Ignoring) {
        if found == Found::DroppingThenEnded {
            let (opener, watched) = (gate.clone(), child.clone());
            end_hooks::attach(end_hooks::PLANE_REPORT, child.id().clone(), move || {
                opener.open();
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while watched.status() != crate::types::ActorStatus::Stopped
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
        }
        let opener = gate.clone();
        end_hooks::attach(end_hooks::REPORTED_WAIT, sup.id().clone(), move || {
            opener.open();
        });
        let (opener, seen, dropped) = (gate.clone(), freed_after_drop.clone(), dropped.clone());
        end_hooks::attach(end_hooks::NAME, sup.id().clone(), move || {
            *seen.lock().unwrap() = Some(dropped.load(Ordering::SeqCst));
            opener.open();
        });
    }
    sup.stop(StopReason::Graceful)
        .await
        .expect("the stop is taken");
    release.notify_one();

    if found == Found::Ignoring {
        // The teardown waits for the child at most as long as the child's own
        // `Shutdown` and its grace, though the drop is still blocked: the gate
        // opens only when the row ends.
        tokio::time::timeout(IGNORING_SHUTDOWN * IGNORING_FACTOR, sup.wait_stopped())
            .await
            .unwrap_or_else(|_| {
                panic!("{row}: the supervisor waited on a child that ignores its stop")
            });
        assert!(
            !dropped.load(Ordering::SeqCst),
            "{row}: the child's drop was still blocked when the supervisor ended"
        );
        return;
    }

    let failed = StopReason::Failure(ActorError::user("failed on command"));
    let (ended, attempt, reason) = match case {
        Case::Refused => {
            let refused = StopReason::Failure(ActorError::user("the start is refused"));
            (
                refused.clone(),
                Some(AttemptOutcome::Failed {
                    reason: refused.clone(),
                }),
                refused,
            )
        }
        Case::TimedOut => {
            let timed_out = StopReason::Failure(ActorError::Spawn(SpawnError::StartTimeout));
            (
                StopReason::Kill,
                Some(AttemptOutcome::Failed {
                    reason: timed_out.clone(),
                }),
                timed_out,
            )
        }
        Case::Failed => (failed.clone(), None, failed),
        Case::AckedFailed => (failed.clone(), Some(AttemptOutcome::Adopted), failed),
    };
    let mut want = vec![format!("ended {ended:?}")];
    if let Some(attempt) = &attempt {
        want.push(format!("attempt {attempt:?}"));
    }
    want.push(format!(
        "died {reason:?} {:?}",
        crate::types::SupervisionAction::SupervisorStopping
    ));
    if attempt.is_some() {
        let events = rig
            .until(&format!("{row}: the attempt event"), |events| {
                trace_of(events, &name, incarnation).len() >= 2
            })
            .await;
        assert_eq!(
            trace_of(&events, &name, incarnation).get(1),
            want.get(1),
            "{row}: the attempt's outcome"
        );
    }
    let finals = || -> Vec<Delivery> {
        deliveries
            .lock()
            .unwrap()
            .iter()
            .filter(|delivery| delivery.2 == crate::types::SupervisionAction::SupervisorStopping)
            .cloned()
            .collect()
    };
    let want_finals = vec![(
        name.clone(),
        reason,
        crate::types::SupervisionAction::SupervisorStopping,
        found != Found::Dropping,
    )];
    let deadline = Instant::now() + Duration::from_secs(10);
    while finals().is_empty() {
        assert!(Instant::now() < deadline, "{row}: no final delivery");
        sleep(Duration::from_millis(5)).await;
    }
    tokio::time::timeout(Duration::from_secs(10), sup.wait_stopped())
        .await
        .expect("the supervisor ends");
    // Read once the supervisor ended: the child's task ended before it, so
    // a second report of the same death would be here.
    let events = rig
        .settled(&format!("{row}: the death event"), |events| {
            trace_of(events, &name, incarnation).len() >= want.len()
        })
        .await;
    assert_eq!(
        trace_of(&events, &name, incarnation),
        want,
        "{row}: the trace"
    );
    assert_eq!(
        finals(),
        want_finals,
        "{row}: one final delivery, made before the child's drop finished when it was dropping"
    );
    if gate.is_some() {
        assert_eq!(
            *freed_after_drop.lock().unwrap(),
            Some(true),
            "{row}: the supervisor freed its name only after the child's task ended"
        );
    }
    // An end the final collection took from its plane is the child's own, and
    // its crash report stands: the teardown logs no unexpected end for it.
    #[cfg(feature = "tracing")]
    {
        let token = format!("child={name} ");
        let unexpected: Vec<String> = errors::lines()
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(&token) && line.contains("ended unexpectedly"))
            .cloned()
            .collect();
        assert!(
            unexpected.is_empty(),
            "{row}: a collected end is logged as unexpected: {unexpected:?}"
        );
    }
}

/// The error lines of the runtimes a row runs on, kept by a subscriber of
/// each thread of the runtime: the lines are written on whichever worker the
/// supervisor's task runs.
#[cfg(feature = "tracing")]
mod errors {
    use std::cell::RefCell;
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex, OnceLock};

    use tracing::field::{Field, Visit};
    use tracing::subscriber::DefaultGuard;
    use tracing::{span, Event, Level, Metadata, Subscriber};

    type Lines = Arc<Mutex<Vec<String>>>;

    /// Every line written so far on the threads that installed the subscriber.
    pub(super) fn lines() -> &'static Lines {
        static LINES: OnceLock<Lines> = OnceLock::new();
        LINES.get_or_init(Lines::default)
    }

    thread_local! {
        static INSTALLED: RefCell<Option<DefaultGuard>> = const { RefCell::new(None) };
    }

    /// Keeps the error lines of the calling thread until the thread ends.
    pub(super) fn install_on_this_thread() {
        let guard = crate::tracing_isolation::install(Errors {
            lines: lines().clone(),
        });
        INSTALLED.with(|slot| *slot.borrow_mut() = Some(guard));
    }

    struct Errors {
        lines: Lines,
    }

    struct Text(String);

    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    impl Subscriber for Errors {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == Level::ERROR
        }

        fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut text = Text(String::new());
            event.record(&mut text);
            self.lines.lock().unwrap().push(text.0);
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }
}

/// Every row: the drop that blocks a thread needs a second worker. With the
/// `tracing` feature each worker keeps the error lines written on it.
#[test]
fn an_own_end_has_one_outcome_multi_thread() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();
    #[cfg(feature = "tracing")]
    builder.on_thread_start(errors::install_on_this_thread);
    builder.build().unwrap().block_on(async {
        for case in [
            Case::Refused,
            Case::TimedOut,
            Case::Failed,
            Case::AckedFailed,
        ] {
            for found in [Found::Dropping, Found::Ended] {
                an_own_end_has_one_outcome(case, found, false).await;
            }
        }
        an_own_end_has_one_outcome(Case::Failed, Found::DroppingThenEnded, false).await;
        an_own_end_has_one_outcome(Case::Failed, Found::Ignoring, false).await;
        for found in [Found::Dropping, Found::Ended] {
            an_own_end_has_one_outcome(Case::AckedFailed, found, true).await;
        }
    });
}
