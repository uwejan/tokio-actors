//! Tests of the pump: the reaper, the Kill steps and the end of a helper task
//! raise their events into a bounded queue that one helper delivers, and none
//! of them runs a sink.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant as StdInstant};

use tokio::runtime::{Builder, Handle};
use tokio::time::Instant;

use super::actor_ref::ActorRef;
use super::event::{EventKind, HelperEnd, HelperKind, KillStepKind, KindSet, TelemetryEvent};
use super::identity::Identity;
use super::mode::{Mode, ModeOwner};
use super::pump::{Pump, PUMP_CAPACITY};
use super::TestSinkGuard;
use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::subtree::AbortRecord;
use crate::actor::{Actor, ActorExt};
use crate::error::ActorResult;
use crate::helper::{HelperEnd as InnerEnd, HelperKind as InnerKind};
use crate::system::{ActorSystem, ReaperEntry};
use crate::types::{ActorId, StopLane, StopReason};

/// The longest a scenario may take. It only limits how long a real hang takes
/// to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(30);

/// The kill grace of the actors that ignore their stop.
const GRACE: Duration = Duration::from_millis(100);

/// How much later than the grace the last abort of a run may land.
const SLACK: Duration = Duration::from_millis(1500);

/// Runs `f` on a thread of its own and waits for it: a scenario that hangs
/// (a push that waits, a sink that blocks the reaper, a lock taken twice)
/// fails here instead of hanging the test binary.
fn bounded<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(HANG_BOUND) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => panic!("timed out after {HANG_BOUND:?}: {what}"),
        Err(RecvTimeoutError::Disconnected) => panic!("the scenario panicked: {what}"),
    }
}

/// Runs the future `f` builds on a multi-thread runtime, inside [`bounded`].
fn scenario<F, Fut>(what: &str, f: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    bounded(what, move || {
        let rt = Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(f());
        rt.shutdown_timeout(Duration::from_secs(5));
    });
}

/// Runs the future `f` builds on a current-thread runtime, inside
/// [`bounded`].
fn scenario_current<F, Fut>(what: &str, f: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    bounded(what, move || {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(f());
        rt.shutdown_timeout(Duration::from_secs(5));
    });
}

/// Polls `condition` until it holds.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = StdInstant::now() + HANG_BOUND;
    while !condition() {
        assert!(
            StdInstant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A fresh system; its mode is bound to it from the start.
fn system() -> Arc<ActorSystem> {
    ActorSystem::create(format!("pump-{}", uuid::Uuid::new_v4())).unwrap()
}

/// Gives `record` an identity on `sys`'s mode and names `sys` as its system,
/// as a spawn made while a sink took events does.
fn bind_block(sys: &Arc<ActorSystem>, id: &ActorId, record: &AbortRecord) {
    record.set_system(sys);
    sys.telemetry_mode().bind(
        record.key(),
        Arc::new(Identity::new(id.clone(), 0, None, None, None)),
    );
}

/// An actor that takes a message and never finishes the turn, so only the
/// abort of the reaper ends it; with a gate it finishes the turn when the
/// test opens the gate.
struct Parked {
    parked: Arc<AtomicUsize>,
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl Actor for Parked {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.parked.fetch_add(1, Ordering::SeqCst);
        match &self.gate {
            Some(gate) => gate.notified().await,
            None => std::future::pending::<()>().await,
        }
        Ok(())
    }
}

/// `count` parked actors on `sys`, each with kill grace [`GRACE`] and a
/// telemetry identity, all inside their parked turn.
async fn park(sys: &Arc<ActorSystem>, count: usize) -> Vec<(String, ActorHandle<Parked>)> {
    let parked = Arc::new(AtomicUsize::new(0));
    let mut out = Vec::new();
    for _ in 0..count {
        let name = format!("parked-{}", uuid::Uuid::new_v4());
        let handle = Parked {
            parked: parked.clone(),
            gate: None,
        }
        .spawn()
        .named(name.clone())
        .kill_grace(GRACE)
        .on_system(sys)
        .await
        .expect("the actor starts");
        bind_block(sys, handle.id(), handle.stop_lane().abort_record());
        handle.notify(()).await.expect("the message is queued");
        out.push((name, handle));
    }
    until("every actor is inside its turn", || {
        parked.load(Ordering::SeqCst) == count
    })
    .await;
    out
}

/// Kills every actor by name and waits until all have stopped; returns how
/// long that took.
async fn kill_all(sys: &Arc<ActorSystem>, actors: &[(String, ActorHandle<Parked>)]) -> Duration {
    let started = StdInstant::now();
    for (name, _) in actors {
        sys.kill(name).await.expect("the kill is accepted");
    }
    for (_, handle) in actors {
        tokio::time::timeout(HANG_BOUND, handle.wait_stopped())
            .await
            .expect("the actor was aborted");
    }
    started.elapsed()
}

fn finished_helper_end() -> TelemetryEvent<'static> {
    TelemetryEvent::HelperEnded {
        kind: HelperKind::Reaper,
        owner: None,
        end: HelperEnd::Finished,
    }
}

/// Spawns a helper of `kind` that ends at once and returns its join handle.
fn short_helper(
    sys: &Arc<ActorSystem>,
    rt: &Handle,
    kind: InnerKind,
) -> tokio::task::JoinHandle<()> {
    sys.spawn_helper(rt, kind, None, |end| end.observe(async {}))
}

/// Pushes a sentinel through the pump and waits for `sink` to see it: every
/// event pushed before it has been delivered by then, as the queue is FIFO.
/// `sink` must take `HelperEnded`.
async fn flush(sys: &Arc<ActorSystem>, sink: &TestSinkGuard) {
    let marker = ActorId::anonymous();
    let wanted = marker.clone();
    sys.telemetry_mode()
        .push(EventKind::HelperEnded, || TelemetryEvent::HelperEnded {
            kind: HelperKind::Pump,
            owner: Some(marker),
            end: HelperEnd::Finished,
        });
    until("the sentinel is delivered", || {
        sink.events().iter().any(|event| {
            matches!(event, TelemetryEvent::HelperEnded { owner: Some(o), .. } if *o == wanted)
        })
    })
    .await;
}

fn kill_steps(sink: &TestSinkGuard) -> Vec<(KillStepKind, Option<Duration>)> {
    sink.events()
        .into_iter()
        .filter_map(|event| match event {
            TelemetryEvent::KillStep { step, after, .. } => Some((step, after)),
            _ => None,
        })
        .collect()
}

/// The kill steps reported for the actor `id`.
fn kill_steps_of(sink: &TestSinkGuard, id: &ActorId) -> Vec<(KillStepKind, Option<Duration>)> {
    sink.events()
        .into_iter()
        .filter_map(|event| match event {
            TelemetryEvent::KillStep { actor, step, after } if actor.id() == id => {
                Some((step, after))
            }
            _ => None,
        })
        .collect()
}

/// Waits until `id`'s abort has been reported, then flushes. The abort is
/// reported after it is requested, so an actor can be seen stopped before its
/// `AbortFired` step is queued: the end of the actor is not the end of the
/// report.
async fn flush_after_abort(sys: &Arc<ActorSystem>, sink: &TestSinkGuard, id: &ActorId) {
    until("the abort is reported", || {
        kill_steps_of(sink, id)
            .iter()
            .any(|(step, _)| *step == KillStepKind::AbortFired)
    })
    .await;
    flush(sys, sink).await;
}

/// Feeds the reaper of the current runtime an entry that falls due `past`
/// from now, waits for its abort to be reported and flushes. The reaper takes
/// entries in the order they fall due, so when this returns it has handled
/// every entry due earlier and their steps are delivered: a step that is
/// still missing is one that is never reported. A kill made through the
/// system or a handle is also aborted by its caller once the grace has
/// passed; whichever abort finds the task unfinished first reports
/// `AbortFired`, and the other reports nothing.
async fn flush_after_reaper(sys: &Arc<ActorSystem>, sink: &TestSinkGuard, past: Duration) {
    let (lane, _rx) = StopLane::new();
    let id = ActorId::anonymous();
    bind_block(sys, &id, lane.abort_record());
    let task = tokio::spawn(std::future::pending::<()>());
    sys.feed_reaper(
        &Handle::current(),
        ReaperEntry::new(
            Instant::now() + past,
            task.abort_handle(),
            lane.abort_record().clone(),
        ),
    );
    flush_after_abort(sys, sink, &id).await;
}

// ---------------------------------------------------------------------------
// With no sink taking the events, nothing is built, queued or started.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The queue holds PUMP_CAPACITY events and counts what it drops; a push never
// waits for room.
// ---------------------------------------------------------------------------

#[test]
fn the_queue_keeps_its_bound_and_counts_what_it_drops() {
    // (pushes, queued after, dropped after)
    let rows = [
        (10, 10, 0),
        (PUMP_CAPACITY, PUMP_CAPACITY, 0),
        (PUMP_CAPACITY + 1, PUMP_CAPACITY, 1),
        (PUMP_CAPACITY + 300, PUMP_CAPACITY, 300),
    ];
    for (pushes, queued, dropped) in rows {
        bounded("pushes into a queue with no consumer", move || {
            let mode = Mode::new();
            let _sink = mode.collect(KindSet::all());
            mode.pump.stall_for_test();
            for _ in 0..pushes {
                mode.push(EventKind::HelperEnded, finished_helper_end);
            }
            assert_eq!(
                mode.pump.queued_for_test(),
                queued,
                "{pushes} pushes: queued"
            );
            assert_eq!(
                mode.pump.dropped_for_test(),
                dropped,
                "{pushes} pushes: dropped"
            );
        });
    }
}

// ---------------------------------------------------------------------------
// A sink that blocks or re-enters cannot hold up the reaper or the registry.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// No sink runs on the reaper or on a helper task whose end it reports.
// ---------------------------------------------------------------------------

#[test]
fn no_sink_runs_on_a_helper_task_whose_end_it_reports() {
    let rows = [
        InnerKind::Reaper,
        InnerKind::Sweep,
        InnerKind::LateDrain,
        InnerKind::StartBound,
        InnerKind::StopLadder,
        InnerKind::ShutdownWalk,
    ];
    for kind in rows {
        scenario("the task a helper end's sink runs on", move || async move {
            let sys = system();
            let mode = sys.telemetry_mode().clone();
            let rt = Handle::current();
            let ran_on = Arc::new(Mutex::new(Vec::new()));
            let seen = ran_on.clone();
            let _sink = mode.test_sink(KindSet::of(&[EventKind::HelperEnded]), move |event| {
                if let TelemetryEvent::HelperEnded { kind, .. } = event {
                    seen.lock().unwrap().push((*kind, tokio::task::try_id()));
                }
            });
            let helper = short_helper(&sys, &rt, kind);
            let helper_task = helper.id();
            helper.await.unwrap();
            until("the end is delivered", || {
                !ran_on.lock().unwrap().is_empty()
            })
            .await;
            let ran_on = ran_on.lock().unwrap();
            assert_eq!(ran_on.len(), 1, "{kind:?}: one end event");
            assert_eq!(ran_on[0].0, HelperKind::from(kind));
            let task = ran_on[0].1.expect("the sink ran inside a task");
            assert_ne!(
                task, helper_task,
                "{kind:?}: the sink ran on the helper task"
            );
        });
    }
}

// ---------------------------------------------------------------------------
// The helper starts on the first armed event, and ends with its system.
// ---------------------------------------------------------------------------

#[test]
fn the_helper_starts_on_the_first_event_and_ends_when_the_pump_closes() {
    scenario("the pump helper's life", || async {
        let sys = system();
        let rt = Handle::current();
        // A mode of its own, so the test can drop its owner as a system
        // drop does, bound to a system that stays.
        let owner = ModeOwner::new();
        let mode = owner.0.clone();
        mode.bind_system(&sys);
        let sink = mode.collect(KindSet::all());

        assert_eq!(sys.live_helpers(), 0);
        assert!(!mode.pump.running_for_test());
        mode.push(EventKind::HelperEnded, finished_helper_end);
        until("the helper delivered the event", || {
            sink.events().len() == 1
        })
        .await;
        assert!(mode.pump.running_for_test());
        assert_eq!(sys.live_helpers(), 1, "one helper, through the helper slot");

        // Many pushes use the one helper.
        for _ in 0..100 {
            mode.push(EventKind::HelperEnded, finished_helper_end);
            tokio::task::yield_now().await;
        }
        until("every event is delivered or counted", || {
            sink.events().len() as u64 + mode.pump.dropped_for_test() == 101
        })
        .await;
        assert_eq!(sys.live_helpers(), 1, "a push does not start a task");
        let slot = sys.helper_slot_of(&rt).expect("the helper slot");

        // An event pushed before the close is delivered, then the helper
        // ends.
        mode.push(EventKind::HelperEnded, finished_helper_end);
        drop(owner);
        // The count of live helpers falls when the end is recorded and the
        // running flag clears when the task's last field is dropped, a moment
        // later, so the test waits for both.
        until("the helper ended and cleared its flag", || {
            slot.live() == 0 && !mode.pump.running_for_test()
        })
        .await;
        assert_eq!(
            sink.events().len() as u64 + mode.pump.dropped_for_test(),
            102
        );
        assert_eq!(slot.ends(), vec![(InnerKind::Pump, InnerEnd::Finished)]);

        // A push after the close starts nothing.
        mode.push(EventKind::HelperEnded, finished_helper_end);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(slot.live(), 0);
    });
}

// ---------------------------------------------------------------------------
// The Kill steps: each once, in order, and only for a raise the lane took.
// ---------------------------------------------------------------------------

/// The abort of a task is reported by the abort that finds it unfinished
/// first, once per incarnation, whichever caller issues it: the reaper's and a
/// caller's own force path both end in `AbortRecord::abort`. The aborts are
/// issued back to back on a current-thread runtime, so the task cannot end
/// between them and the second abort is the loser of a real race.
#[test]
fn an_abort_is_reported_once_whoever_issues_it() {
    struct Row {
        what: &'static str,
        /// End the task before the abort.
        task_ends: bool,
        aborts: usize,
        expect: usize,
    }
    let rows = [
        Row {
            what: "one abort of a live task",
            task_ends: false,
            aborts: 1,
            expect: 1,
        },
        Row {
            what: "two aborts of a live task",
            task_ends: false,
            aborts: 2,
            expect: 1,
        },
        Row {
            what: "an abort of a task that ended",
            task_ends: true,
            aborts: 1,
            expect: 0,
        },
    ];
    for row in rows {
        scenario_current(row.what, move || async move {
            let sys = system();
            let mode = sys.telemetry_mode().clone();
            let sink = mode.collect(KindSet::all());
            let (lane, _rx) = StopLane::new();
            let id = ActorId::anonymous();
            bind_block(&sys, &id, lane.abort_record());
            let task = tokio::spawn(std::future::pending::<()>());
            let abort = task.abort_handle();
            if row.task_ends {
                abort.abort();
                let _ = task.await;
            }
            for _ in 0..row.aborts {
                lane.abort_record().abort(&abort);
            }
            flush(&sys, &sink).await;
            let order: Vec<_> = kill_steps_of(&sink, &id).iter().map(|(s, _)| *s).collect();
            assert_eq!(
                order,
                vec![KillStepKind::AbortFired; row.expect],
                "{}",
                row.what
            );
        });
    }
}

/// Queues the end of an actor when its task is dropped, as the run loop's end
/// does: the event the abort of a task causes.
struct EndOnDrop {
    mode: Arc<Mode>,
    actor: ActorRef,
    ended: Arc<AtomicBool>,
}

impl Drop for EndOnDrop {
    fn drop(&mut self) {
        let actor = self.actor.clone();
        self.mode
            .push(EventKind::ActorEnded, || actor_ended(&actor));
        self.ended.store(true, Ordering::SeqCst);
    }
}

/// Two aborts of one task race: the winner is held between its lock and its
/// push (the test hook), the loser then aborts. The loser must not be able to
/// end the task ahead of the winner's step, so the step precedes the actor's
/// end whichever abort ends the task. The loser is watched for a bounded
/// window: a loser that is not held by the lock ends the task inside it, and
/// the winner then pushes after the end.
async fn a_losing_abort_waits_for_the_step() {
    use crate::actor::subtree::test_hook;
    let sys = system();
    let mode = sys.telemetry_mode().clone();
    let sink = mode.collect(KindSet::all());
    let (lane, _rx) = StopLane::new();
    let id = ActorId::anonymous();
    bind_block(&sys, &id, lane.abort_record());
    let ended = Arc::new(AtomicBool::new(false));
    let end = EndOnDrop {
        mode: mode.clone(),
        actor: ActorRef::new_root(id.clone(), 0),
        ended: ended.clone(),
    };
    let task = tokio::spawn(async move {
        let _end = end;
        std::future::pending::<()>().await;
    });
    let abort = task.abort_handle();
    let record = lane.abort_record().clone();
    let (held_tx, held_rx) = channel::<()>();
    let (release_tx, release_rx) = channel::<()>();
    let winner = {
        let (record, abort) = (record.clone(), abort.clone());
        std::thread::spawn(move || {
            test_hook::arm_before_push(move || {
                held_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(HANG_BOUND)
                    .expect("the test releases the winner");
            });
            record.abort(&abort);
        })
    };
    until("the winner holds the record's lock", || {
        held_rx.try_recv().is_ok()
    })
    .await;
    let (done_tx, done_rx) = channel::<()>();
    let loser = std::thread::spawn(move || {
        record.abort(&abort);
        let _ = done_tx.send(());
    });
    let window = StdInstant::now() + Duration::from_millis(300);
    let mut loser_done = false;
    while !loser_done && StdInstant::now() < window {
        loser_done = done_rx.try_recv().is_ok();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    if loser_done {
        until("the loser's abort ended the task", || {
            ended.load(Ordering::SeqCst)
        })
        .await;
    }
    release_tx.send(()).unwrap();
    winner.join().unwrap();
    loser.join().unwrap();
    until("the winner's abort ended the task", || {
        ended.load(Ordering::SeqCst)
    })
    .await;
    flush(&sys, &sink).await;
    let order: Vec<&str> = sink
        .events()
        .iter()
        .filter_map(|event| match event {
            TelemetryEvent::KillStep { actor, step, .. }
                if actor.id() == &id && *step == KillStepKind::AbortFired =>
            {
                Some("AbortFired")
            }
            TelemetryEvent::ActorEnded { actor, .. } if actor.id() == &id => Some("ActorEnded"),
            _ => None,
        })
        .collect();
    assert_eq!(order, vec!["AbortFired", "ActorEnded"]);
}

#[test]
fn an_abort_that_lost_the_race_cannot_end_the_task_ahead_of_the_step() {
    scenario(
        "losing abort, multi-thread",
        a_losing_abort_waits_for_the_step,
    );
    scenario_current(
        "losing abort, current-thread",
        a_losing_abort_waits_for_the_step,
    );
}

/// Wakes by sealing a lane: the loop's waker of a task that ends as soon as
/// it is woken.
struct SealOnWake(StopLane);

impl std::task::Wake for SealOnWake {
    fn wake(self: Arc<Self>) {
        self.0.seal();
    }
}

// ---------------------------------------------------------------------------
// The pump's own life: an unpolled helper, a system bound from its creation,
// an event raised after the system is gone, and what the pump drops.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// An event raised from inside a handler is skipped and counted.
// ---------------------------------------------------------------------------

async fn raised_inside_a_handler() {
    {
        let sys = system();
        let mode = sys.telemetry_mode().clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let (m, c) = (mode.clone(), calls.clone());
        assert!(sys.attach_telemetry(
            "raiser",
            KindSet::of(&[EventKind::HelperEnded]),
            move |_| {
                c.fetch_add(1, Ordering::SeqCst);
                m.push(EventKind::HelperEnded, finished_helper_end);
            }
        ));
        mode.push(EventKind::HelperEnded, finished_helper_end);
        until("the nested event is counted", || {
            mode.pump.dropped_for_test() >= 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the pump did not loop");
        assert_eq!(mode.pump.dropped_for_test(), 1, "the nested event counted");
        assert_eq!(mode.pump.queued_for_test(), 0, "nothing was queued");
        sys.detach_telemetry("raiser");
    }
}

// ---------------------------------------------------------------------------
// What the pump drops is carried into the subscriptions that would have
// taken it.
// ---------------------------------------------------------------------------

async fn carried_drop() {
    use super::subscription::{self, Filter, QueueItem, Scope, SubEntry, SubQueue, SubscriptionId};
    {
        const EXTRA: u64 = 9;
        let sys = system();
        let mode = sys.telemetry_mode().clone();
        let (queue, mut rx) = SubQueue::new(PUMP_CAPACITY * 2);
        let other_queue = SubQueue::new(PUMP_CAPACITY * 2);
        subscription::register(
            &mode,
            SubEntry {
                id: SubscriptionId(1),
                watcher: ActorRef::new_root(ActorId::from("w"), 1),
                filter: Filter::new(Scope::System).kinds(KindSet::of(&[EventKind::HelperEnded])),
                queue: queue.clone(),
            },
        );
        subscription::register(
            &mode,
            SubEntry {
                id: SubscriptionId(2),
                watcher: ActorRef::new_root(ActorId::from("x"), 1),
                filter: Filter::new(Scope::System).kinds(KindSet::of(&[EventKind::KillStep])),
                queue: other_queue.0.clone(),
            },
        );
        mode.pump.stall_for_test();
        for _ in 0..PUMP_CAPACITY as u64 + EXTRA {
            mode.push(EventKind::HelperEnded, finished_helper_end);
        }
        assert_eq!(mode.pump.dropped_for_test(), EXTRA);
        assert_eq!(
            queue.dropped.load(Ordering::SeqCst),
            EXTRA,
            "the watcher's count"
        );
        assert_eq!(
            other_queue.0.dropped.load(Ordering::SeqCst),
            0,
            "a filter that would not take the kind is not charged"
        );
        mode.pump.unstall_for_test();
        Pump::ensure_started(&mode);
        let mut events = 0usize;
        let mut missed = 0u64;
        until("the queued events reach the subscription", || {
            while let Ok(item) = rx.try_recv() {
                if let QueueItem::Event { missed_before, .. } = item {
                    events += 1;
                    missed += missed_before;
                }
            }
            events == PUMP_CAPACITY
        })
        .await;
        assert_eq!(missed, EXTRA, "missed_before reports the loss");
    }
}

// ---------------------------------------------------------------------------
// Every way the pump gives up on an event counts it once and carries it into
// exactly the subscriptions whose filter would have taken it.
// ---------------------------------------------------------------------------

/// The ways an event is dropped.
#[derive(Clone, Copy, Debug)]
enum DropPath {
    /// The queue holds its bound and the event finds it full.
    FullQueue,
    /// The event is raised inside a handler of its own system.
    HandlerSkip,
    /// The event is raised after its system is gone.
    SystemGone,
}

fn actor_ended(actor: &ActorRef) -> TelemetryEvent<'static> {
    TelemetryEvent::ActorEnded {
        actor: actor.clone(),
        reason: StopReason::Graceful,
        lifetime: None,
    }
}

/// Drives `path` once and checks the count and the carry. `drops` is the
/// number of events the path gives up on.
async fn one_drop_path(path: DropPath, drops: u64) {
    use super::subscription::{self, Filter, Scope, SubEntry, SubQueue, SubscriptionId};
    let root = ActorRef::new_root(ActorId::from("p"), 1);
    let subject = ActorRef::new_child(ActorId::from("a"), 1, &root);
    // Held for the whole run, so a system that is meant to live does.
    let live = system();
    let mode = match path {
        DropPath::SystemGone => Mode::new_bound(std::sync::Weak::new(), ""),
        _ => live.telemetry_mode().clone(),
    };
    let ended = KindSet::of(&[EventKind::ActorEnded]);
    // (name, scope, kinds, watcher, charged)
    let rows = [
        ("system scope", Scope::System, ended, "w1", true),
        (
            "kind it does not take",
            Scope::System,
            KindSet::of(&[EventKind::KillStep]),
            "w2",
            false,
        ),
        (
            "another actor",
            Scope::Actor(ActorId::from("zz")),
            ended,
            "w3",
            false,
        ),
        (
            "this actor",
            Scope::Actor(ActorId::from("a")),
            ended,
            "w4",
            true,
        ),
        (
            "the actor's ancestor",
            Scope::Subtree(ActorId::from("p")),
            ended,
            "w5",
            true,
        ),
        ("the subject as watcher", Scope::System, ended, "a", false),
    ];
    let mut queues = Vec::new();
    for (n, (_, scope, kinds, watcher, _)) in rows.iter().enumerate() {
        let (queue, rx) = SubQueue::new(PUMP_CAPACITY * 2);
        let watcher_ref = if *watcher == "a" {
            subject.clone()
        } else {
            ActorRef::new_root(ActorId::from(*watcher), 1)
        };
        subscription::register(
            &mode,
            SubEntry {
                id: SubscriptionId(n as u64 + 1),
                watcher: watcher_ref,
                filter: Filter::new(scope.clone()).kinds(*kinds),
                queue: queue.clone(),
            },
        );
        queues.push((queue, rx));
    }
    let calls = Arc::new(AtomicUsize::new(0));
    match path {
        DropPath::FullQueue => {
            mode.pump.stall_for_test();
            for _ in 0..PUMP_CAPACITY as u64 + drops {
                mode.push(EventKind::ActorEnded, || actor_ended(&subject));
            }
            assert_eq!(
                mode.pump.queued_for_test(),
                PUMP_CAPACITY,
                "the bound holds"
            );
        }
        DropPath::SystemGone => {
            assert!(mode.system_gone(), "bound to a system that is gone");
            for _ in 0..drops {
                mode.push(EventKind::ActorEnded, || actor_ended(&subject));
            }
            assert_eq!(mode.pump.queued_for_test(), 0, "nothing is queued");
        }
        DropPath::HandlerSkip => {
            assert_eq!(drops, 1, "one event reaches the handler");
            let (m, c, s) = (mode.clone(), calls.clone(), subject.clone());
            assert!(live.attach_telemetry("raiser", ended, move |_| {
                c.fetch_add(1, Ordering::SeqCst);
                m.push(EventKind::ActorEnded, || actor_ended(&s));
            }));
            let reports = Arc::new(Mutex::new(Vec::new()));
            let seen = reports.clone();
            assert!(live.attach_telemetry(
                "reporter",
                KindSet::of(&[EventKind::Dropped]),
                move |event| {
                    if let TelemetryEvent::Dropped { missed } = event {
                        seen.lock().unwrap().push(*missed);
                    }
                }
            ));
            mode.push(EventKind::ActorEnded, || actor_ended(&subject));
            until("the nested event is counted", || {
                mode.pump.dropped_for_test() >= 1
            })
            .await;
            until("the skip is reported to the handler", || {
                !reports.lock().unwrap().is_empty()
            })
            .await;
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert_eq!(calls.load(Ordering::SeqCst), 1, "the pump did not loop");
            assert_eq!(
                mode.pump.queued_for_test(),
                0,
                "the nested event was not queued"
            );
            assert_eq!(
                *reports.lock().unwrap(),
                vec![1],
                "one notice carries the skipped event"
            );
            live.detach_telemetry("reporter");
        }
    }
    assert_eq!(
        mode.pump.dropped_for_test(),
        drops,
        "{path:?}: each drop is counted once"
    );
    for ((name, _, _, _, charged), (queue, _)) in rows.iter().zip(&queues) {
        let expected = if *charged { drops } else { 0 };
        assert_eq!(
            queue.dropped.load(Ordering::SeqCst),
            expected,
            "{path:?}: the subscription with {name}"
        );
    }
    live.detach_telemetry("raiser");
}

async fn drop_paths() {
    for (path, drops) in [
        (DropPath::FullQueue, 3),
        (DropPath::HandlerSkip, 1),
        (DropPath::SystemGone, 2),
    ] {
        one_drop_path(path, drops).await;
    }
}

// ---------------------------------------------------------------------------
// The running mark belongs to one system.
// ---------------------------------------------------------------------------

async fn two_systems() {
    let a = system();
    let b = system();
    let (mode_a, mode_b) = (a.telemetry_mode().clone(), b.telemetry_mode().clone());
    let sink_b = mode_b.collect(KindSet::of(&[EventKind::HelperEnded]));
    let into_b = mode_b.clone();
    let into_a = mode_a.clone();
    // A handler of A raises one event into B and one into A.
    assert!(a.attach_telemetry(
        "crosser",
        KindSet::of(&[EventKind::HelperEnded]),
        move |_| {
            into_b.push(EventKind::HelperEnded, finished_helper_end);
            into_a.push(EventKind::HelperEnded, finished_helper_end);
        }
    ));
    mode_a.push(EventKind::HelperEnded, finished_helper_end);
    until("B's event is delivered", || {
        sink_b
            .events()
            .iter()
            .any(|event| matches!(event, TelemetryEvent::HelperEnded { .. }))
    })
    .await;
    until("A's own nested event is counted", || {
        mode_a.pump.dropped_for_test() >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        mode_a.pump.dropped_for_test(),
        1,
        "A skipped only its own nested event"
    );
    assert_eq!(
        mode_b.pump.dropped_for_test(),
        0,
        "a handler of A never made B skip"
    );
    assert_eq!(sink_b.events().len(), 1, "B's event arrived once");
}

// ---------------------------------------------------------------------------
// The mark ends with the handler, however the handler ends.
// ---------------------------------------------------------------------------

async fn mark_ends_with_the_handler() {
    let sys = system();
    let mode = sys.telemetry_mode().clone();
    let sink = mode.collect(KindSet::of(&[
        EventKind::HelperEnded,
        EventKind::HandlerDetached,
    ]));
    assert!(sys.attach_telemetry(
        "panicker",
        KindSet::of(&[EventKind::HelperEnded]),
        |_| panic!("handler panic")
    ));
    mode.push(EventKind::HelperEnded, finished_helper_end);
    until("the handler is detached", || {
        sink.events()
            .iter()
            .any(|event| matches!(event, TelemetryEvent::HandlerDetached { .. }))
    })
    .await;
    // From this thread, which is the helper's on a current-thread runtime.
    mode.push(EventKind::HelperEnded, finished_helper_end);
    until("the next event is delivered", || {
        sink.events()
            .iter()
            .filter(|event| matches!(event, TelemetryEvent::HelperEnded { .. }))
            .count()
            == 2
    })
    .await;
    assert_eq!(mode.pump.dropped_for_test(), 0, "nothing was skipped");
}

// ---------------------------------------------------------------------------
// An event delivered inside a handler of its own system skips the handlers
// and is counted once; the subscriptions had it already.
// ---------------------------------------------------------------------------

async fn nested_delivery() {
    use super::subscription::{self, Filter, QueueItem, Scope, SubEntry, SubQueue, SubscriptionId};
    let sys = system();
    let mode = sys.telemetry_mode().clone();
    let (queue, mut rx) = SubQueue::new(PUMP_CAPACITY);
    subscription::register(
        &mode,
        SubEntry {
            id: SubscriptionId(1),
            watcher: ActorRef::new_root(ActorId::from("w"), 1),
            filter: Filter::new(Scope::System).kinds(KindSet::of(&[EventKind::HelperEnded])),
            queue: queue.clone(),
        },
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let (m, c) = (mode.clone(), calls.clone());
    assert!(sys.attach_telemetry(
        "nester",
        KindSet::of(&[EventKind::HelperEnded]),
        move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            m.dispatch(EventKind::HelperEnded, finished_helper_end);
        }
    ));
    mode.push(EventKind::HelperEnded, finished_helper_end);
    until("the nested delivery is counted", || {
        mode.pump.dropped_for_test() >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the handler ran once");
    assert_eq!(mode.pump.dropped_for_test(), 1, "the skip counted once");
    assert_eq!(
        queue.dropped.load(Ordering::SeqCst),
        0,
        "the subscription had the event already, so it is not charged"
    );
    let mut events = 0;
    while let Ok(item) = rx.try_recv() {
        if let QueueItem::Event { missed_before, .. } = item {
            assert_eq!(missed_before, 0, "nothing was missed");
            events += 1;
        }
    }
    assert_eq!(
        events, 2,
        "the subscription saw the event and the nested one"
    );
}

// ---------------------------------------------------------------------------
// Events still queued when the system's telemetry state drops are counted.
// ---------------------------------------------------------------------------

async fn queued_at_the_drop() {
    use super::subscription::{self, Filter, Scope, SubEntry, SubQueue, SubscriptionId};
    const QUEUED: u64 = 5;
    let mode = Mode::new();
    let (taking, _rx) = SubQueue::new(PUMP_CAPACITY);
    let (other, _other_rx) = SubQueue::new(PUMP_CAPACITY);
    for (n, (queue, kind)) in [
        (&taking, EventKind::HelperEnded),
        (&other, EventKind::KillStep),
    ]
    .into_iter()
    .enumerate()
    {
        subscription::register(
            &mode,
            SubEntry {
                id: SubscriptionId(n as u64 + 1),
                watcher: ActorRef::new_root(ActorId::from(format!("w{n}")), 1),
                filter: Filter::new(Scope::System).kinds(KindSet::of(&[kind])),
                queue: queue.clone(),
            },
        );
    }
    // No helper can run: the flag is held, as for a push with no runtime.
    mode.pump.stall_for_test();
    for _ in 0..QUEUED {
        mode.push(EventKind::HelperEnded, finished_helper_end);
    }
    assert_eq!(mode.pump.queued_for_test() as u64, QUEUED, "queued");
    assert_eq!(mode.pump.dropped_for_test(), 0, "none dropped yet");
    // Counted on a live mode by the same function the drop calls.
    mode.pump.discard_queued(&mode);
    assert_eq!(mode.pump.queued_for_test(), 0, "emptied");
    assert_eq!(mode.pump.dropped_for_test(), QUEUED, "counted once each");
    assert_eq!(taking.dropped.load(Ordering::SeqCst), QUEUED, "carried");
    assert_eq!(other.dropped.load(Ordering::SeqCst), 0, "kind not taken");
    // And by the drop itself.
    let again = Mode::new();
    let (watcher, _watcher_rx) = SubQueue::new(PUMP_CAPACITY);
    subscription::register(
        &again,
        SubEntry {
            id: SubscriptionId(9),
            watcher: ActorRef::new_root(ActorId::from("w9"), 1),
            filter: Filter::new(Scope::System).kinds(KindSet::of(&[EventKind::HelperEnded])),
            queue: watcher.clone(),
        },
    );
    again.pump.stall_for_test();
    for _ in 0..QUEUED {
        again.push(EventKind::HelperEnded, finished_helper_end);
    }
    assert_eq!(
        watcher.dropped.load(Ordering::SeqCst),
        0,
        "queued, not dropped"
    );
    drop(again);
    assert_eq!(
        watcher.dropped.load(Ordering::SeqCst),
        QUEUED,
        "the drop of the mode counted and carried what was still queued"
    );
}
