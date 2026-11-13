//! Turn events, run events and the start event: when the clock is read, where
//! a handler runs, and what each event carries.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use super::{ActorRef, EventKind, HookKind, KindSet, TelemetryEvent, TurnOutcome};
use crate::actor::context::ActorContext;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult};
use crate::system::ActorSystem;
use crate::types::{ActorId, StopReason};

/// The longest a wait for a condition may take; it only limits how long a
/// real hang takes to report.
const BOUND: Duration = Duration::from_secs(60);

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

pub(super) fn unique(base: &str) -> String {
    format!(
        "turns-{base}-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

/// Runs `body` on a multi-thread runtime and on a current-thread runtime.
pub(super) fn both_flavors<F, Fut>(body: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for multi in [true, false] {
        let runtime = if multi {
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
        runtime.block_on(body());
    }
}

/// Whether the pump holds an event about the actor behind `handle`.
fn pumped<A: Actor>(sys: &ActorSystem, handle: &crate::ActorHandle<A>) -> bool {
    super::identity_of(sys, handle.stop_lane().abort_record())
        .is_some_and(|ident| ident.has_pumped())
}

/// Waits until `done` holds, or fails after the bound.
pub(super) async fn until(what: &str, done: impl Fn() -> bool) {
    until_showing(what, String::new, done).await;
}

/// [`until`], and a failure after the bound also prints what `state` reads at
/// that moment, so a wait that never ends says where the system stands.
pub(super) async fn until_showing(what: &str, state: impl Fn() -> String, done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + BOUND;
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out: {what}\n{}",
            state()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[derive(Clone, Copy, Debug)]
enum Msg {
    Ok,
    Err,
    Panic,
}

/// The tasks `handle` ran on, in message order.
type Tasks = Arc<Mutex<Vec<tokio::task::Id>>>;

/// An actor whose hooks do what the test says.
#[derive(Default)]
struct Probe {
    tasks: Tasks,
    /// `pre_start` waits for this before it returns.
    gate: Option<Arc<Notify>>,
    /// The first `handle` waits for this after it noted its task.
    first_turn_gate: Option<Arc<Notify>>,
    on_started_sleep: Duration,
    fail_pre_start: bool,
    fail_on_started: bool,
}

impl Actor for Probe {
    type Message = Msg;
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        if self.fail_pre_start {
            return Err(ActorError::user("no start"));
        }
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        tokio::time::sleep(self.on_started_sleep).await;
        if self.fail_on_started {
            return Err(ActorError::user("no start"));
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.tasks.lock().unwrap().push(tokio::task::id());
        if let Some(gate) = self.first_turn_gate.take() {
            gate.notified().await;
        }
        match msg {
            Msg::Ok => Ok(()),
            Msg::Err => Err(ActorError::user("failed turn")),
            Msg::Panic => panic!("panicked turn"),
        }
    }
}

/// What a handler saw of one event.
#[derive(Clone, Debug)]
struct Got {
    kind: EventKind,
    /// The task the handler ran on.
    task: Option<tokio::task::Id>,
    actor: ActorRef,
    hook: Option<HookKind>,
    outcome: Option<TurnOutcome>,
    takes: u32,
    took: Duration,
}

type Log = Arc<Mutex<Vec<Got>>>;

/// Attaches a handler under `id` that logs events of `kinds`.
fn log_events(sys: &ActorSystem, id: &str, kinds: KindSet) -> Log {
    let log: Log = Arc::default();
    let sink = log.clone();
    assert!(sys.attach_telemetry(id, kinds, move |event| {
        let task = tokio::task::try_id();
        let got = match event {
            TelemetryEvent::TurnFinished {
                actor,
                hook,
                took,
                outcome,
            } => Got {
                kind: EventKind::TurnFinished,
                task,
                actor: actor.as_ref().clone(),
                hook: Some(*hook),
                outcome: Some(*outcome),
                takes: 0,
                took: *took,
            },
            TelemetryEvent::RunFinished { actor, takes, took } => Got {
                kind: EventKind::RunFinished,
                task,
                actor: actor.as_ref().clone(),
                hook: None,
                outcome: None,
                takes: *takes,
                took: *took,
            },
            TelemetryEvent::ActorStarted { actor, took } => Got {
                kind: EventKind::ActorStarted,
                task,
                actor: actor.clone(),
                hook: None,
                outcome: None,
                takes: 0,
                took: *took,
            },
            _ => return,
        };
        sink.lock().unwrap().push(got);
    }));
    log
}

fn count(log: &Log, keep: impl Fn(&Got) -> bool) -> usize {
    log.lock().unwrap().iter().filter(|got| keep(got)).count()
}

fn turn_of(log: &Log, hook: HookKind, outcome: TurnOutcome) -> Option<Got> {
    log.lock()
        .unwrap()
        .iter()
        .find(|got| got.hook == Some(hook) && got.outcome == Some(outcome))
        .cloned()
}

#[derive(Clone, Copy, Debug)]
enum Armed {
    Nothing,
    OnlyAColdKind,
}

#[test]
fn a_turn_that_goes_on_reaches_the_handler_on_the_actors_task_and_a_failed_one_does_not() {
    for (failing, outcome) in [
        (Msg::Err, TurnOutcome::Failed),
        (Msg::Panic, TurnOutcome::Panicked),
    ] {
        both_flavors(|| async move {
            let sys = ActorSystem::create(unique("tasks")).unwrap();
            let log = log_events(&sys, "turns", KindSet::of(&[EventKind::TurnFinished]));
            let probe = Probe::default();
            let tasks = probe.tasks.clone();
            let handle = probe.spawn().on_system(&sys).await.unwrap();
            // A hot event runs on the actor's task only while the pump holds
            // nothing, so the start hooks' events are delivered first.
            until("the start events are delivered", || !pumped(&sys, &handle)).await;
            handle.send(Msg::Ok).await.unwrap();
            let _ = handle.send(failing).await;
            // Two start hooks, two turns of `handle`, one `on_stopped`.
            until("five turn events", || count(&log, |_| true) == 5).await;

            let actor_task = tasks.lock().unwrap()[0];
            let went_on = turn_of(&log, HookKind::Handle, TurnOutcome::Returned).unwrap();
            assert_eq!(
                went_on.task,
                Some(actor_task),
                "{failing:?}: the turn that went on runs its handler on the actor's task"
            );
            let failed = turn_of(&log, HookKind::Handle, outcome).unwrap();
            assert!(
                failed.task.is_some() && failed.task != Some(actor_task),
                "{failing:?}: a failed turn's handler runs off the actor's task"
            );
            for hook in [HookKind::PreStart, HookKind::OnStarted, HookKind::OnStopped] {
                let got = turn_of(&log, hook, TurnOutcome::Returned).unwrap();
                assert!(
                    got.task.is_some() && got.task != Some(actor_task),
                    "{failing:?}: {hook:?} runs its handler off the actor's task"
                );
            }
        });
    }
}

#[test]
fn a_hot_turn_waits_behind_the_pumps_backlog_and_runs_on_the_actors_task_once_it_is_empty() {
    both_flavors(|| async {
        let sys = ActorSystem::create(unique("order")).unwrap();
        let mode = sys.telemetry_mode().clone();
        let log = log_events(
            &sys,
            "order",
            KindSet::of(&[EventKind::ActorStarted, EventKind::TurnFinished]),
        );
        let probe = Probe::default();
        let tasks = probe.tasks.clone();

        // The pump holds a backlog: nothing it has queued is delivered yet.
        mode.pump.stall_for_test();
        let handle = probe.spawn().on_system(&sys).await.unwrap();
        handle.send(Msg::Ok).await.unwrap();
        assert_eq!(tasks.lock().unwrap().len(), 1, "the message was handled");
        // The turn's event is raised after its reply: it is queued once the
        // pump has taken the start event, two start hooks and this turn.
        until("the handled turn is queued", || {
            mode.pump.pushed_for_test() >= 4
        })
        .await;
        assert_eq!(
            count(&log, |_| true),
            0,
            "the first handled turn reached a handler ahead of the start events the pump holds"
        );

        // Released, the pump delivers the start events and then the turn.
        mode.pump.unstall_for_test();
        super::pump::Pump::ensure_started(&mode);
        // Two start hooks, the start event, the handled turn.
        until("the backlog is delivered", || count(&log, |_| true) == 4).await;
        let order: Vec<(EventKind, Option<HookKind>)> = log
            .lock()
            .unwrap()
            .iter()
            .map(|got| (got.kind, got.hook))
            .collect();
        assert_eq!(
            order.last(),
            Some(&(EventKind::TurnFinished, Some(HookKind::Handle))),
            "the handled turn came after the start events: {order:?}"
        );
        assert!(
            order.contains(&(EventKind::ActorStarted, None))
                && order.contains(&(EventKind::TurnFinished, Some(HookKind::PreStart)))
                && order.contains(&(EventKind::TurnFinished, Some(HookKind::OnStarted))),
            "{order:?}"
        );

        // A turn queued behind the backlog is delivered by the helper, not on
        // the actor's task.
        let queued = log
            .lock()
            .unwrap()
            .iter()
            .find(|got| got.hook == Some(HookKind::Handle))
            .cloned()
            .unwrap();
        assert_ne!(
            queued.task,
            Some(tasks.lock().unwrap()[0]),
            "the queued turn ran on the actor's task"
        );

        // With nothing outstanding a turn is delivered where it ended, and the
        // pump is not used for it.
        until("the actor's events are delivered", || {
            !pumped(&sys, &handle)
        })
        .await;
        let pushed = mode.pump.pushed_for_test();
        handle.send(Msg::Ok).await.unwrap();
        until("the second turn", || count(&log, |_| true) == 5).await;
        let second = log.lock().unwrap().last().cloned().unwrap();
        assert_eq!(second.task, Some(tasks.lock().unwrap()[0]));
        assert_eq!(mode.pump.pushed_for_test(), pushed, "the turn was queued");
    });
}

/// The helpers that work for one actor, and how a row makes the real helper
/// end while the pump holds what it is given.
#[derive(Clone, Copy, Debug)]
enum Helper {
    /// A helper task spawned by hand around an owner the row builds.
    ByHand,
    /// The stop ladder of a bounded stop whose first request the lane refuses.
    StopLadder,
    /// The start bound of a spawn with a start timeout, after the start.
    StartBound,
    /// The late-value drain of a queue that ended with a slot still out.
    LateDrain,
    /// The system monitor's helper, when another actor replaces its actor.
    Monitor,
    /// The reply helper of a supervisor's `terminate_child` through a
    /// `SupervisorRef`.
    SupervisorReply,
}

/// The end of a helper that works for an actor is counted in that actor's
/// events, so a turn of the actor never reaches a handler ahead of it.
async fn a_helper_end_naming_an_actor_is_counted_in_its_events(helper: Helper) {
    use crate::helper::{HelperKind, HelperOwner};
    use crate::system::LadderPoint;

    let row = format!("{helper:?}");
    let sys = ActorSystem::create(unique("helper-end")).unwrap();
    let mode = sys.telemetry_mode().clone();
    let name = unique("hp");
    let id = ActorId::from(name.as_str());
    let seen: Arc<Mutex<Vec<EventKind>>> = Arc::default();
    {
        let (seen, mine) = (seen.clone(), id.clone());
        assert!(sys.attach_telemetry(
            "helper-end",
            KindSet::of(&[EventKind::HelperEnded, EventKind::TurnFinished]),
            move |event| match event {
                TelemetryEvent::HelperEnded { owner, .. } if owner.as_ref() == Some(&mine) => {
                    seen.lock().unwrap().push(EventKind::HelperEnded);
                }
                TelemetryEvent::TurnFinished { hook, .. } if *hook == HookKind::Handle => {
                    seen.lock().unwrap().push(EventKind::TurnFinished);
                }
                _ => {}
            },
        ));
    }
    // The late-value row holds the actor's first turn, so its loop takes no
    // request from the system channel meanwhile.
    let gate = Arc::new(Notify::new());
    let probe = Probe {
        first_turn_gate: matches!(helper, Helper::LateDrain).then(|| gate.clone()),
        ..Probe::default()
    };
    let tasks = probe.tasks.clone();
    let builder = probe.spawn().named(name).on_system(&sys);
    let builder = if let Helper::SupervisorReply = helper {
        builder.supervisor()
    } else {
        builder
    };
    // The pump holds from before the start, so the end of the start bound's
    // helper is queued whenever it comes.
    if let Helper::StartBound | Helper::LateDrain = helper {
        mode.pump.hold_for_test(true);
    }
    let mut handle = Some(match helper {
        Helper::StartBound => builder.start_timeout(BOUND).await.unwrap(),
        _ => builder.await.unwrap(),
    });
    let actor = handle.clone().unwrap();
    let kid = unique("kid");
    if matches!(
        helper,
        Helper::ByHand | Helper::StopLadder | Helper::Monitor | Helper::SupervisorReply
    ) {
        if let Helper::SupervisorReply = helper {
            let name = kid.clone();
            let child = crate::actor::supervisor_ref::SupervisorRef::of(&actor)
                .start_child(Probe::default, move |child| child.named(name))
                .await
                .unwrap();
            // A child still starting has no run to wait for.
            until("the child runs", || {
                child.status() == crate::types::ActorStatus::Running
            })
            .await;
        }
        // The monitor is in place before the pump holds what it is given.
        if let Helper::Monitor = helper {
            let config = super::MonitorConfig {
                long_schedule: Some(Duration::from_millis(500)),
                long_message_queue: None,
            };
            sys.system_monitor(&actor, |_| Msg::Ok, config).unwrap();
        }
        until("the start events are delivered", || !pumped(&sys, &actor)).await;
        mode.pump.hold_for_test(true);
    }

    match helper {
        Helper::ByHand => {
            let owner =
                HelperOwner::new(id.clone(), Some(actor.stop_lane().abort_record().clone()));
            drop(sys.spawn_helper(
                &tokio::runtime::Handle::current(),
                HelperKind::LateDrain,
                Some(owner),
                |end| end.observe(async {}),
            ));
        }
        Helper::StopLadder => {
            // The lane refuses the first request after the ladder's task was
            // spawned, so the task ends at once and the actor goes on.
            let lane = actor.stop_lane();
            sys.set_ladder_hook(move |point, _| {
                if point == LadderPoint::StopBeforeRaise {
                    lane.close_to_non_kill();
                }
            });
            let caller = actor.clone();
            let refused = tokio::spawn(async move {
                caller
                    .stop_within(StopReason::Graceful, Duration::from_secs(3600))
                    .await
            })
            .await
            .unwrap();
            sys.clear_ladder_hook();
            assert!(refused.is_err(), "{row}: the lane refused the request");
        }
        Helper::StartBound => {}
        Helper::LateDrain => {
            // A slot out on the system channel when the actor ends hands the
            // channel to the drain, which ends once the value sent through
            // the slot is dropped.
            actor.notify(Msg::Ok).await.unwrap();
            until("the first turn is held", || {
                !tasks.lock().unwrap().is_empty()
            })
            .await;
            let record = actor.stop_lane().abort_record().clone();
            for _ in 0..2 {
                let (reply, _) = tokio::sync::oneshot::channel();
                record.submit_status(reply).await.unwrap();
            }
            let tx = record.system_tx_for_test().expect("the channel is open");
            let slot = tx.reserve_owned().await.unwrap();
            gate.notify_one();
            actor.stop(StopReason::Graceful).await.unwrap();
            actor.wait_stopped().await;
            let (reply, _) = tokio::sync::oneshot::channel();
            let _sender = slot.send(crate::types::SystemMessage::GetStatus(
                crate::types::StatusRequest { reply, task: false },
            ));
            handle = None;
        }
        Helper::SupervisorReply => {
            // The call answers once the child ended, and its reply helper
            // ends after the answer.
            crate::actor::supervisor_ref::SupervisorRef::of(&actor)
                .terminate_child(kid.as_str())
                .await
                .unwrap();
        }
        Helper::Monitor => {
            let other = Probe::default().spawn().on_system(&sys).await.unwrap();
            let config = super::MonitorConfig {
                long_schedule: Some(Duration::from_millis(500)),
                long_message_queue: None,
            };
            sys.system_monitor(&other, |_| Msg::Ok, config).unwrap();
        }
    }
    drop(actor);

    until(&format!("{row}: the end is queued"), || {
        mode.pump
            .queued_owners_for_test(EventKind::HelperEnded)
            .contains(&Some(id.clone()))
    })
    .await;
    // Read here, asserted after the order claim, so a wrong owner never
    // hides an order that broke.
    let owners = mode.pump.queued_owners_for_test(EventKind::HelperEnded);
    if let Some(handle) = &handle {
        let queued = mode.pump.queued_for_test();
        handle.send(Msg::Ok).await.unwrap();
        // The turn's event is raised after its reply: it is queued behind the
        // end, or a handler has it already.
        until(&format!("{row}: the turn's event is raised"), || {
            mode.pump.queued_for_test() > queued || !seen.lock().unwrap().is_empty()
        })
        .await;
    }
    mode.pump.hold_for_test(false);
    let want = match helper {
        // The actor ended: the held turn's event, raised before the end,
        // comes first.
        Helper::LateDrain => vec![EventKind::TurnFinished, EventKind::HelperEnded],
        _ => vec![EventKind::HelperEnded, EventKind::TurnFinished],
    };
    until(&format!("{row}: both are delivered"), || {
        seen.lock().unwrap().len() == want.len()
    })
    .await;
    assert_eq!(
        *seen.lock().unwrap(),
        want,
        "{row}: the actor's turn reached a handler ahead of the end of its helper"
    );
    assert_eq!(
        owners,
        vec![Some(id.clone())],
        "{row}: the end is not counted in the actor's events"
    );
}

#[test]
fn a_helper_end_naming_an_actor_reaches_a_handler_before_the_actors_next_turn() {
    for helper in [
        Helper::ByHand,
        Helper::StopLadder,
        Helper::StartBound,
        Helper::LateDrain,
        Helper::Monitor,
        Helper::SupervisorReply,
    ] {
        both_flavors(|| a_helper_end_naming_an_actor_is_counted_in_its_events(helper));
    }
}

/// Detaches a handler from inside its own turn.
struct Detacher {
    system: Arc<ActorSystem>,
}

impl Actor for Detacher {
    type Message = &'static str;
    type Response = ();

    async fn handle(&mut self, id: &'static str, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if !id.is_empty() {
            assert!(self.system.detach_telemetry(id));
        }
        Ok(())
    }
}

/// The hook and the task a handler ran on, for each turn event it saw.
type HookTasks = Arc<Mutex<Vec<(HookKind, Option<tokio::task::Id>)>>>;

/// Records the task it is dropped on.
struct DropsOn(Arc<Mutex<Vec<Option<tokio::task::Id>>>>);

impl Drop for DropsOn {
    fn drop(&mut self) {
        self.0.lock().unwrap().push(tokio::task::try_id());
    }
}

#[test]
fn the_state_of_a_handler_detached_meanwhile_is_not_dropped_on_the_actors_task() {
    both_flavors(|| async {
        let sys = ActorSystem::create(unique("release")).unwrap();
        let dropped: Arc<Mutex<Vec<Option<tokio::task::Id>>>> = Arc::default();
        let state = DropsOn(dropped.clone());
        let seen: HookTasks = Arc::default();
        let sink = seen.clone();
        assert!(sys.attach_telemetry(
            "holds",
            KindSet::of(&[EventKind::TurnFinished]),
            move |event| {
                let _ = &state;
                if let TelemetryEvent::TurnFinished { hook, .. } = event {
                    sink.lock().unwrap().push((*hook, tokio::task::try_id()));
                }
            }
        ));
        let probe = Probe::default();
        let tasks = probe.tasks.clone();
        let handle = probe.spawn().on_system(&sys).await.unwrap();
        // A turn event runs on the actor's task, and fills the loop's copy of
        // the handler list, only while the pump holds no earlier event about
        // the actor: wait for both start hooks' events to be delivered.
        until("the start hooks' events are delivered", || {
            let seen = seen.lock().unwrap();
            seen.iter().any(|(hook, _)| *hook == HookKind::OnStarted) && !pumped(&sys, &handle)
        })
        .await;
        handle.send(Msg::Ok).await.unwrap();
        // The handler has seen the handled turn: the loop read the list.
        until("the handled turn reaches the handler", || {
            seen.lock()
                .unwrap()
                .iter()
                .any(|(hook, _)| *hook == HookKind::Handle)
        })
        .await;
        let actor_task = tasks.lock().unwrap()[0];
        assert!(
            seen.lock()
                .unwrap()
                .contains(&(HookKind::Handle, Some(actor_task))),
            "the handled turn did not run its handler on the actor's task"
        );

        // The loop's copy of the handler list is now the last holder of the
        // handler's state.
        assert!(sys.detach_telemetry("holds"));
        assert!(dropped.lock().unwrap().is_empty());
        handle.stop(StopReason::Graceful).await.unwrap();
        handle.wait_stopped().await;
        until("the state is dropped", || {
            !dropped.lock().unwrap().is_empty()
        })
        .await;
        assert_ne!(
            dropped.lock().unwrap()[0],
            Some(actor_task),
            "the actor's task ran the handler's drop"
        );
    });
}

/// How a prefilled mailbox ends.
#[derive(Clone, Copy, Debug)]
enum Fill {
    /// Ten messages that go on.
    Ten,
    /// Two that go on, then one that fails.
    EndsInAFailure,
}

#[test]
fn a_run_reports_the_takes_it_made_and_an_empty_loop_reports_none() {
    for fill in [Fill::Ten, Fill::EndsInAFailure] {
        both_flavors(|| async move {
            let sys = ActorSystem::create(unique("runs")).unwrap();
            let log = log_events(
                &sys,
                "runs",
                KindSet::of(&[EventKind::RunFinished, EventKind::TurnFinished]),
            );
            let gate = Arc::new(Notify::new());
            let first_turn = Arc::new(Notify::new());
            let probe = Probe {
                gate: Some(gate.clone()),
                first_turn_gate: matches!(fill, Fill::Ten).then(|| first_turn.clone()),
                ..Probe::default()
            };
            let tasks = probe.tasks.clone();
            let handle = probe.spawn().on_system(&sys).detached().await.unwrap();
            let messages: Vec<Msg> = match fill {
                Fill::Ten => vec![Msg::Ok; 10],
                Fill::EndsInAFailure => vec![Msg::Ok, Msg::Ok, Msg::Err],
            };
            // The mailbox is full before the loop takes its first message,
            // so every take belongs to one run.
            for message in &messages {
                handle.try_notify(*message).unwrap();
            }
            gate.notify_one();
            if matches!(fill, Fill::Ten) {
                // The first turn waits until the pump has delivered the
                // start events, so the run's events find it empty of this
                // actor's and are delivered on its task.
                until("the first turn begins", || {
                    !tasks.lock().unwrap().is_empty()
                })
                .await;
                until("the start events are delivered", || !pumped(&sys, &handle)).await;
                first_turn.notify_one();
            }
            let taken = |log: &Log| -> usize {
                log.lock()
                    .unwrap()
                    .iter()
                    .filter(|got| got.kind == EventKind::RunFinished)
                    .map(|got| got.takes as usize)
                    .sum()
            };
            until("the takes of the runs add up to the messages", || {
                taken(&log) == messages.len()
            })
            .await;
            assert!(count(&log, |got| got.kind == EventKind::RunFinished) >= 1);

            let actor_task = tasks.lock().unwrap()[0];
            for got in log.lock().unwrap().iter() {
                if got.kind != EventKind::RunFinished {
                    continue;
                }
                match fill {
                    // Closed at the point the loop waits: nothing is owed.
                    Fill::Ten => assert_eq!(got.task, Some(actor_task), "{fill:?}"),
                    // Open when the loop exits: the stop sequence follows.
                    Fill::EndsInAFailure => assert!(
                        got.task.is_some() && got.task != Some(actor_task),
                        "{fill:?}: the open run's handler ran on the actor's task"
                    ),
                }
            }

            // An actor that took nothing reports no run; its last turn, the
            // stop hook, shows its events have all been delivered.
            let idle = Probe::default().spawn().on_system(&sys).await.unwrap();
            idle.stop(StopReason::Graceful).await.unwrap();
            idle.wait_stopped().await;
            until("the idle actor's stop hook turn", || {
                count(&log, |got| {
                    got.actor.id() == idle.id() && got.hook == Some(HookKind::OnStopped)
                }) == 1
            })
            .await;
            assert_eq!(
                count(&log, |got| got.actor.id() == idle.id()
                    && got.kind == EventKind::RunFinished),
                0,
                "{fill:?}: a loop that took nothing reported a run"
            );
        });
    }
}

/// A supervisor with one child whose first incarnation crashes.
struct Boss {
    spawned: Arc<AtomicUsize>,
}

struct Flaky {
    spawned: Arc<AtomicUsize>,
}

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let spawned = self.spawned.clone();
        ctx.spawn_child(move || Flaky {
            spawned: spawned.clone(),
        })
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

impl Actor for Flaky {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.spawned.fetch_add(1, Ordering::SeqCst) == 0 {
            ctx.self_handle().try_notify(()).unwrap();
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("first incarnation crashes"))
    }
}

#[test]
fn the_start_event_comes_once_per_incarnation_and_never_for_a_failed_start() {
    both_flavors(|| async {
        let sys = ActorSystem::create(unique("started")).unwrap();
        let log = log_events(&sys, "started", KindSet::of(&[EventKind::ActorStarted]));
        let started = |log: &Log, id: &ActorId| count(log, |got| got.actor.id() == id);

        // A start whose second hook sleeps reports at least the sleep, and
        // the pump delivers it.
        let sleep = Duration::from_millis(40);
        let slow = Probe {
            on_started_sleep: sleep,
            ..Probe::default()
        };
        let slow = slow.spawn().on_system(&sys).await.unwrap();
        until("the slow start's event", || started(&log, slow.id()) == 1).await;
        let first = log.lock().unwrap()[0].clone();
        assert!(first.took >= sleep, "the start took {:?}", first.took);
        assert!(first.task.is_some());

        // A start that fails in either hook raises none.
        let failures = [
            Probe {
                fail_pre_start: true,
                ..Probe::default()
            },
            Probe {
                fail_on_started: true,
                ..Probe::default()
            },
        ];
        let mut failed = Vec::new();
        for probe in failures {
            let name = unique("failed");
            assert!(probe
                .spawn()
                .named(name.clone())
                .on_system(&sys)
                .await
                .is_err());
            failed.push(ActorId::from(name));
        }
        // The pump is one queue: once a later start is reported, everything
        // raised before it has been delivered.
        let later = Probe::default().spawn().on_system(&sys).await.unwrap();
        until("the later start's event", || started(&log, later.id()) == 1).await;
        for id in &failed {
            assert_eq!(started(&log, id), 0, "a failed start reported {id}");
        }

        // Every incarnation of a restarted child reports its own start.
        let spawned = Arc::new(AtomicUsize::new(0));
        let boss = Boss {
            spawned: spawned.clone(),
        }
        .spawn()
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        let others = |got: &Got| {
            got.actor.id() != slow.id()
                && got.actor.id() != later.id()
                && got.actor.id() != boss.id()
        };
        until("two incarnations of the child start", || {
            spawned.load(Ordering::SeqCst) >= 2 && count(&log, others) == 2
        })
        .await;
        assert_eq!(started(&log, boss.id()), 1);
        let mut incarnations: Vec<u64> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|got| others(got))
            .map(|got| got.actor.incarnation())
            .collect();
        incarnations.sort_unstable();
        incarnations.dedup();
        assert_eq!(incarnations.len(), 2, "one event per incarnation");
    });
}

/// An actor that answers a request and stops itself in the same turn.
struct Stopper;

impl Actor for Stopper {
    type Message = ();
    type Response = u8;

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<u8> {
        ctx.stop(StopReason::Graceful)
            .expect("the lane takes the actor's own stop");
        Ok(42)
    }
}

#[test]
fn a_request_that_stops_its_own_actor_reports_one_turn_and_still_answers() {
    both_flavors(|| async {
        let sys = ActorSystem::create(unique("self-stop")).unwrap();
        let log = log_events(&sys, "turns", KindSet::of(&[EventKind::TurnFinished]));
        let handle = Stopper.spawn().on_system(&sys).await.unwrap();
        let answer = handle.send(()).await;
        assert!(
            matches!(answer, Ok(42)),
            "the caller reads the reply the stopping turn held: {answer:?}"
        );
        handle.wait_stopped().await;
        // The pump is one queue: the stop hook's turn comes after the
        // request's, so the request's has been reported by now or never.
        until("the stop hook turn is reported", || {
            count(&log, |got| got.hook == Some(HookKind::OnStopped)) == 1
        })
        .await;
        assert_eq!(
            count(&log, |got| got.hook == Some(HookKind::Handle)),
            1,
            "the turn that held its reply is reported once"
        );
        assert!(
            turn_of(&log, HookKind::Handle, TurnOutcome::Returned).is_some(),
            "and as returned"
        );
    });
}

/// An actor that sets an idle length in its start and notes its idle fire.
struct Idler {
    idled: Arc<AtomicUsize>,
}

impl Actor for Idler {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.idle_after(Duration::from_millis(1));
        Ok(())
    }

    async fn on_idle(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.cancel_idle_after();
        self.idled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn an_idle_fire_reports_its_turn_as_the_idle_hook() {
    both_flavors(|| async {
        let sys = ActorSystem::create(unique("idle-turn")).unwrap();
        let log = log_events(&sys, "turns", KindSet::of(&[EventKind::TurnFinished]));
        let idled = Arc::new(AtomicUsize::new(0));
        let handle = Idler {
            idled: idled.clone(),
        }
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();
        until("the idle hook ran", || idled.load(Ordering::SeqCst) == 1).await;
        handle.send(()).await.unwrap();
        // The pump is one queue: the message's turn comes after the idle
        // fire's, so the fire's has been reported by now or never.
        until("the message's turn is reported", || {
            count(&log, |got| got.hook == Some(HookKind::Handle)) == 1
        })
        .await;
        assert_eq!(
            count(&log, |got| got.hook == Some(HookKind::OnIdle)
                && got.outcome == Some(TurnOutcome::Returned)),
            1,
            "the idle fire is reported once, as returned"
        );
    });
}
