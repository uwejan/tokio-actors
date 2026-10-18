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
    ActorResult, ActorStatus, ActorSystem, ChildEvent, StopReason, SupervisionConfig, SystemConfig,
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

/// A trivial actor: `handle` returns immediately.
#[derive(Default)]
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

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
        let deadline = std::time::Instant::now() + BOUND;
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
async fn turn_elapsed_is_none_between_turns() {
    let sys = ActorSystem::create_with(
        uname("between-turns"),
        SystemConfig::default().with_turn_warning_threshold(LONG_THRESHOLD),
    )
    .unwrap();
    let handle = Idle.spawn().on_system(&sys).await.unwrap();

    assert_eq!(sys.turn_elapsed(handle.id()), None);
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_elapsed_clears_when_the_turn_ends() {
    let sys = ActorSystem::create_with(
        uname("turn-ends"),
        SystemConfig::default().with_turn_warning_threshold(LONG_THRESHOLD),
    )
    .unwrap();
    let (handle, turn, _open) = slow_on(&sys).await;

    held_turn(&handle, &turn).await;
    assert!(
        sys.turn_elapsed(handle.id()).is_some(),
        "the turn should still be in flight"
    );

    // Once handle() returns, the turn clock clears.
    turn.open();
    until("the turn clock to clear once handle() returns", || {
        sys.turn_elapsed(handle.id()).is_none()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_elapsed_is_none_while_the_watchdog_is_off() {
    let sys = ActorSystem::create(uname("watchdog-off")).unwrap();
    assert_eq!(sys.turn_warning_threshold(), None);

    let (handle, turn, _open) = slow_on(&sys).await;

    // A held turn is genuinely in flight, well past its start - but turns
    // are only ever timed while a threshold is set.
    held_turn(&handle, &turn).await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        sys.turn_elapsed(handle.id()),
        None,
        "turns must never be timed while the watchdog is off, even mid-turn"
    );
    assert_eq!(sys.actor_status(handle.id()), Some(ActorStatus::Running));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_elapsed_is_none_once_the_watchdog_is_switched_off_mid_turn() {
    let sys = ActorSystem::create_with(
        uname("off-mid-turn"),
        SystemConfig::default().with_turn_warning_threshold(LONG_THRESHOLD),
    )
    .unwrap();
    let (handle, turn, _open) = slow_on(&sys).await;

    held_turn(&handle, &turn).await;
    assert!(
        sys.turn_elapsed(handle.id()).is_some(),
        "the turn should be reported while the threshold is still on"
    );

    // Switch the watchdog off mid-turn: the actor's own turn clock keeps a
    // nonzero token for this turn until `handle()` returns, but the
    // system's current threshold - not that stale token - decides whether
    // `turn_elapsed` reports it.
    sys.set_turn_warning_threshold(None);

    assert_eq!(
        sys.turn_elapsed(handle.id()),
        None,
        "turn_elapsed must report None as soon as the threshold is off, even mid-turn"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_actors_lists_turns_over_the_threshold() {
    let sys = ActorSystem::create_with(
        uname("stuck-list"),
        SystemConfig::default().with_turn_warning_threshold(Duration::from_millis(100)),
    )
    .unwrap();

    let (slow, turn, _open) = slow_on(&sys).await;
    let idle = Idle.spawn().on_system(&sys).await.unwrap();

    held_turn(&slow, &turn).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let stuck = sys.stuck_actors();
    assert_eq!(
        stuck.len(),
        1,
        "only the slow actor's turn exceeds the threshold"
    );
    assert_eq!(&stuck[0].0, slow.id());
    assert!(stuck[0].1 >= Duration::from_millis(100));
    assert!(
        !stuck.iter().any(|(id, _)| id == idle.id()),
        "an idle actor with no turn in flight must never be reported stuck"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_actors_is_empty_without_a_threshold() {
    let sys = ActorSystem::create(uname("no-threshold")).unwrap();
    let (handle, turn, _open) = slow_on(&sys).await;

    held_turn(&handle, &turn).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(sys.stuck_actors().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_elapsed_is_none_for_an_unknown_id() {
    let sys = ActorSystem::create(uname("unknown-id")).unwrap();
    let unknown = tokio_actors::ActorId::from(format!("unknown-{}", uuid::Uuid::new_v4()));

    assert_eq!(sys.turn_elapsed(&unknown), None);
}

/// Restores the default system's turn warning threshold to `None` on drop,
/// so `threshold_set_on_the_default_system_at_runtime_takes_effect` never
/// leaves it set for later tests - including on this test's own assertion
/// failure, since a drop guard still runs while the panic unwinds.
struct RestoreDefaultThreshold;

impl Drop for RestoreDefaultThreshold {
    fn drop(&mut self) {
        ActorSystem::default().set_turn_warning_threshold(None);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn threshold_set_on_the_default_system_at_runtime_takes_effect() {
    let _restore = RestoreDefaultThreshold;
    let sys = ActorSystem::default();
    sys.set_turn_warning_threshold(Some(Duration::from_millis(100)));

    let (handle, turn, _open) = slow_on(&sys).await;
    held_turn(&handle, &turn).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let elapsed = sys
        .turn_elapsed(handle.id())
        .expect("a held turn is still in flight");
    assert!(elapsed >= Duration::from_millis(100));

    // Membership-only, per the crate's default-system test discipline:
    // `cargo test` runs many tests concurrently against this same singleton.
    assert!(sys.stuck_actors().iter().any(|(id, _)| id == handle.id()));
}

// ---------------------------------------------------------------------------
// The watchdog times every callback
// ---------------------------------------------------------------------------

/// How long a test waits for a condition before it fails.
const BOUND: Duration = Duration::from_secs(10);

async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + BOUND;
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

/// An actor whose `on_started` waits for its gate.
struct SlowStart {
    gate: Gate,
}

impl Actor for SlowStart {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_actors_lists_an_actor_stuck_in_its_initialization() {
    let sys = watched("stuck-init");
    let gate = Gate::default();
    let name = uname("slow-start");
    let spawned = {
        let sys = sys.clone();
        let name = name.clone();
        let gate = gate.clone();
        tokio::spawn(async move { SlowStart { gate }.spawn().named(name).on_system(&sys).await })
    };

    until("the actor to be listed as stuck", || {
        !sys.stuck_actors().is_empty()
    })
    .await;
    assert!(
        !spawned.is_finished(),
        "the spawn has not acknowledged yet while the actor is listed"
    );
    let (id, elapsed) = sys.stuck_actors().remove(0);
    assert!(elapsed >= Duration::from_millis(50), "{elapsed:?}");
    assert_eq!(sys.actor_status(&id), Some(ActorStatus::Initializing));
    gate.open();
    spawned.await.unwrap().expect("the actor starts");
}

/// A supervisor whose handler waits for `busy` and whose `on_child_stopped`
/// waits for `delivery`.
struct StuckDelivery {
    /// Set once a `Busy` message is being handled.
    in_turn: Arc<AtomicBool>,
    busy: Gate,
    delivery: Gate,
}

#[derive(Clone)]
enum DeliveryMsg {
    Busy,
}

impl Actor for StuckDelivery {
    type Message = DeliveryMsg;
    type Response = ();

    async fn handle(
        &mut self,
        _msg: DeliveryMsg,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.in_turn.store(true, Ordering::SeqCst);
        self.busy.wait().await;
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(Idle::default)
            .named("delivery-child".to_string())
            .await?;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        _event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.delivery.wait().await;
        Ok(())
    }
}

async fn delivery_supervisor(
    sys: &Arc<ActorSystem>,
) -> (
    tokio_actors::ActorHandle<StuckDelivery>,
    Arc<AtomicBool>,
    Gate,
    Gate,
) {
    let in_turn = Arc::new(AtomicBool::new(false));
    let (busy, delivery) = (Gate::default(), Gate::default());
    let sup = StuckDelivery {
        in_turn: in_turn.clone(),
        busy: busy.clone(),
        delivery: delivery.clone(),
    }
    .spawn()
    .named(uname("sup"))
    .with_supervision(SupervisionConfig::one_for_one())
    .on_system(sys)
    .await
    .unwrap();
    (sup, in_turn, busy, delivery)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_actors_lists_a_supervisor_stuck_in_a_final_delivery() {
    let sys = watched("stuck-final");
    let (sup, in_turn, busy_gate, delivery) = delivery_supervisor(&sys).await;
    // The handler is busy, a child ends meanwhile, and the stop lands in the
    // same handler: the death is pending when the message loop ends.
    let busy = {
        let sup = sup.clone();
        tokio::spawn(async move { sup.notify(DeliveryMsg::Busy).await })
    };
    until("the handler to be in its turn", || {
        in_turn.load(Ordering::SeqCst)
    })
    .await;
    let child = sys
        .get::<Idle>("delivery-child")
        .expect("the supervisor's child is registered");
    child
        .stop(StopReason::Failure(tokio_actors::ActorError::user("crash")))
        .await
        .unwrap();
    child.wait_stopped().await;
    sup.stop(StopReason::Graceful).await.unwrap();
    busy_gate.open();
    busy.await.unwrap().unwrap();

    // The handler ended; the final delivery now waits for its gate.
    until(
        "the supervisor to be listed with its status Stopping",
        || {
            sys.stuck_actors().iter().any(|(id, _)| id == sup.id())
                && sys.actor_status(sup.id()) == Some(ActorStatus::Stopping)
        },
    )
    .await;
    delivery.open();
    tokio::time::timeout(BOUND, sup.wait_stopped())
        .await
        .expect("the supervisor stops");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_actors_lists_a_supervisor_stuck_in_a_loop_delivery() {
    let sys = watched("stuck-loop");
    let (sup, _in_turn, _busy, delivery) = delivery_supervisor(&sys).await;
    let child = sys
        .get::<Idle>("delivery-child")
        .expect("the supervisor's child is registered");
    child
        .stop(StopReason::Failure(tokio_actors::ActorError::user("crash")))
        .await
        .unwrap();
    // The supervisor is idle, so the death is delivered by its message loop,
    // while its status is still Running.
    until("the supervisor to be listed as stuck", || {
        sys.stuck_actors().iter().any(|(id, _)| id == sup.id())
    })
    .await;
    assert_eq!(sys.actor_status(sup.id()), Some(ActorStatus::Running));
    let elapsed = sys.turn_elapsed(sup.id()).expect("a callback is in flight");
    assert!(elapsed >= Duration::from_millis(50), "{elapsed:?}");
    delivery.open();
    sup.stop(StopReason::Graceful).await.unwrap();
    tokio::time::timeout(BOUND, sup.wait_stopped())
        .await
        .expect("the supervisor stops");
}

/// An actor whose `pre_start` waits for its gate.
struct SlowPreStart {
    gate: Gate,
}

impl Actor for SlowPreStart {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.gate.wait().await;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_actors_lists_an_actor_stuck_in_pre_start() {
    let sys = watched("stuck-pre-start");
    let gate = Gate::default();
    let spawned = {
        let sys = sys.clone();
        let gate = gate.clone();
        tokio::spawn(async move { SlowPreStart { gate }.spawn().on_system(&sys).await })
    };
    until("the actor to be listed as stuck", || {
        !sys.stuck_actors().is_empty()
    })
    .await;
    assert!(
        !spawned.is_finished(),
        "the spawn has not acknowledged yet while the actor is listed"
    );
    let (id, elapsed) = sys.stuck_actors().remove(0);
    assert!(elapsed >= Duration::from_millis(50), "{elapsed:?}");
    assert_eq!(sys.actor_status(&id), Some(ActorStatus::Initializing));
    gate.open();
    spawned.await.unwrap().expect("the actor starts");
}

/// An actor whose `pre_stop` waits for its gate and then lets the stop
/// proceed.
struct SlowPreStop {
    gate: Gate,
}

impl Actor for SlowPreStop {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.gate.wait().await;
        true
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_actors_lists_an_actor_stuck_in_pre_stop() {
    let sys = watched("stuck-pre-stop");
    let gate = Gate::default();
    let handle = SlowPreStop { gate: gate.clone() }
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    until("the actor to be listed as stuck", || {
        sys.stuck_actors().iter().any(|(id, _)| id == handle.id())
    })
    .await;
    // The actor stays Running while `pre_stop` decides.
    assert_eq!(sys.actor_status(handle.id()), Some(ActorStatus::Running));
    let elapsed = sys
        .turn_elapsed(handle.id())
        .expect("a callback is in flight");
    assert!(elapsed >= Duration::from_millis(50), "{elapsed:?}");

    gate.open();
    tokio::time::timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the actor stops");
    assert_eq!(sys.turn_elapsed(handle.id()), None, "gone once it returned");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_callback_is_timed_while_the_watchdog_is_off() {
    let sys = ActorSystem::create(uname("callbacks-off")).unwrap();
    let gate = Gate::default();
    let handle = SlowStop { gate: gate.clone() }
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    // Held in `on_stopped` for as long as the checks take.
    until("the actor to be stopping", || {
        sys.actor_status(handle.id()) == Some(ActorStatus::Stopping)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(sys.actor_status(handle.id()), Some(ActorStatus::Stopping));
    assert_eq!(
        sys.turn_elapsed(handle.id()),
        None,
        "no callback is timed while no threshold is set"
    );
    assert!(sys.stuck_actors().is_empty());
    gate.open();
    tokio::time::timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the actor stops");
}
