//! Behavioral suite for manual child management.
//!
//! - `ctx.terminate_child(id)`: stop WITHOUT restart, budget-free, spec kept
//!   unless Temporary/SimpleOneForOne (pruned); idempotent on dead children;
//!   bounded by Shutdown policy -> Kill escalation -> abort() backstop.
//! - `ctx.stop_child(id)`: the BOUNCE - Permanent children are restarted
//!   BUDGET-FREE; Transient/Temporary stay down.
//! - `ctx.restart_child(id)` / `ctx.delete_child(id)`: sync, typed errors
//!   ChildNotFound / ChildRunning / ChildRestarting.
//! - `SupervisionAction::RestartInitiated` (renamed from `Restarted`).
//! - Cancelled -> Kill translation for supervised children.
//!
//! Manual operations run inside actor callbacks only, so every test drives
//! them through a command-driven supervisor whose `Response` carries the
//! outcome (`SupervisionError` is not `PartialEq`; errors travel as their
//! Display strings and tests assert substrings like "is running").
//!
//! All tests use event-based waiting (poll with a deadline) instead of fixed
//! sleeps wherever the condition is observable; fixed sleeps appear only as
//! NEGATIVE quiet windows ("no restart happened") and hang-arming delays.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{sleep, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorSystem, ChildEvent, ChildInfo, RestartType, Shutdown,
    StopReason, SupervisionAction, SupervisionConfig,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type EventLog = Arc<tokio::sync::Mutex<Vec<ChildEvent>>>;

fn recorder() -> EventLog {
    Arc::new(tokio::sync::Mutex::new(Vec::new()))
}

/// Unique registry names so tests can run in parallel within one process.
static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("mgmt-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

/// Polls a synchronous predicate until it holds or the deadline passes.
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

/// Polls the recorder until `pred` holds (or the deadline passes) and returns a snapshot.
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

/// Re-looks a named `Worker` up until a FRESH instance responds (counter == 0).
///
/// A fresh counter doubles as proof of a completed restart: a pre-stop
/// instance with a bumped counter can never satisfy this predicate.
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

/// Sends a manual-management command and unwraps the outcome envelope.
async fn op(sup: &ActorHandle<MgmtSup>, cmd: SupCmd) -> Result<(), String> {
    match sup.send(cmd).await.expect("supervisor must answer") {
        SupReply::Done(res) => res,
        other => panic!("expected SupReply::Done, got {other:?}"),
    }
}

/// Fetches the supervisor's child registry snapshot via `ctx.children()`.
async fn roster(sup: &ActorHandle<MgmtSup>) -> Vec<ChildInfo> {
    match sup
        .send(SupCmd::Children)
        .await
        .expect("supervisor must answer")
    {
        SupReply::Roster(children) => children,
        other => panic!("expected SupReply::Roster, got {other:?}"),
    }
}

/// Polls the registry snapshot until `pred` holds (or the deadline passes)
/// and returns the last snapshot. Registry bookkeeping may lag a manual op by
/// one system-channel event, so roster assertions always go through here.
async fn wait_roster<P>(sup: &ActorHandle<MgmtSup>, timeout_ms: u64, pred: P) -> Vec<ChildInfo>
where
    P: Fn(&[ChildInfo]) -> bool,
{
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let snapshot = roster(sup).await;
        if pred(&snapshot) {
            return snapshot;
        }
        if Instant::now() >= deadline {
            return snapshot;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn event_for<'e>(events: &'e [ChildEvent], name: &str) -> Option<&'e ChildEvent> {
    events
        .iter()
        .find(|ev| ev.child_name.as_deref() == Some(name))
}

fn info_for<'r>(children: &'r [ChildInfo], name: &str) -> Option<&'r ChildInfo> {
    children.iter().find(|c| c.name.as_deref() == Some(name))
}

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

/// A worker with a counter (fresh-state probe) that crashes or wedges on command.
#[derive(Default, Debug)]
struct Worker {
    count: u32,
}

#[derive(Clone)]
enum WorkerMsg {
    Bump,
    Count,
    Crash,
    /// Parks the handler on a never-ready future: the actor never returns to
    /// its message loop, so even a Kill signal can never be processed. Only
    /// the abort() backstop can reap it.
    Hang,
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
            WorkerMsg::Hang => {
                std::future::pending::<()>().await;
                unreachable!("pending() never resolves")
            }
        }
    }
}

/// A child that vetoes every stoppable stop: `pre_stop` returns `false`.
/// Only the Shutdown-Timeout -> Kill escalation can bring it down.
#[derive(Default)]
struct Vetoer;

impl Actor for Vetoer {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }
}

/// A child whose `pre_stop` always panics: a deterministic stand-in for "a
/// real crash lands at the exact moment a manual stop's signal arrives".
/// Any vetoable stop signal (`ParentRequest`, from `Shutdown::Timeout` or
/// `Infinity`) drives this into a REAL `StopReason::Failure` regardless of
/// the caller's intent - exactly the race `manual_stop_child` must classify
/// on the observed fate rather than the intent.
#[derive(Default)]
struct PanicsOnStop;

impl Actor for PanicsOnStop {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        panic!("deliberate pre_stop panic simulating a racing crash");
    }
}

/// The command-driven supervisor: records every `ChildEvent`, spawns children
/// and executes manual child-management ops on command, returning the outcome.
struct MgmtSup {
    events: EventLog,
}

#[derive(Clone)]
enum SupCmd {
    SpawnWorker {
        name: String,
        restart_type: RestartType,
        shutdown: Shutdown,
    },
    SpawnVetoer {
        name: String,
        timeout_ms: u64,
    },
    SpawnPanicsOnStop {
        name: String,
        restart_type: RestartType,
    },
    Terminate {
        id: String,
    },
    Stop {
        id: String,
    },
    Restart {
        id: String,
    },
    Delete {
        id: String,
    },
    Children,
}

#[derive(Debug)]
enum SupReply {
    /// Outcome of a spawn or manual op; errors travel as Display strings.
    Done(Result<(), String>),
    /// Snapshot of `ctx.children()`.
    Roster(Vec<ChildInfo>),
}

impl Actor for MgmtSup {
    type Message = SupCmd;
    type Response = SupReply;

    async fn handle(&mut self, msg: SupCmd, ctx: &mut ActorContext<Self>) -> ActorResult<SupReply> {
        let reply = match msg {
            SupCmd::SpawnWorker {
                name,
                restart_type,
                shutdown,
            } => {
                ctx.spawn_child(Worker::default)
                    .named(name)
                    .restart_type(restart_type)
                    .shutdown(shutdown)
                    .await?;
                SupReply::Done(Ok(()))
            }
            SupCmd::SpawnVetoer { name, timeout_ms } => {
                ctx.spawn_child(Vetoer::default)
                    .named(name)
                    .restart_type(RestartType::Permanent)
                    .shutdown(Shutdown::Timeout(Duration::from_millis(timeout_ms)))
                    .await?;
                SupReply::Done(Ok(()))
            }
            SupCmd::SpawnPanicsOnStop { name, restart_type } => {
                ctx.spawn_child(PanicsOnStop::default)
                    .named(name)
                    .restart_type(restart_type)
                    .shutdown(Shutdown::Timeout(Duration::from_secs(2)))
                    .await?;
                SupReply::Done(Ok(()))
            }
            SupCmd::Terminate { id } => {
                SupReply::Done(ctx.terminate_child(id).await.map_err(|e| e.to_string()))
            }
            SupCmd::Stop { id } => {
                SupReply::Done(ctx.stop_child(id).await.map_err(|e| e.to_string()))
            }
            SupCmd::Restart { id } => {
                SupReply::Done(ctx.restart_child(id).map_err(|e| e.to_string()))
            }
            SupCmd::Delete { id } => {
                SupReply::Done(ctx.delete_child(id).map_err(|e| e.to_string()))
            }
            SupCmd::Children => SupReply::Roster(ctx.children()),
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
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn terminate_child_stops_without_restart() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-term"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("term-worker");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name.clone(),
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn worker");
    wait_worker_ready(&name, 5_000).await.expect("worker up");

    // Terminate: stop WITHOUT restart (OTP terminate_child).
    op(&sup, SupCmd::Terminate { id: name.clone() })
        .await
        .expect("terminate_child must return Ok");

    // Dead: the registered name is released.
    let sys = ActorSystem::default();
    assert!(
        wait_until(5_000, || sys.get::<Worker>(&name).is_none()).await,
        "terminated child must disappear from the name registry"
    );

    // The death event: ParentRequest + Removed (manual death bypasses strategy).
    let evs = wait_for_events(&events, 5_000, |e| event_for(e, &name).is_some()).await;
    let ev = event_for(&evs, &name).unwrap_or_else(|| panic!("no death event: {evs:?}"));
    assert!(
        matches!(ev.reason, StopReason::ParentRequest),
        "terminate reason must be ParentRequest, got {:?}",
        ev.reason
    );
    assert_eq!(
        ev.action,
        SupervisionAction::Removed,
        "terminate_child must NOT initiate a restart"
    );

    // Quiet window: a PERMANENT child stays down after a manual terminate.
    sleep(Duration::from_millis(300)).await;
    assert!(
        sys.get::<Worker>(&name).is_none(),
        "child must still be down after the quiet window"
    );
    let evs = events.lock().await.clone();
    assert!(
        evs.iter()
            .all(|e| e.action != SupervisionAction::RestartInitiated),
        "no restart may be initiated: {evs:?}"
    );
    assert_eq!(evs.len(), 1, "exactly one death event: {evs:?}");

    // Spec KEPT (restartable later via restart_child): entry present, dead, idle.
    let snapshot = wait_roster(&sup, 5_000, |r| {
        info_for(r, &name).is_some_and(|c| !c.is_alive && !c.restart_pending)
    })
    .await;
    let info = info_for(&snapshot, &name)
        .unwrap_or_else(|| panic!("spec must be KEPT after terminate_child: {snapshot:?}"));
    assert!(!info.is_alive, "child must be recorded dead: {info:?}");
    assert!(!info.restart_pending, "no restart pending: {info:?}");
    assert_eq!(info.restart_type, RestartType::Permanent);
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn terminate_child_prunes_temporary() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-temp"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("temp-worker");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name.clone(),
            restart_type: RestartType::Temporary,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn worker");
    wait_worker_ready(&name, 5_000).await.expect("worker up");

    op(&sup, SupCmd::Terminate { id: name.clone() })
        .await
        .expect("terminate_child must return Ok");

    // OTP: a Temporary child's spec is deleted as soon as the process
    // terminates - the registry must no longer contain it.
    let snapshot = wait_roster(&sup, 5_000, |r| info_for(r, &name).is_none()).await;
    assert!(
        info_for(&snapshot, &name).is_none(),
        "Temporary spec must be pruned on terminate: {snapshot:?}"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// The death event's `child_name` must be captured BEFORE the spec is pruned:
// a Temporary child's spec is removed as part of handling the SAME manual
// terminate that produces this event, so looking the name up afterward would
// spuriously see `None` (the entry is already gone).
#[tokio::test(flavor = "multi_thread")]
async fn terminate_child_event_carries_name_even_when_the_spec_is_pruned() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-temp-name"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("temp-worker-name");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name.clone(),
            restart_type: RestartType::Temporary,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn worker");
    wait_worker_ready(&name, 5_000).await.expect("worker up");

    op(&sup, SupCmd::Terminate { id: name.clone() })
        .await
        .expect("terminate_child must return Ok");

    let evs = wait_for_events(&events, 5_000, |e| event_for(e, &name).is_some()).await;
    let ev = event_for(&evs, &name).unwrap_or_else(|| panic!("no death event: {evs:?}"));
    assert_eq!(
        ev.child_name.as_deref(),
        Some(name.as_str()),
        "the pruned Temporary spec must not make child_name spuriously None"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn stop_child_bounce_restarts_permanent_budget_free() {
    let events = recorder();
    // Budget of exactly ONE restart per window: if the two bounces below
    // charged it, the real crash afterwards would exhaust the supervisor
    // instead of restarting the child.
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-bounce"))
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)))
    .await
    .unwrap();

    let name = uname("bounce-worker");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name.clone(),
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn worker");

    // Two bounces. Each round dirties the counter first so the fresh
    // incarnation (counter back to 0) is distinguishable from the old one.
    for round in 1..=2u32 {
        let worker = wait_worker_ready(&name, 10_000)
            .await
            .unwrap_or_else(|| panic!("bounce {round}: worker never became ready"));
        assert_eq!(worker.send(WorkerMsg::Bump).await.unwrap(), 1);

        op(&sup, SupCmd::Stop { id: name.clone() })
            .await
            .unwrap_or_else(|e| panic!("bounce {round}: stop_child failed: {e}"));
    }

    // One REAL crash: the single budget slot must be untouched by the bounces.
    let worker = wait_worker_ready(&name, 10_000)
        .await
        .expect("worker must be back after bounce 2");
    worker.notify(WorkerMsg::Crash).await.unwrap();
    assert!(
        wait_worker_ready(&name, 10_000).await.is_some(),
        "a crash after two bounces must STILL restart - bounces must not charge the budget"
    );
    assert!(
        sup.is_alive(),
        "supervisor must not have exhausted its budget"
    );

    // Bounce deaths are ParentRequest and carry RestartInitiated.
    let evs = wait_for_events(&events, 10_000, |e| e.len() >= 3).await;
    let bounces: Vec<_> = evs
        .iter()
        .filter(|e| matches!(e.reason, StopReason::ParentRequest))
        .collect();
    assert_eq!(bounces.len(), 2, "two bounce deaths expected: {evs:?}");
    for ev in bounces {
        assert_eq!(
            ev.action,
            SupervisionAction::RestartInitiated,
            "a Permanent bounce must initiate a (budget-free) restart: {ev:?}"
        );
    }
    assert!(
        evs.iter()
            .any(|e| matches!(e.reason, StopReason::Failure(_))
                && e.action == SupervisionAction::RestartInitiated),
        "the real crash must be restarted on the budget: {evs:?}"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn stop_child_transient_stays_down() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-transient"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("transient-worker");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name.clone(),
            restart_type: RestartType::Transient,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn worker");
    wait_worker_ready(&name, 5_000).await.expect("worker up");

    op(&sup, SupCmd::Stop { id: name.clone() })
        .await
        .expect("stop_child must return Ok");

    // Transient x ParentRequest = clean exit: down, not bounced.
    let sys = ActorSystem::default();
    assert!(
        wait_until(5_000, || sys.get::<Worker>(&name).is_none()).await,
        "stopped Transient child must leave the name registry"
    );
    let evs = wait_for_events(&events, 5_000, |e| event_for(e, &name).is_some()).await;
    let ev = event_for(&evs, &name).unwrap_or_else(|| panic!("no death event: {evs:?}"));
    assert_eq!(
        ev.action,
        SupervisionAction::Removed,
        "a Transient manual stop must not restart"
    );

    // Quiet window: still down, no restart initiated.
    sleep(Duration::from_millis(300)).await;
    assert!(
        sys.get::<Worker>(&name).is_none(),
        "Transient child must stay down"
    );
    let evs = events.lock().await.clone();
    assert!(
        evs.iter()
            .all(|e| e.action != SupervisionAction::RestartInitiated),
        "no restart may be initiated for a stopped Transient child: {evs:?}"
    );

    // Spec kept: a clean Transient exit keeps the entry (restartable later).
    let snapshot = wait_roster(&sup, 5_000, |r| {
        info_for(r, &name).is_some_and(|c| !c.is_alive && !c.restart_pending)
    })
    .await;
    let info = info_for(&snapshot, &name)
        .unwrap_or_else(|| panic!("Transient spec must be kept: {snapshot:?}"));
    assert!(!info.is_alive);
    assert!(!info.restart_pending);
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn delete_child_lifecycle() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-delete"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("delete-worker");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name.clone(),
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn worker");
    wait_worker_ready(&name, 5_000).await.expect("worker up");

    // delete_child on a RUNNING child -> ChildRunning (OTP: "must not be running").
    let err = op(&sup, SupCmd::Delete { id: name.clone() })
        .await
        .expect_err("delete_child on a running child must fail");
    assert!(
        err.contains("is running"),
        "expected ChildRunning, got: {err}"
    );

    // terminate_child then delete_child -> Ok (the OTP composition).
    op(&sup, SupCmd::Terminate { id: name.clone() })
        .await
        .expect("terminate_child must return Ok");
    op(&sup, SupCmd::Delete { id: name.clone() })
        .await
        .expect("delete_child after terminate_child must return Ok");

    // The spec is gone: this supervisor's registry is now empty.
    let snapshot = wait_roster(&sup, 5_000, |r| r.is_empty()).await;
    assert!(
        snapshot.is_empty(),
        "children() must be empty after delete_child: {snapshot:?}"
    );

    // restart_child on a deleted spec -> ChildNotFound.
    let err = op(&sup, SupCmd::Restart { id: name.clone() })
        .await
        .expect_err("restart_child after delete_child must fail");
    assert!(
        err.contains("not found"),
        "expected ChildNotFound, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn restart_child_revives_terminated() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-revive"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("revive-worker");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name.clone(),
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn worker");

    // Dirty the state so revival with FRESH state is provable.
    let worker = wait_worker_ready(&name, 5_000).await.expect("worker up");
    assert_eq!(worker.send(WorkerMsg::Bump).await.unwrap(), 1);
    assert_eq!(worker.send(WorkerMsg::Bump).await.unwrap(), 2);

    op(&sup, SupCmd::Terminate { id: name.clone() })
        .await
        .expect("terminate_child must return Ok");

    // Let the death fully settle (name released, registry marked dead) before
    // restarting, so this test does not exercise the Edge-20 window race.
    let sys = ActorSystem::default();
    assert!(
        wait_until(5_000, || sys.get::<Worker>(&name).is_none()).await,
        "terminated child must leave the name registry"
    );
    wait_roster(&sup, 5_000, |r| {
        info_for(r, &name).is_some_and(|c| !c.is_alive && !c.restart_pending)
    })
    .await;

    // restart_child initiates and returns Ok; completion observed by lookup.
    op(&sup, SupCmd::Restart { id: name.clone() })
        .await
        .expect("restart_child on a terminated child must return Ok");
    let fresh = wait_worker_ready(&name, 10_000)
        .await
        .expect("restart_child must revive the child under the same name");
    assert_eq!(
        fresh.send(WorkerMsg::Count).await.unwrap(),
        0,
        "revived child must carry FRESH state"
    );

    // Once the restart is fully adopted, a second restart_child sees a
    // RUNNING child (waiting out restart_pending avoids ChildRestarting).
    wait_roster(&sup, 5_000, |r| {
        info_for(r, &name).is_some_and(|c| c.is_alive && !c.restart_pending)
    })
    .await;
    let err = op(&sup, SupCmd::Restart { id: name.clone() })
        .await
        .expect_err("restart_child on a running child must fail");
    assert!(
        err.contains("is running"),
        "expected ChildRunning, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn terminate_vetoing_child_escalates() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-veto"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("vetoer");
    op(
        &sup,
        SupCmd::SpawnVetoer {
            name: name.clone(),
            timeout_ms: 150,
        },
    )
    .await
    .expect("spawn vetoer");
    let sys = ActorSystem::default();
    assert!(
        wait_until(5_000, || sys.get::<Vetoer>(&name).is_some()).await,
        "vetoer must come up"
    );

    // The veto (pre_stop -> false) cannot hold off Shutdown::Timeout(150ms):
    // Kill is escalated at expiry, so terminate_child stays BOUNDED.
    let t0 = Instant::now();
    op(&sup, SupCmd::Terminate { id: name.clone() })
        .await
        .expect("terminate_child must return Ok despite the veto");
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "terminate of a vetoing child must stay bounded (150ms timeout + \
         kill grace, plus CI scheduling slack), took {elapsed:?}"
    );

    // Dead for real (killed at escalation), and recorded as Removed.
    assert!(
        wait_until(5_000, || sys.get::<Vetoer>(&name).is_none()).await,
        "vetoing child must be dead after escalation"
    );
    let evs = wait_for_events(&events, 5_000, |e| event_for(e, &name).is_some()).await;
    let ev = event_for(&evs, &name).unwrap_or_else(|| panic!("no death event: {evs:?}"));
    assert_eq!(
        ev.action,
        SupervisionAction::Removed,
        "manual terminate must not restart, even after escalation"
    );
}

// ---------------------------------------------------------------------------
// The abort backstop's headline test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn terminate_hung_child_abort_backstop() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-hang"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("hung-worker");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name.clone(),
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::Timeout(Duration::from_millis(150)),
        },
    )
    .await
    .expect("spawn worker");
    let worker = wait_worker_ready(&name, 5_000).await.expect("worker up");

    // Wedge the handler on a never-ready future, then give it time to get
    // stuck: from here on, no message (not even Kill) can ever be processed.
    worker.notify(WorkerMsg::Hang).await.unwrap();
    sleep(Duration::from_millis(100)).await;

    // ParentRequest is never seen, Kill at 150ms is never seen; only the
    // abort() after KILL_GRACE reaps the task. terminate_child must still
    // return Ok, bounded well under 2s wall time.
    let t0 = Instant::now();
    op(&sup, SupCmd::Terminate { id: name.clone() })
        .await
        .expect("terminate_child must return Ok via the abort backstop");
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "abort backstop must bound a hung child's terminate (150ms timeout \
         + 100ms grace expected, plus CI scheduling slack), took {elapsed:?}"
    );

    // Dead for real.
    let sys = ActorSystem::default();
    assert!(
        wait_until(5_000, || sys.get::<Worker>(&name).is_none()).await,
        "hung child must be gone after the abort"
    );

    // The watcher observed an aborted task (JoinError::is_cancelled) and the
    // supervision layer translates in-tree Cancelled to Kill.
    let evs = wait_for_events(&events, 5_000, |e| event_for(e, &name).is_some()).await;
    let ev = event_for(&evs, &name).unwrap_or_else(|| panic!("no death event: {evs:?}"));
    assert!(
        matches!(ev.reason, StopReason::Kill),
        "an aborted supervised child must be reported as Kill, got {:?}",
        ev.reason
    );
    assert_eq!(
        ev.action,
        SupervisionAction::Removed,
        "manual death must not restart"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn handle_equality_stable_across_restart() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-handle-eq"))
    .supervisor()
    .await
    .unwrap();

    let name = uname("handle-eq-worker");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name.clone(),
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn worker");

    let before = wait_worker_ready(&name, 5_000).await.expect("worker up");
    before.notify(WorkerMsg::Crash).await.unwrap();

    let after = wait_worker_ready(&name, 10_000)
        .await
        .expect("worker must be back after the crash-triggered restart");

    assert_eq!(
        before.id(),
        after.id(),
        "a restart must preserve the ActorId"
    );
    assert_eq!(
        before, after,
        "same id + same system must stay equal across a restart even \
         though the handle's underlying channels were replaced"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn manual_ops_error_during_pending_group() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-group"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60)))
    .await
    .unwrap();

    // Three children; the middle one vetoes with a LONG shutdown timeout
    // (800ms), pinning the group's stop phase open long enough to observe it.
    let name_a = uname("group-a");
    let name_veto = uname("group-veto");
    let name_c = uname("group-c");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name_a.clone(),
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn a");
    op(
        &sup,
        SupCmd::SpawnVetoer {
            name: name_veto.clone(),
            timeout_ms: 800,
        },
    )
    .await
    .expect("spawn vetoer");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name_c.clone(),
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn c");

    let worker_a = wait_worker_ready(&name_a, 5_000).await.expect("a up");
    let sys = ActorSystem::default();
    assert!(
        wait_until(5_000, || sys.get::<Vetoer>(&name_veto).is_some()).await,
        "vetoer must come up"
    );
    wait_worker_ready(&name_c, 5_000).await.expect("c up");

    // Crash `a`: OneForAll pulls the whole group into a restart. The vetoer
    // holds the stop phase open for ~800ms (+ kill grace), so 100ms after the
    // crash the group is reliably mid-flight.
    worker_a.notify(WorkerMsg::Crash).await.unwrap();
    sleep(Duration::from_millis(100)).await;

    // A Bounce (stop_child) on a member of the pending group must still be
    // refused: unlike Terminate (see
    // `terminate_during_group_stop_overrides_default_restart`), a Bounce
    // carries no override semantics, so it stays rejected while the group's
    // own state machine owns this member.
    match op(
        &sup,
        SupCmd::Stop {
            id: name_veto.clone(),
        },
    )
    .await
    {
        Err(msg) => assert!(
            msg.contains("is restarting"),
            "a Bounce on a pending-group member must be ChildRestarting, got: {msg}"
        ),
        // Timing tolerance: on a pathologically slow runner the group may
        // already have completed, making the stop legitimately succeed.
        // The 800ms veto window makes this the rare path.
        Ok(()) => eprintln!(
            "group completed before the manual op could race it; accepting Ok \
             (timing-tolerant path)"
        ),
    }

    assert!(
        sup.is_alive(),
        "supervisor must stay alive through group restart + refused manual op"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// A caller's `terminate_child` on a group member still awaiting its own
// death (the group's Stopping phase) is honored over the group's default
// restart: the member ends up Removed instead of rejoining the restart
// chain, and the group still completes for its other members.
#[tokio::test(flavor = "multi_thread")]
async fn terminate_during_group_stop_overrides_default_restart() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-group-override"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60)))
    .await
    .unwrap();

    let name_a = uname("override-a");
    let name_veto = uname("override-veto");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: name_a.clone(),
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn a");
    op(
        &sup,
        SupCmd::SpawnVetoer {
            name: name_veto.clone(),
            timeout_ms: 800,
        },
    )
    .await
    .expect("spawn vetoer");

    let worker_a = wait_worker_ready(&name_a, 5_000).await.expect("a up");
    let sys = ActorSystem::default();
    assert!(
        wait_until(5_000, || sys.get::<Vetoer>(&name_veto).is_some()).await,
        "vetoer must come up"
    );

    // Crash `a`: OneForAll pulls the group (both members) into a Stopping
    // phase. The vetoer holds it open for ~800ms, giving a wide window to
    // land the override.
    worker_a.notify(WorkerMsg::Crash).await.unwrap();
    sleep(Duration::from_millis(100)).await;

    // The override: Terminate wins over the group's default restart for
    // this specific member. It still blocks until the vetoer is actually
    // gone (escalated Kill after the 800ms timeout).
    let t0 = Instant::now();
    op(
        &sup,
        SupCmd::Terminate {
            id: name_veto.clone(),
        },
    )
    .await
    .expect("terminate_child must be honored during the group's Stopping phase");
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "the override must still be bounded by the vetoer's own escalation ladder"
    );

    // The group still completes: `a` comes back up with fresh state.
    let revived_a = wait_worker_ready(&name_a, 10_000)
        .await
        .expect("the group must still complete for its other member");
    assert_eq!(revived_a.send(WorkerMsg::Count).await.unwrap(), 0);

    // The overridden member is Removed, not restarted, and stays down.
    let evs = wait_for_events(&events, 10_000, |e| event_for(e, &name_veto).is_some()).await;
    let ev = event_for(&evs, &name_veto)
        .unwrap_or_else(|| panic!("no death event for the overridden member: {evs:?}"));
    assert_eq!(
        ev.action,
        SupervisionAction::Removed,
        "a Terminate override must report Removed, not RestartInitiated: {ev:?}"
    );
    assert!(
        sys.get::<Vetoer>(&name_veto).is_none(),
        "the overridden member must stay down"
    );
    sleep(Duration::from_millis(200)).await;
    assert!(
        sys.get::<Vetoer>(&name_veto).is_none(),
        "the overridden member must not be revived by the group's own chain"
    );
    assert!(sup.is_alive());
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// A `terminate_child` racing a child that independently crashes at the exact
// moment the stop signal lands (real `StopReason::Failure`, via a `pre_stop`
// panic) absorbs the crash into the manual completion: no budget charge, no
// restart - exactly like a clean terminate.
#[tokio::test(flavor = "multi_thread")]
async fn terminate_child_absorbs_a_racing_failure() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-race-terminate"))
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)))
    .await
    .unwrap();

    let name = uname("race-terminate-worker");
    op(
        &sup,
        SupCmd::SpawnPanicsOnStop {
            name: name.clone(),
            restart_type: RestartType::Permanent,
        },
    )
    .await
    .expect("spawn worker");
    let sys = ActorSystem::default();
    assert!(
        wait_until(5_000, || sys.get::<PanicsOnStop>(&name).is_some()).await,
        "worker up"
    );

    // The commit raises ParentRequest (Shutdown::Timeout); `pre_stop` panics
    // instead of honoring it, so the REAL observed reason is Failure even
    // though our intent was Terminate.
    op(&sup, SupCmd::Terminate { id: name.clone() })
        .await
        .expect("terminate_child must absorb the racing crash and return Ok");

    let evs = wait_for_events(&events, 5_000, |e| event_for(e, &name).is_some()).await;
    let ev = event_for(&evs, &name).unwrap_or_else(|| panic!("no death event: {evs:?}"));
    assert!(
        matches!(ev.reason, StopReason::Failure(_)),
        "the REAL reason must be reported, not the intent: {:?}",
        ev.reason
    );
    assert_eq!(
        ev.action,
        SupervisionAction::Removed,
        "Terminate absorbs a racing Failure: no restart"
    );
    assert_eq!(evs.len(), 1, "exactly one death event: {evs:?}");
    assert!(
        sys.get::<PanicsOnStop>(&name).is_none(),
        "absorbed Terminate must not restart the child"
    );

    // The budget was NOT charged: a second, unrelated crash on a fresh
    // sibling must still be allowed to restart under the same max_restarts=1
    // supervisor.
    let sibling = uname("race-terminate-sibling");
    op(
        &sup,
        SupCmd::SpawnWorker {
            name: sibling.clone(),
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::Timeout(Duration::from_secs(1)),
        },
    )
    .await
    .expect("spawn sibling");
    let sibling_handle = wait_worker_ready(&sibling, 5_000)
        .await
        .expect("sibling up");
    sibling_handle.notify(WorkerMsg::Crash).await.unwrap();
    assert!(
        wait_worker_ready(&sibling, 10_000).await.is_some(),
        "a fresh crash must still restart - the absorbed racing Failure must not have \
         consumed the shared restart budget"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// A `stop_child` (Bounce) racing a child that crashes at the exact moment
// the stop signal lands (real `StopReason::Failure`, via a `pre_stop` panic)
// is NOT treated as a manual bounce: the real Failure routes to ordinary,
// budget-charged strategy evaluation instead.
#[tokio::test(flavor = "multi_thread")]
async fn stop_child_bounce_racing_failure_charges_the_budget() {
    let events = recorder();
    let sup = MgmtSup {
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup-race-bounce"))
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)))
    .await
    .unwrap();

    let name = uname("race-bounce-worker");
    op(
        &sup,
        SupCmd::SpawnPanicsOnStop {
            name: name.clone(),
            restart_type: RestartType::Permanent,
        },
    )
    .await
    .expect("spawn worker");
    let sys = ActorSystem::default();
    assert!(
        wait_until(5_000, || sys.get::<PanicsOnStop>(&name).is_some()).await,
        "worker up"
    );

    op(&sup, SupCmd::Stop { id: name.clone() })
        .await
        .expect("stop_child must still return Ok once the real fate is observed");

    // Routed through ordinary strategy evaluation: restarted ON THE BUDGET,
    // reported with the REAL Failure reason (not a synthetic ParentRequest).
    assert!(
        wait_until(10_000, || sys.get::<PanicsOnStop>(&name).is_some()).await,
        "a racing Failure under Bounce must still be restarted by strategy"
    );

    let evs = wait_for_events(&events, 10_000, |e| event_for(e, &name).is_some()).await;
    let ev = event_for(&evs, &name).unwrap_or_else(|| panic!("no death event: {evs:?}"));
    assert!(
        matches!(ev.reason, StopReason::Failure(_)),
        "a racing Failure under Bounce reports the REAL reason: {:?}",
        ev.reason
    );
    assert_eq!(
        ev.action,
        SupervisionAction::RestartInitiated,
        "a racing Failure under Bounce still restarts, on the budget: {ev:?}"
    );

    // The single restart slot is now spent: a second manual stop (which will
    // ALSO panic in pre_stop, i.e. another real Failure) must exhaust the
    // budget and stop the supervisor - proof the first one DID charge it.
    let _ = op(&sup, SupCmd::Stop { id: name.clone() }).await;
    assert!(
        wait_until(10_000, || !sup.is_alive()).await,
        "the budget must already be exhausted by the racing-Failure bounce"
    );
}
