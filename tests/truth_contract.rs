//! Behavioral suite for the `terminate_child`/`stop_child` truth contract:
//! a return from `terminate_child` means the child is gone AND its name is
//! free, in the SAME handler invocation that awaited it - and
//! `on_child_stopped` fires exactly once per death, regardless of how a
//! same-handler `delete_child`/respawn or a dropped caller future interleave
//! with the run loop's own observation of the underlying watcher completion.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{sleep, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorSystem, ChildEvent, RestartType, Shutdown, StopReason,
    SupervisionAction, SupervisionConfig,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type EventLog = Arc<tokio::sync::Mutex<Vec<ChildEvent>>>;

fn recorder() -> EventLog {
    Arc::new(tokio::sync::Mutex::new(Vec::new()))
}

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("truth-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

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
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_events<P>(events: &EventLog, timeout_ms: u64, pred: P) -> Vec<ChildEvent>
where
    P: Fn(&[ChildEvent]) -> bool,
{
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        {
            let guard = events.lock().await;
            if pred(&guard) {
                return guard.clone();
            }
        }
        if Instant::now() >= deadline {
            return events.lock().await.clone();
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn events_for(events: &[ChildEvent], name: &str) -> Vec<ChildEvent> {
    events
        .iter()
        .filter(|e| e.child_name.as_deref() == Some(name))
        .cloned()
        .collect()
}

async fn op(sup: &ActorHandle<Sup>, cmd: SupCmd) -> Result<(), String> {
    let SupReply::Done(res) = sup.send(cmd).await.expect("supervisor must answer");
    res
}

/// Re-looks a named `Worker` up until a FRESH instance responds (count == 0).
async fn wait_worker_ready(name: &str, timeout_ms: u64) -> Option<ActorHandle<Worker>> {
    let sys = ActorSystem::default();
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if let Some(handle) = sys.get::<Worker>(name) {
            if let Ok(0) = handle.send(WorkerMsg::Count).await {
                return Some(handle);
            }
        }
        sleep(Duration::from_millis(10)).await;
    }
    None
}

/// True once `name` resolves in the default system to a live handle of type `A`.
async fn wait_named_alive<A: Actor>(name: &str, timeout_ms: u64) -> bool {
    let sys = ActorSystem::default();
    wait_until(timeout_ms, || {
        sys.get::<A>(name).map(|h| h.is_alive()).unwrap_or(false)
    })
    .await
}

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct Worker {
    count: u32,
}

#[derive(Clone)]
enum WorkerMsg {
    Bump,
    Count,
    Crash,
}

impl Actor for Worker {
    type Message = WorkerMsg;
    type Response = u32;

    async fn handle(&mut self, msg: WorkerMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        match msg {
            WorkerMsg::Bump => {
                self.count += 1;
                Ok(self.count)
            }
            WorkerMsg::Count => Ok(self.count),
            WorkerMsg::Crash => panic!("worker crashed on command"),
        }
    }
}

/// The command-driven supervisor. Every command below runs to completion
/// inside ONE dispatched `handle` invocation - the whole point of the tests
/// that chain terminate/delete/respawn together.
struct Sup {
    events: EventLog,
}

#[derive(Clone)]
enum SupCmd {
    SpawnWorker {
        name: String,
    },
    /// Runs terminate_child -> delete_child -> spawn_child (same name) all
    /// inside this ONE handler invocation.
    TerminateDeleteRespawn {
        name: String,
    },
    /// Repeats `TerminateDeleteRespawn`'s cycle `iterations - 1` times, then
    /// spawns one FINAL fresh instance and returns - all inside this ONE
    /// handler invocation. Every prior incarnation's watcher completion is
    /// still sitting, undrained, in the supervisor's death plane the whole
    /// time (the run loop cannot service it mid-turn), so they all arrive in
    /// a burst right after this handler returns - the deliberately-delayed
    /// "straggler" fates the truth contract must never misattribute.
    StressLoop {
        name: String,
        iterations: u32,
    },
    /// Issues `terminate_child` but bounds the wait to (effectively) zero and
    /// drops the future the instant it elapses, exercising cancel-safety.
    TerminateAndAbandon {
        name: String,
    },
}

#[derive(Debug)]
enum SupReply {
    Done(Result<(), String>),
}

async fn spawn_worker(ctx: &mut ActorContext<Sup>, name: &str) -> Result<(), String> {
    ctx.spawn_child(Worker::default)
        .named(name.to_string())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_secs(2)))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// `terminate_child` -> `delete_child` -> `spawn_child` (same name), all
/// awaited sequentially inside the SAME async call - the truth contract this
/// whole suite is about.
async fn terminate_delete_respawn(ctx: &mut ActorContext<Sup>, name: &str) -> Result<(), String> {
    ctx.terminate_child(name.to_string())
        .await
        .map_err(|e| e.to_string())?;
    ctx.delete_child(name.to_string())
        .map_err(|e| e.to_string())?;
    spawn_worker(ctx, name).await
}

impl Actor for Sup {
    type Message = SupCmd;
    type Response = SupReply;

    async fn handle(&mut self, msg: SupCmd, ctx: &mut ActorContext<Self>) -> ActorResult<SupReply> {
        let reply = match msg {
            SupCmd::SpawnWorker { name } => {
                ctx.spawn_child(Worker::default)
                    .named(name)
                    .restart_type(RestartType::Permanent)
                    .shutdown(Shutdown::Timeout(Duration::from_secs(2)))
                    .await?;
                SupReply::Done(Ok(()))
            }
            SupCmd::TerminateDeleteRespawn { name } => {
                SupReply::Done(terminate_delete_respawn(ctx, &name).await)
            }
            SupCmd::StressLoop { name, iterations } => {
                let res = match spawn_worker(ctx, &name).await {
                    Ok(()) => {
                        let mut res = Ok(());
                        for _ in 1..iterations {
                            res = terminate_delete_respawn(ctx, &name).await;
                            if res.is_err() {
                                break;
                            }
                        }
                        res
                    }
                    Err(e) => Err(e),
                };
                SupReply::Done(res)
            }
            SupCmd::TerminateAndAbandon { name } => {
                // Polled exactly once by `timeout`'s own internal ordering
                // (it always polls the inner future before its own delay),
                // so the commit (ledger transition + lane raise) has already
                // happened by the time this drops the future on the elapsed
                // branch.
                let _ =
                    tokio::time::timeout(Duration::from_micros(1), ctx.terminate_child(name)).await;
                SupReply::Done(Ok(()))
            }
        };
        Ok(reply)
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
// Item 4: same-handler terminate -> delete -> respawn
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn same_handler_terminate_delete_respawn_succeeds_in_one_dispatch() {
    let events = recorder();
    let sup = Sup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-same-handler"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("worker");
    op(&sup, SupCmd::SpawnWorker { name: name.clone() })
        .await
        .expect("spawn worker");
    let first = wait_worker_ready(&name, 5_000).await.expect("worker up");
    assert_eq!(first.send(WorkerMsg::Bump).await.unwrap(), 1);

    // The whole terminate -> delete -> respawn cycle runs inside ONE
    // dispatched `handle` call: if `terminate_child` did not guarantee the
    // name was free the instant it returned, `delete_child` and the
    // following `spawn_child` (DuplicateChild / already_present) would fail
    // right here.
    op(&sup, SupCmd::TerminateDeleteRespawn { name: name.clone() })
        .await
        .expect("terminate -> delete -> respawn must all succeed in one handler invocation");

    let fresh = wait_worker_ready(&name, 5_000)
        .await
        .expect("the respawned child must come up under the same name");
    // The terminated incarnation is truly gone (its own handle now dead),
    // and the respawned one under the same name is genuinely FRESH state -
    // together, proof this is a new incarnation, not the terminated one
    // somehow still lingering under the same name.
    assert!(!first.is_alive(), "the terminated incarnation must be dead");
    assert_eq!(fresh.send(WorkerMsg::Count).await.unwrap(), 0);
}

// ---------------------------------------------------------------------------
// Item 4 (loop): incarnation-collision stress under load
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn incarnation_collision_stress_loop_never_marks_fresh_child_dead() {
    let events = recorder();
    let sup = Sup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-stress"))
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(0, Duration::from_secs(60)))
    .await
    .unwrap();

    let name = uname("stress-worker");
    const ITERATIONS: u32 = 30;

    // Every one of the first 29 incarnations' watcher completions is left
    // sitting, undrained, in the death plane while this ONE handler runs -
    // the run loop cannot service them mid-turn. They all arrive in a burst
    // right after this returns: a deliberately-delayed straggler fate racing
    // the FINAL, still-live incarnation on every single one of them.
    op(
        &sup,
        SupCmd::StressLoop {
            name: name.clone(),
            iterations: ITERATIONS,
        },
    )
    .await
    .expect("the whole stress loop must succeed in one handler invocation");

    // The straggler burst has to be given time to actually drain and
    // (wrongly, if the incarnation counter were not global/monotonic) try to
    // mark the fresh child dead.
    let fresh = wait_worker_ready(&name, 5_000)
        .await
        .expect("the final incarnation must be alive and responsive");
    for _ in 0..20 {
        sleep(Duration::from_millis(20)).await;
        assert_eq!(
            fresh.send(WorkerMsg::Count).await.unwrap(),
            0,
            "a stale straggler fate must never be mistaken for the fresh child's own death"
        );
    }

    // Every one of the (ITERATIONS - 1) terminated incarnations eventually
    // gets its manual completion delivered - none lost, none misattributed.
    let evs = wait_for_events(&events, 10_000, |e| {
        events_for(e, &name).len() as u32 >= ITERATIONS - 1
    })
    .await;
    let mine = events_for(&evs, &name);
    assert_eq!(
        mine.len() as u32,
        ITERATIONS - 1,
        "exactly (iterations - 1) manual completions expected, none lost/duplicated: {mine:?}"
    );
    assert!(
        mine.iter().all(|e| e.action == SupervisionAction::Removed),
        "every terminated incarnation reports Removed: {mine:?}"
    );
}

// ---------------------------------------------------------------------------
// Cancel-safety: dropping the caller's future still completes the stop
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn dropped_terminate_child_future_still_completes_and_fires_once() {
    let events = recorder();
    let sup = Sup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-drop"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("drop-worker");
    op(&sup, SupCmd::SpawnWorker { name: name.clone() })
        .await
        .expect("spawn worker");
    wait_worker_ready(&name, 5_000).await.expect("worker up");

    // The handler commits (ledger transition + lane raise) and then drops
    // its own await on the fate cell via a near-zero timeout: by construction
    // the run loop still completes the operation.
    op(&sup, SupCmd::TerminateAndAbandon { name: name.clone() })
        .await
        .expect("the command itself always returns Ok regardless of the abandoned wait");

    let sys = ActorSystem::default();
    assert!(
        wait_until(5_000, || sys.get::<Worker>(&name).is_none()).await,
        "the stop must still complete even though the caller's future was dropped"
    );

    let evs = wait_for_events(&events, 5_000, |e| !events_for(e, &name).is_empty()).await;
    let mine = events_for(&evs, &name);
    assert_eq!(
        mine.len(),
        1,
        "on_child_stopped must fire EXACTLY once despite the dropped future: {mine:?}"
    );
    assert_eq!(mine[0].action, SupervisionAction::Removed);

    // Idempotent by construction: nothing double-fires on a later, unrelated
    // turn either.
    sleep(Duration::from_millis(200)).await;
    let evs = events.lock().await.clone();
    assert_eq!(
        events_for(&evs, &name).len(),
        1,
        "still exactly one event after a quiet window: {evs:?}"
    );
}

// ---------------------------------------------------------------------------
// on_child_stopped exactly-once audit
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn on_child_stopped_fires_once_for_a_normal_manual_stop() {
    let events = recorder();
    let sup = Sup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-audit-manual"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("audit-manual-worker");
    op(&sup, SupCmd::SpawnWorker { name: name.clone() })
        .await
        .expect("spawn worker");
    wait_worker_ready(&name, 5_000).await.expect("worker up");

    op(&sup, SupCmd::TerminateDeleteRespawn { name: name.clone() })
        .await
        .expect("terminate -> delete -> respawn");

    let evs = wait_for_events(&events, 5_000, |e| !events_for(e, &name).is_empty()).await;
    assert_eq!(
        events_for(&evs, &name).len(),
        1,
        "exactly one event for the terminated incarnation, even with a \
         same-handler delete+respawn before the run loop's death arm ever \
         runs: {evs:?}"
    );

    // The respawned instance must never itself be reported dead here.
    sleep(Duration::from_millis(200)).await;
    let evs = events.lock().await.clone();
    assert_eq!(
        events_for(&evs, &name).len(),
        1,
        "the fresh incarnation must not spuriously accrue a second event: {evs:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn on_child_stopped_fires_once_for_a_natural_death() {
    let events = recorder();
    let sup = Sup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-audit-natural"))
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(0, Duration::from_secs(60)))
    .await
    .unwrap();

    let name = uname("audit-natural-worker");
    op(&sup, SupCmd::SpawnWorker { name: name.clone() })
        .await
        .expect("spawn worker");
    let worker = wait_worker_ready(&name, 5_000).await.expect("worker up");

    worker.notify(WorkerMsg::Crash).await.unwrap();

    let evs = wait_for_events(&events, 5_000, |e| !events_for(e, &name).is_empty()).await;
    assert_eq!(
        events_for(&evs, &name).len(),
        1,
        "exactly one event for a natural death: {evs:?}"
    );
    assert!(matches!(
        events_for(&evs, &name)[0].reason,
        StopReason::Failure(_)
    ));

    sleep(Duration::from_millis(200)).await;
    let evs = events.lock().await.clone();
    assert_eq!(
        events_for(&evs, &name).len(),
        1,
        "still exactly one: {evs:?}"
    );
}

// A child cascaded down by its parent's own `StopReason::Kill` teardown never
// gets an `on_child_stopped` callback at all: the parent drops its
// supervision state directly (see the run loop's Kill cascade-inversion
// path) instead of ever processing a death event for it.
#[tokio::test(flavor = "multi_thread")]
async fn on_child_stopped_is_skipped_for_a_child_killed_by_a_parent_cascade() {
    let events = recorder();
    let sup = Sup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-audit-kill"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("audit-kill-worker");
    op(&sup, SupCmd::SpawnWorker { name: name.clone() })
        .await
        .expect("spawn worker");
    wait_worker_ready(&name, 5_000).await.expect("worker up");

    sup.stop(StopReason::Kill).await.unwrap();

    let sys = ActorSystem::default();
    assert!(
        wait_until(5_000, || sys.get::<Worker>(&name).is_none()).await,
        "the cascaded child must be gone"
    );
    assert!(
        wait_until(5_000, || !sup.is_alive()).await,
        "the killed supervisor must be gone too"
    );

    // No event was ever recorded for the cascaded child: `on_child_stopped`
    // is skipped entirely for a Kill teardown, by construction.
    let evs = events.lock().await.clone();
    assert!(
        events_for(&evs, &name).is_empty(),
        "a Kill-cascaded child must not fire on_child_stopped: {evs:?}"
    );
}

// ---------------------------------------------------------------------------
// terminate_child override during an in-flight group stop: the truth
// contract (Ok implies delete_child + a same-name spawn_child succeed in the
// SAME handler) must hold even for a member whose disposition was overridden
// mid-group, not settled by the run loop's own ordinary death arm.
// ---------------------------------------------------------------------------

/// A child that never yields to a `Graceful`/`ParentRequest` stop (`pre_stop`
/// always vetoes), so its real death can only come from the Kill escalation
/// at the end of its `Shutdown::Timeout` ladder - long enough that it is
/// still sitting in a group's `awaiting` set when a same-handler override
/// targets it.
struct VetoWorker {
    stopped_calls: Arc<AtomicU64>,
}

impl Actor for VetoWorker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// `terminate_child` -> `delete_child` -> `spawn_child` (same name, a fresh
/// `Worker`), all awaited sequentially inside the SAME async call - the
/// group-override variant of [`terminate_delete_respawn`].
async fn terminate_delete_respawn_worker(
    ctx: &mut ActorContext<GSup>,
    name: &str,
) -> Result<(), String> {
    ctx.terminate_child(name.to_string())
        .await
        .map_err(|e| e.to_string())?;
    ctx.delete_child(name.to_string())
        .map_err(|e| e.to_string())?;
    ctx.spawn_child(Worker::default)
        .named(name.to_string())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_secs(2)))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// OneForAll supervisor of two members: `a` (a plain [`Worker`], the crash
/// trigger) and `b` (a [`VetoWorker`], still `awaiting` in the group's
/// `Stopping` phase when the test overrides its disposition mid-handler).
struct GSup {
    a: String,
    b: String,
    veto_stopped: Arc<AtomicU64>,
    events: EventLog,
}

#[derive(Clone)]
enum GCmd {
    /// Runs `terminate_child(b)` -> `delete_child(b)` -> `spawn_child(b)`
    /// (fresh `Worker`) inside this ONE handler invocation, while `b` is
    /// still a member of an in-flight group's `Stopping` phase.
    OverrideTerminateDuringGroup { name: String },
}

#[derive(Debug)]
enum GReply {
    Done(Result<(), String>),
}

impl Actor for GSup {
    type Message = GCmd;
    type Response = GReply;

    async fn handle(&mut self, msg: GCmd, ctx: &mut ActorContext<Self>) -> ActorResult<GReply> {
        let reply = match msg {
            GCmd::OverrideTerminateDuringGroup { name } => {
                GReply::Done(terminate_delete_respawn_worker(ctx, &name).await)
            }
        };
        Ok(reply)
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(Worker::default)
            .named(self.a.clone())
            .restart_type(RestartType::Permanent)
            .shutdown(Shutdown::Timeout(Duration::from_secs(2)))
            .await?;

        let veto_stopped = self.veto_stopped.clone();
        ctx.spawn_child(move || VetoWorker {
            stopped_calls: veto_stopped.clone(),
        })
        .named(self.b.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
        .await?;

        Ok(())
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

#[tokio::test(flavor = "multi_thread")]
async fn terminate_override_during_group_stop_honors_the_truth_contract() {
    let events = recorder();
    let a_name = uname("group-a");
    let b_name = uname("group-b");
    let veto_stopped = Arc::new(AtomicU64::new(0));

    let sup = GSup {
        a: a_name.clone(),
        b: b_name.clone(),
        veto_stopped: veto_stopped.clone(),
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-group-override"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60)))
    .await
    .unwrap();

    assert!(
        wait_named_alive::<Worker>(&a_name, 5_000).await,
        "a must start"
    );
    assert!(
        wait_named_alive::<VetoWorker>(&b_name, 5_000).await,
        "b must start"
    );

    let ha = ActorSystem::default().get::<Worker>(&a_name).unwrap();
    ha.notify(WorkerMsg::Crash)
        .await
        .expect("deliver crash to a");

    // Wait until the OneForAll group has formed around a's crash: a's own
    // ChildStopped event is recorded the instant the strategy is evaluated,
    // which is also the instant b's stop signal is raised and b enters the
    // group's `awaiting` set. b's `pre_stop` vetoes every stop request, so it
    // is still very much alive - and still `awaiting` - by the time this
    // resolves.
    wait_for_events(&events, 5_000, |e| !events_for(e, &a_name).is_empty()).await;

    let GReply::Done(res) = sup
        .send(GCmd::OverrideTerminateDuringGroup {
            name: b_name.clone(),
        })
        .await
        .expect("supervisor must answer");
    res.expect(
        "terminate -> delete -> respawn must all succeed in one handler invocation, \
         even for a member still awaited by an in-flight group stop",
    );

    // The overridden incarnation's `on_child_stopped` must fire EXACTLY
    // once - not lost, and not duplicated by the group's own generic
    // bookkeeping for this member.
    let evs = wait_for_events(&events, 5_000, |e| !events_for(e, &b_name).is_empty()).await;
    let b_events = events_for(&evs, &b_name);
    assert_eq!(
        b_events.len(),
        1,
        "on_child_stopped must fire exactly once for the overridden incarnation: {b_events:?}"
    );
    assert_eq!(b_events[0].action, SupervisionAction::Removed);

    // The group itself still completes: a is restarted to a fresh, live
    // instance even though its sibling's disposition was overridden mid-stop.
    assert!(
        wait_named_alive::<Worker>(&a_name, 5_000).await,
        "the group must still complete and restart a even though b was overridden"
    );

    // The respawned b is a genuinely fresh instance under the same name.
    let fresh_b = wait_worker_ready(&b_name, 5_000)
        .await
        .expect("the respawned b must come up under the same name");
    assert_eq!(fresh_b.send(WorkerMsg::Count).await.unwrap(), 0);

    // No spurious second event for b after a quiet window (the run loop
    // eventually drains the original watcher completion, but it must find
    // nothing left to do beyond what already fired above).
    sleep(Duration::from_millis(300)).await;
    let evs = events.lock().await.clone();
    assert_eq!(
        events_for(&evs, &b_name).len(),
        1,
        "no spurious second event for b after the quiet window: {evs:?}"
    );
}
