#![allow(dead_code)]
//! Behavioral suite for manual child management.
//!
//! - `ctx.terminate_child(id)`: stop WITHOUT restart, budget-free, spec kept
//!   unless Temporary/SimpleOneForOne (pruned); idempotent on dead children;
//!   bounded by Shutdown policy -> Kill escalation -> abort() backstop.
//! - `ctx.stop_child(id)`: the BOUNCE - Permanent children are restarted
//!   BUDGET-FREE; Transient/Temporary stay down.
//! - `ctx.restart_child(id)` / `ctx.delete_child(id)`: sync, typed errors
//!   ChildNotFound / ChildRunning / ChildRestarting.
//! - `SupervisionAction::RestartInitiated`.
//! - A supervised child's death reason is reported truthfully: a genuine
//!   `Cancelled` stays `Cancelled`; an aborted task is reported as `Kill`.
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
    ActorError, ActorHandle, ActorResult, ActorSystem, ChildEvent, ChildInfo, RestartType,
    Shutdown, StopChildOutcome, StopReason,
};

mod common;

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

/// Waits until the roster reads the named child alive with no restart
/// pending, which is after the supervisor adopted its latest incarnation: a
/// manual call made earlier, when the incarnation already answers but is not
/// adopted yet, is answered `ChildRestarting`.
async fn wait_adopted(sup: &ActorHandle<MgmtSup>, name: &str) {
    let snapshot = wait_roster(sup, 10_000, |children| {
        info_for(children, name).is_some_and(|info| info.is_alive && !info.restart_pending)
    })
    .await;
    assert!(
        info_for(&snapshot, name).is_some_and(|info| info.is_alive && !info.restart_pending),
        "{name} was never adopted: {snapshot:?}"
    );
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

/// How many times each vetoer, by name, was asked to stop.
static VETOES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
    std::sync::LazyLock::new(Default::default);

fn vetoes_of(name: &str) -> usize {
    VETOES.lock().unwrap().get(name).copied().unwrap_or(0)
}

impl Actor for Vetoer {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, ctx: &mut ActorContext<Self>) -> bool {
        if let Some(name) = ctx.actor_name() {
            *VETOES.lock().unwrap().entry(name.to_string()).or_default() += 1;
        }
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
    /// `stop_child`, answering with its outcome.
    StopOutcome {
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
    /// The outcome of a `stop_child`.
    Outcome(Result<StopChildOutcome, String>),
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
            SupCmd::Stop { id } => SupReply::Done(
                ctx.stop_child(id)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
            ),
            SupCmd::StopOutcome { id } => {
                SupReply::Outcome(ctx.stop_child(id).await.map_err(|e| e.to_string()))
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

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[test]
fn delete_child_lifecycle() {
    common::run(0, async {
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
    });
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The abort backstop's headline test
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A supervised child's death reason: genuine Cancelled stays Cancelled, an
// abort is reported as Kill
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// An `Err` from `on_child_stopped` stops the supervisor, exactly like `handle`
// ---------------------------------------------------------------------------

/// A supervisor whose `on_child_stopped` either fails deliberately or
/// succeeds, depending on `fails`. Captures the last error and the reason
/// `on_stopped` saw externally, the same pattern as `ctx.last_error()`
/// captured through `on_stopped` elsewhere in this crate's test suite.
struct OnChildStoppedSup {
    fails: bool,
    child_name: String,
    captured_error: Arc<std::sync::Mutex<Option<ActorError>>>,
    captured_reason: Arc<std::sync::Mutex<Option<StopReason>>>,
}

impl Actor for OnChildStoppedSup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(Worker::default)
            .named(self.child_name.clone())
            .restart_type(RestartType::Temporary)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        _event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if self.fails {
            Err(ActorError::user("boom"))
        } else {
            Ok(())
        }
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.captured_error.lock().unwrap() = ctx.last_error().cloned();
        *self.captured_reason.lock().unwrap() = Some(reason.clone());
        Ok(())
    }
}
