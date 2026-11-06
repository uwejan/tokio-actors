#![allow(dead_code)]
//! Behavioral suite for the spawn ack.
//!
//! - Default `spawn().await` does not return until `pre_start` AND
//!   `on_started` have both run (OTP `start_link` parity); an `Err` from
//!   either surfaces as `SpawnError::Init`.
//! - The failure ack fires only after the registry guard has dropped, so an
//!   immediate same-name respawn can never observe `NameTaken` (OTP 26
//!   corpse-consumed parity) - the load-bearing invariant under test.
//! - `.detached()` reproduces the exact v0.7.1 fire-and-forget behavior.
//! - `.start_timeout(dur)` bounds the wait; on expiry the task is aborted
//!   with no lifecycle hook running afterward.
//! - When a SUPERVISOR's `on_started` spawns children via `ctx.spawn_child`
//!   and then fails, the shared teardown tail's Phase 5 (`stop_all_children`)
//!   still runs: pre-spawned children are stopped, not orphaned, even though
//!   the supervisor's own `on_stopped` is skipped (self-cleaning contract).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorId, ActorResult, ActorStatus, ActorSystem, ChildEvent, RestartType,
    SpawnError, StopReason,
};

mod common;

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

type Log = Arc<Mutex<Vec<&'static str>>>;

/// Records `pre_start` and `on_started` into a shared log, yielding inside
/// each hook so a premature ack would have a real chance to race ahead.
#[derive(Default)]
struct OrderActor {
    log: Log,
}

impl Actor for OrderActor {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        tokio::task::yield_now().await;
        self.log.lock().unwrap().push("pre_start");
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        tokio::task::yield_now().await;
        self.log.lock().unwrap().push("on_started");
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// An actor whose `pre_start` always fails: init never completes.
#[derive(Default)]
struct FailInit;

impl Actor for FailInit {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("nope"))
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// An actor whose `pre_start` succeeds but whose `on_started` always panics,
/// and whose `on_stopped` records whether it ever ran.
#[derive(Default)]
struct PanicInit {
    stopped: Arc<AtomicBool>,
}

impl Actor for PanicInit {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("on_started always panics");
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Polls a synchronous predicate until it holds or the deadline passes
/// (mirrors the `wait_until` helper used across the supervision suites).
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_spawn_waits_for_on_started() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let handle = OrderActor { log: log.clone() }.spawn().await.unwrap();

    // No sleep here: the ack itself is the proof both hooks already ran, in
    // order, before `.await` returned.
    assert_eq!(
        *log.lock().unwrap(),
        vec!["pre_start", "on_started"],
        "spawn().await must not return before pre_start AND on_started have both run"
    );

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_spawn_does_not_wait_and_stays_silent_on_init_failure() {
    // .detached() reproduces the exact v0.7.1 behavior: the ack is skipped
    // entirely, so spawn() itself reports success even though pre_start is
    // guaranteed to fail moments later in the background.
    let handle = FailInit.spawn().detached().await.unwrap();

    // The actor still dies from the failed pre_start - just silently, with
    // nothing telling THIS caller why. wait_stopped still resolves.
    tokio::time::timeout(Duration::from_secs(2), handle.wait_stopped())
        .await
        .expect("the detached actor must still die from the failed pre_start");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_started_panic_surfaces_as_spawn_error_init_and_skips_on_stopped() {
    let stopped = Arc::new(AtomicBool::new(false));
    let result = PanicInit {
        stopped: stopped.clone(),
    }
    .spawn()
    .await;

    match result {
        Err(SpawnError::Init(err)) => {
            assert!(
                matches!(*err, ActorError::Panic(_)),
                "expected the panic to surface as ActorError::Panic, got: {err}"
            );
        }
        Ok(_) => panic!("spawn must fail when on_started panics"),
        Err(other) => panic!("expected SpawnError::Init, got: {other}"),
    }

    // No sleep needed here (unlike the start_timeout test above): the
    // default, no-deadline spawn ack only fires after run_actor's entire
    // teardown tail has already completed, and that tail unconditionally
    // skips on_stopped when init failed - there is nothing left pending to
    // race, so the terminal state of `stopped` is already decided.
    assert!(
        !stopped.load(Ordering::SeqCst),
        "on_stopped must NOT run after an on_started panic (self-cleaning contract)"
    );
}

// ---------------------------------------------------------------------------
// An awaited spawn whose actor was killed before it finished initializing
// ---------------------------------------------------------------------------

static UNIQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "spawn-ack-{base}-{}",
        UNIQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// The bound on every outcome a test awaits that is not derived from a
/// configured window.
const BOUND: Duration = Duration::from_secs(10);

/// The longest a callback waits at a [`HookGate`]: past every bound a test
/// awaits, so a gate the test never opens shows as that test's own failure.
const GATE_CAP: Duration = Duration::from_secs(15);

/// A gate a callback waits at until the test opens it. The callback says it
/// has entered, so a request the test raises after [`HookGate::entered`]
/// lands while the callback is in flight, whatever the scheduling, and the
/// callback ends only once the test opens the gate.
#[derive(Debug, Default)]
struct HookGate {
    entered: AtomicBool,
    open: AtomicBool,
    wake: tokio::sync::Notify,
}

impl HookGate {
    /// Says the callback has entered, then waits until the gate is open, for
    /// at most [`GATE_CAP`]. Returns whether the gate was opened.
    async fn pass(&self) -> bool {
        self.entered.store(true, Ordering::SeqCst);
        timeout(GATE_CAP, async {
            loop {
                let mut woken = std::pin::pin!(self.wake.notified());
                woken.as_mut().enable();
                if self.open.load(Ordering::SeqCst) {
                    return;
                }
                woken.await;
            }
        })
        .await
        .is_ok()
    }

    /// Returns once the callback has entered the gate.
    async fn entered(&self) {
        let entered = || self.entered.load(Ordering::SeqCst);
        assert!(
            wait_until(BOUND.as_millis() as u64, entered).await,
            "the callback reaches its gate"
        );
    }

    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }
}

/// Opens its gate when dropped, also while a failed assertion unwinds, so a
/// callback never waits at the gate past the test that holds it.
struct OpensOnDrop(Arc<HookGate>);

impl Drop for OpensOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// An actor whose `pre_start` waits at a gate the test opens, records that
/// it returned, and whose `on_started` records that it ran.
#[derive(Debug)]
struct SlowPreStart {
    gate: Arc<HookGate>,
    returned: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
}

impl Actor for SlowPreStart {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if !self.gate.pass().await {
            return Err(ActorError::user("the test never opened the gate"));
        }
        self.returned.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A `Kill` accepted while `pre_start` runs is read on the lane once
/// `pre_start` returns, inside the 5 s kill grace: the spawn reports
/// `Killed`, `pre_start` ran to its end (the abort did not cut it), and
/// `on_started` never runs.
#[test]
fn awaited_spawn_killed_during_pre_start_reports_killed() {
    common::run(2, async {
        let name = uname("killed-pre-start");
        let gate = Arc::new(HookGate::default());
        let _opens = OpensOnDrop(gate.clone());
        let returned = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let spawning = SlowPreStart {
            gate: gate.clone(),
            returned: returned.clone(),
            started: started.clone(),
        }
        .spawn()
        .named(name.clone())
        .kill_grace(Duration::from_secs(5));
        let spawn = tokio::spawn(async move { spawning.await });
        // The Kill is accepted while `pre_start` waits at its gate; the gate
        // opens right after, well inside the grace.
        gate.entered().await;
        ActorSystem::default()
            .kill(&name)
            .await
            .expect("the kill is accepted");
        gate.open();
        let result = timeout(BOUND, spawn)
            .await
            .expect("the spawn returns")
            .expect("the spawning task ends");
        assert!(matches!(result, Err(SpawnError::Killed)), "{result:?}");
        assert!(
            returned.load(Ordering::SeqCst),
            "pre_start ran to its end: the lane read, not the abort, ended the initialization"
        );
        assert!(
            !started.load(Ordering::SeqCst),
            "on_started never ran after the kill"
        );
    });
}

// ---------------------------------------------------------------------------
// A restart attempt killed during its initialization
// ---------------------------------------------------------------------------

/// A child whose restarted instances wait in `on_started` while the probe's
/// flag is set.
struct HeldChild {
    held: Arc<AtomicBool>,
}

impl Actor for HeldChild {
    type Message = bool;
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        while self.held.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: bool, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("the child crashes on command");
    }
}

struct KillSup {
    held: Arc<AtomicBool>,
    built: Arc<std::sync::atomic::AtomicUsize>,
    child: String,
    events: Arc<Mutex<Vec<ChildEvent>>>,
}

impl Actor for KillSup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let held = self.held.clone();
        let built = self.built.clone();
        ctx.spawn_child(move || {
            if built.fetch_add(1, Ordering::SeqCst) == 0 {
                held.store(false, Ordering::SeqCst);
            } else {
                held.store(true, Ordering::SeqCst);
            }
            HeldChild { held: held.clone() }
        })
        .named(self.child.clone())
        .restart_type(RestartType::Permanent)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

#[test]
fn restart_attempt_killed_during_init_is_reported_as_kill() {
    common::run(2, async {
        let held = Arc::new(AtomicBool::new(false));
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let events = Arc::new(Mutex::new(Vec::new()));
        let child = uname("held-child");
        let sup = KillSup {
            held: held.clone(),
            built: built.clone(),
            child: child.clone(),
            events: events.clone(),
        }
        .spawn()
        .named(uname("kill-sup"))
        .supervisor()
        .await
        .expect("supervisor spawn");
        let handle = ActorSystem::default()
            .get::<HeldChild>(&child)
            .expect("the child is registered");
        let _ = handle.send(true).await;
        // The attempt's incarnation is registered under the id once the old
        // one has ended and the registry holds a live one again.
        assert!(
            wait_until(5_000, || {
                built.load(Ordering::SeqCst) == 2
                    && handle.status() == ActorStatus::Stopped
                    && ActorSystem::default()
                        .get::<HeldChild>(&child)
                        .is_some_and(|attempt| attempt.status() != ActorStatus::Stopped)
            })
            .await,
            "the restart attempt's incarnation is registered"
        );
        let report = ActorSystem::default()
            .kill_by_id(&ActorId::from(child.clone()))
            .await;
        assert!(report.is_some(), "the attempt's incarnation is registered");
        assert!(
            wait_until(5_000, || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event.reason, StopReason::Kill))
            })
            .await,
            "the attempt's end is reported as a kill: {:?}",
            events.lock().unwrap()
        );
        for event in events.lock().unwrap().iter() {
            assert!(
                !matches!(event.reason, StopReason::Failure(ActorError::User(_))),
                "never a fabricated user error: {event:?}"
            );
        }
        held.store(false, Ordering::SeqCst);
        sup.stop(StopReason::Graceful).await.unwrap();
    });
}

/// A queued message whose drop holds its worker thread while the test keeps
/// its flag set.
struct HeldMessage {
    entered: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
}

impl Drop for HeldMessage {
    fn drop(&mut self) {
        self.entered.store(true, Ordering::SeqCst);
        let held = self.held.clone();
        tokio::task::block_in_place(|| {
            while held.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
    }
}

/// Releases a held flag however the test ends.
struct ReleaseFlag(Arc<AtomicBool>);

impl Drop for ReleaseFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// What an [`AbortTarget`] receives.
enum AbortMsg {
    /// A turn parked for ever: the actor is aborted after its kill grace.
    Park,
    /// A request answered with `()`.
    Ask,
    Held(HeldMessage),
}

/// An actor whose `pre_start` parks until `go` while `hang_init` is set.
struct AbortTarget {
    hang_init: bool,
    entered: Arc<AtomicBool>,
    parked: Arc<AtomicBool>,
}

impl Actor for AbortTarget {
    type Message = AbortMsg;
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.store(true, Ordering::SeqCst);
        if self.hang_init {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn handle(&mut self, msg: AbortMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            AbortMsg::Park => {
                self.parked.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok(())
            }
            AbortMsg::Ask => Ok(()),
            AbortMsg::Held(message) => {
                drop(message);
                Ok(())
            }
        }
    }
}

/// A `Kill` reaches an initialization that never yields, with a message
/// queued whose drop holds: the task is aborted after its kill grace, and
/// the awaited spawn (with a start timeout, and without) returns `Killed`
/// while that drop still holds, since the reason is written before the
/// queued messages drop. Multi-thread only: the held drop blocks its thread.
#[test]
fn an_awaited_spawn_aborted_in_its_initialization_returns_before_the_queued_drops() {
    common::run_multi_thread_only(4, async {
        for start_timeout in [None, Some(Duration::from_secs(5))] {
            let name = uname("abort-init");
            let entered = Arc::new(AtomicBool::new(false));
            let held = Arc::new(AtomicBool::new(true));
            let _release = ReleaseFlag(held.clone());
            let drop_entered = Arc::new(AtomicBool::new(false));
            let actor = AbortTarget {
                hang_init: true,
                entered: entered.clone(),
                parked: Arc::new(AtomicBool::new(false)),
            };
            let spawn_name = name.clone();
            let spawner = tokio::spawn(async move {
                let mut builder = actor
                    .spawn()
                    .named(spawn_name)
                    .kill_grace(Duration::from_millis(50));
                if let Some(bound) = start_timeout {
                    builder = builder.start_timeout(bound);
                }
                builder.await
            });
            assert!(
                wait_until(BOUND.as_millis() as u64, || entered.load(Ordering::SeqCst)).await,
                "pre_start begins"
            );
            let handle = ActorSystem::default()
                .get::<AbortTarget>(&name)
                .expect("registered");
            handle
                .notify(AbortMsg::Held(HeldMessage {
                    entered: drop_entered.clone(),
                    held: held.clone(),
                }))
                .await
                .expect("queued");
            handle.stop(StopReason::Kill).await.expect("accepted");
            let result = timeout(BOUND, spawner)
                .await
                .expect("the spawn returns while the queued drop holds")
                .expect("the spawner ends");
            assert!(
                matches!(result, Err(SpawnError::Killed)),
                "{:?}",
                result.err()
            );
            assert!(held.load(Ordering::SeqCst), "the queued drop still held");
            held.store(false, Ordering::SeqCst);
            assert_eq!(
                timeout(BOUND, handle.wait_stopped()).await.expect("ends"),
                StopReason::Kill
            );
            assert!(drop_entered.load(Ordering::SeqCst));
        }
    });
}
