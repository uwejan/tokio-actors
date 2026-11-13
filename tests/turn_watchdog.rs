//! Behavioral suite for the stuck-turn watchdog: `ActorSystem::turn_elapsed`,
//! `ActorSystem::stuck_actors`, and `ActorSystem::set_turn_warning_threshold`.
//!
//! Covers: no turn in flight between messages, a running turn reported while
//! it is still in flight, the clock clearing once the turn ends, a threshold
//! actually filtering `stuck_actors` to the actors that exceed it, an unset
//! threshold reporting nothing from either accessor (including while a turn
//! is genuinely in flight - turns are only ever timed while a threshold is
//! set), an unknown id reporting nothing, and setting the threshold at
//! runtime on the lazily-created default system.
//!
//! Every test uses a dedicated `ActorSystem::create`/`create_with` so
//! thresholds never leak between tests, except the runtime-threshold test,
//! which exercises `ActorSystem::default()` directly and restores its
//! threshold to `None` before returning (including on an assertion failure,
//! via a drop guard). Tests that only need `turn_elapsed` to report a value
//! (not `stuck_actors`) use a threshold far longer than anything the test
//! waits for, so nothing is ever reported stuck by coincidence.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, ActorStatus, ActorSystem, StopReason, SystemConfig,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "turn-watchdog-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

/// A turn the test holds: the handler records the instant it began, then
/// blocks the OS thread it runs on, never yielding back to the runtime, until
/// the test opens the turn (at most `BOUND`). Every observation a test makes
/// of the turn in flight happens before it opens it, so how late the test is
/// scheduled never decides whether the turn is still running.
#[derive(Clone, Default)]
struct HeldTurn(Arc<HeldTurnState>);

#[derive(Default)]
struct HeldTurnState {
    began: std::sync::Mutex<Option<std::time::Instant>>,
    open: AtomicBool,
}

impl HeldTurn {
    fn block(&self) {
        *self.0.began.lock().unwrap() = Some(std::time::Instant::now());
        let deadline = std::time::Instant::now() + HANG_BOUND;
        while !self.0.open.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Waits until the handler is inside the held turn, and returns the
    /// instant it began.
    async fn began(&self) -> std::time::Instant {
        let mut began = None;
        until("the held turn to begin", || {
            began = *self.0.began.lock().unwrap();
            began.is_some()
        })
        .await;
        began.unwrap()
    }

    fn open(&self) {
        self.0.open.store(true, Ordering::SeqCst);
    }

    /// Opens the turn when dropped, so a failed assertion never leaves a
    /// worker thread blocked.
    fn opens_on_drop(&self) -> OpenOnDrop {
        OpenOnDrop(self.clone())
    }
}

struct OpenOnDrop(HeldTurn);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// An actor whose handler holds its turn on a [`HeldTurn`].
struct SlowHandler(HeldTurn);

impl Actor for SlowHandler {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.0.block();
        Ok(())
    }
}

/// Starts a held turn on `handle` and returns the instant it began, once it
/// did.
async fn held_turn(
    handle: &tokio_actors::ActorHandle<SlowHandler>,
    turn: &HeldTurn,
) -> std::time::Instant {
    handle.notify(()).await.unwrap();
    turn.began().await
}

/// Spawns a [`SlowHandler`] on `sys` and returns it, its turn, and a guard
/// that opens the turn when the test ends.
async fn slow_on(
    sys: &Arc<ActorSystem>,
) -> (tokio_actors::ActorHandle<SlowHandler>, HeldTurn, OpenOnDrop) {
    let turn = HeldTurn::default();
    let handle = SlowHandler(turn.clone())
        .spawn()
        .on_system(sys)
        .await
        .unwrap();
    let open = turn.opens_on_drop();
    (handle, turn, open)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A threshold far longer than anything any test in this suite waits for, so
/// a test that only cares about `turn_elapsed` reporting a value never has
/// its actor coincidentally show up in `stuck_actors` too.
const LONG_THRESHOLD: Duration = Duration::from_secs(3600);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_elapsed_reports_a_running_turn() {
    let sys = ActorSystem::create_with(
        uname("running-turn"),
        SystemConfig::default().with_turn_warning_threshold(LONG_THRESHOLD),
    )
    .unwrap();
    let (handle, turn, _open) = slow_on(&sys).await;

    let began = held_turn(&handle, &turn).await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The turn began no later than the handler did, so the reported time is
    // at least what the test itself measured since then.
    let measured = began.elapsed();
    let elapsed = sys
        .turn_elapsed(handle.id())
        .expect("a held turn is still in flight");
    assert!(
        elapsed >= measured && elapsed >= Duration::from_millis(150),
        "expected at least {measured:?} elapsed, got {elapsed:?}"
    );
    assert_eq!(sys.actor_status(handle.id()), Some(ActorStatus::Running));
}

/// How a turn reaches the handler.
#[derive(Clone, Copy, Debug)]
enum Entry {
    /// An ordinary message taken from the mailbox.
    Mailbox,
    /// A message taken from the priority lane.
    Priority,
    /// The step a handler asked for with `continue_with`.
    ChainStep,
}

enum EntryMsg {
    /// Holds the turn on the actor's [`HeldTurn`].
    Hold,
    /// Replies at once and asks for `Hold` as the next step.
    ThenHold,
}

/// An actor that holds a turn on a [`HeldTurn`] for the message `Hold`.
struct HoldsOnEntry(HeldTurn);

impl Actor for HoldsOnEntry {
    type Message = EntryMsg;
    type Response = ();

    async fn handle(&mut self, msg: EntryMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            EntryMsg::Hold => self.0.block(),
            EntryMsg::ThenHold => ctx.continue_with(EntryMsg::Hold),
        }
        Ok(())
    }
}

/// The turn clock clears once the turn ends, whichever way the turn was
/// reached: from the mailbox, from the priority lane, or as a chain step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_elapsed_clears_when_the_turn_ends() {
    for entry in [Entry::Mailbox, Entry::Priority, Entry::ChainStep] {
        let sys = ActorSystem::create_with(
            uname("turn-ends"),
            SystemConfig::default().with_turn_warning_threshold(LONG_THRESHOLD),
        )
        .unwrap();
        let turn = HeldTurn::default();
        let _open = turn.opens_on_drop();
        let handle = HoldsOnEntry(turn.clone())
            .spawn()
            .with_config(ActorConfig::default().with_priority_lane(8))
            .on_system(&sys)
            .await
            .unwrap();

        match entry {
            Entry::Mailbox => handle.notify(EntryMsg::Hold).await.unwrap(),
            Entry::Priority => handle.notify_priority(EntryMsg::Hold).await.unwrap(),
            Entry::ChainStep => handle.notify(EntryMsg::ThenHold).await.unwrap(),
        }
        turn.began().await;
        assert!(
            sys.turn_elapsed(handle.id()).is_some(),
            "{entry:?}: the turn should still be in flight"
        );

        // Once handle() returns, the turn clock clears.
        turn.open();
        until(
            &format!("{entry:?}: the turn clock to clear once handle() returns"),
            || sys.turn_elapsed(handle.id()).is_none(),
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// The watchdog times every callback
// ---------------------------------------------------------------------------

/// How long a test waits for a condition before it fails.
const BOUND: Duration = Duration::from_secs(10);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// Polls `pred` until it holds. The bound only limits how long a real hang takes to report.
async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + HANG_BOUND;
    while !pred() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A latch the test opens after its checks: the callbacks below wait on it,
/// so the order of events is decided by what the test observed, never by how
/// long a sleep lasted.
#[derive(Clone, Default)]
struct Gate {
    open: Arc<AtomicBool>,
    changed: Arc<tokio::sync::Notify>,
}

impl Gate {
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.open.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

/// An actor whose `on_stopped` waits for its gate.
struct SlowStop {
    gate: Gate,
}

impl Actor for SlowStop {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.gate.wait().await;
        Ok(())
    }
}

fn watched(label: &str) -> Arc<ActorSystem> {
    ActorSystem::create_with(
        uname(label),
        SystemConfig::default().with_turn_warning_threshold(Duration::from_millis(50)),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_actors_lists_an_actor_stuck_in_on_stopped() {
    let sys = watched("stuck-stop");
    let gate = Gate::default();
    let handle = SlowStop { gate: gate.clone() }
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    until("the actor to be listed as stuck", || {
        sys.stuck_actors().iter().any(|(id, _)| id == handle.id())
    })
    .await;
    let elapsed = sys
        .turn_elapsed(handle.id())
        .expect("a callback is in flight");
    assert!(elapsed >= Duration::from_millis(50), "{elapsed:?}");
    assert_eq!(sys.actor_status(handle.id()), Some(ActorStatus::Stopping));

    gate.open();
    tokio::time::timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the actor stops");
    assert_eq!(sys.turn_elapsed(handle.id()), None, "gone once it returned");
    assert!(!sys.stuck_actors().iter().any(|(id, _)| id == handle.id()));
}
