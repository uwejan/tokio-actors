#![allow(dead_code)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, ActorSystem, Shutdown, StopReason,
};

// ---------------------------------------------------------------------------
// Shared actor that tracks which lifecycle callbacks fire
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StubActor {
    pre_stop_called: Arc<AtomicBool>,
    on_stopped_called: Arc<AtomicBool>,
}

impl Actor for StubActor {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.pre_stop_called.store(true, Ordering::SeqCst);
        true
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.on_stopped_called.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Kill bypasses both pre_stop and on_stopped (Tier 3 termination).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_bypasses_all_callbacks() {
    let pre_stop = Arc::new(AtomicBool::new(false));
    let on_stopped = Arc::new(AtomicBool::new(false));

    let actor = StubActor {
        pre_stop_called: Arc::clone(&pre_stop),
        on_stopped_called: Arc::clone(&on_stopped),
    };

    let handle = actor
        .spawn()
        .named("kill-test")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    handle.stop(StopReason::Kill).await.unwrap();
    timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the killed actor stops");

    assert!(
        !pre_stop.load(Ordering::SeqCst),
        "pre_stop must NOT be called on Kill"
    );
    assert!(
        !on_stopped.load(Ordering::SeqCst),
        "on_stopped must NOT be called on Kill"
    );
}

// ---------------------------------------------------------------------------
// A stop reason raised while pre_stop is in flight
// ---------------------------------------------------------------------------

/// The bound on every outcome a test awaits.
const BOUND: Duration = Duration::from_secs(10);

/// The longest a callback waits at a [`Gate`]: past every bound a test
/// awaits, so a gate the test never opens shows as that test's own failure.
const GATE_CAP: Duration = Duration::from_secs(15);

/// A gate a callback waits at until the test opens it, so a request the test
/// raises after seeing `entered` lands while the callback is in flight,
/// whatever the scheduling. The wait is bounded so a test that fails before
/// opening it still ends.
#[derive(Clone, Default)]
struct Gate(Arc<(AtomicBool, AtomicBool)>);

/// Opens its gate when dropped, also while a failed assertion unwinds, so a
/// callback never waits at the gate past the test that holds it.
struct OpensOnDrop(Gate);

impl Drop for OpensOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

impl Gate {
    async fn pass(&self) {
        self.0 .0.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + GATE_CAP;
        while !self.0 .1.load(Ordering::SeqCst) && Instant::now() < deadline {
            sleep(Duration::from_millis(1)).await;
        }
    }

    /// Waits until the callback reached its gate. The bound only limits how long a real hang takes to report.
    async fn entered(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self.0 .0.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "the callback never reached its gate"
            );
            sleep(Duration::from_millis(1)).await;
        }
    }

    fn open(&self) {
        self.0 .1.store(true, Ordering::SeqCst);
    }
}

/// Actor whose `pre_stop` gate is configurable (a test-held gate before
/// answering, and whether it accepts or vetoes), so a test can control
/// exactly what lands on the stop lane while the gate is still in flight, and
/// records the reason `on_stopped` (if any) was called with and how many
/// `pre_stop` calls answered.
#[derive(Default)]
struct GatedStopActor {
    pre_stop_gate: Option<Gate>,
    pre_stop_accepts: bool,
    answered: Arc<AtomicUsize>,
    on_stopped_reason: Arc<Mutex<Option<StopReason>>>,
}

impl Actor for GatedStopActor {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        if let Some(gate) = &self.pre_stop_gate {
            gate.pass().await;
        }
        self.answered.fetch_add(1, Ordering::SeqCst);
        self.pre_stop_accepts
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.on_stopped_reason.lock().unwrap() = Some(reason.clone());
        Ok(())
    }
}

/// A `Kill` raised while an accepting `pre_stop` is still in flight must win
/// over the stale `Graceful` the gate was called with: `on_stopped` must
/// never run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_raised_during_an_accepting_pre_stop_bypasses_on_stopped() {
    let on_stopped_reason = Arc::new(Mutex::new(None));
    let gate = Gate::default();
    let actor = GatedStopActor {
        pre_stop_gate: Some(gate.clone()),
        pre_stop_accepts: true,
        on_stopped_reason: Arc::clone(&on_stopped_reason),
        ..Default::default()
    };

    let handle = actor
        .spawn()
        .named("kill-during-accepting-pre-stop")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    // pre_stop is at its gate, so Kill lands while it is in flight; the gate
    // then opens at once, long before the Kill's grace could abort the actor.
    gate.entered().await;
    handle.stop(StopReason::Kill).await.unwrap();
    gate.open();
    timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the killed actor stops");

    let reason = on_stopped_reason.lock().unwrap().clone();
    assert!(
        reason.is_none(),
        "on_stopped must never run once Kill wins over an accepting pre_stop, got {reason:?}"
    );
}

// ---------------------------------------------------------------------------
// A same-severity stop raised while pre_stop is in flight
// ---------------------------------------------------------------------------

/// `pre_stop`'s first call vetoes; every call after that accepts
/// immediately, regardless of the reason it is called with. Also panics on
/// its first call when `panic_first` is set, instead of vetoing.
#[derive(Default)]
struct VetoOnceActor {
    /// When set, the first call waits at this gate before it vetoes or
    /// panics.
    first_gate: Option<Gate>,
    panic_first: bool,
    pre_stop_calls: Arc<AtomicUsize>,
    on_stopped_reason: Arc<Mutex<Option<StopReason>>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl Actor for VetoOnceActor {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, ctx: &mut ActorContext<Self>) -> bool {
        let n = self.pre_stop_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            if let Some(gate) = &self.first_gate {
                gate.pass().await;
            }
            if self.panic_first {
                panic!("pre_stop blew up");
            }
            false
        } else {
            let _ = ctx;
            true
        }
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.on_stopped_reason.lock().unwrap() = Some(reason.clone());
        *self.last_error.lock().unwrap() = ctx.last_error().map(|e| e.to_string());
        Ok(())
    }
}

/// A supervisor with one `Shutdown::Infinity` child under a once-vetoing
/// gate: a user request starts the gate, and the parent's own teardown
/// raises `ParentRequest` on the same lane while that gate is still in
/// flight. The child's retried gate accepts, so the parent's unbounded wait
/// on that child resolves instead of hanging forever.
struct InfinityChildSup {
    gate: Gate,
}

impl Actor for InfinityChildSup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let gate = self.gate.clone();
        ctx.spawn_child(move || VetoOnceActor {
            first_gate: Some(gate.clone()),
            ..Default::default()
        })
        .named("infinity-child")
        .shutdown(Shutdown::Infinity)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// A pre_stop panic must not be masked by a forced reason raised meanwhile
// ---------------------------------------------------------------------------

/// A `Cancelled` raised while `pre_stop` is panicking (not vetoing) must not
/// mask that panic: the panic already happened, so it is what `on_stopped`
/// sees and what gets recorded, not the reason that happened to land after
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_stop_panic_is_not_masked_by_a_cancelled_raised_meanwhile() {
    let gate = Gate::default();
    let actor = VetoOnceActor {
        first_gate: Some(gate.clone()),
        panic_first: true,
        ..Default::default()
    };
    let on_stopped_reason = Arc::clone(&actor.on_stopped_reason);
    let last_error = Arc::clone(&actor.last_error);

    let handle = actor
        .spawn()
        .named("pre-stop-panic-not-masked")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    gate.entered().await;
    handle.stop(StopReason::Cancelled).await.unwrap();
    gate.open();
    timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the actor stops");

    let reason = on_stopped_reason.lock().unwrap().clone();
    assert!(
        matches!(reason, Some(StopReason::Failure(_))),
        "on_stopped must see the pre_stop panic as a Failure, not the Cancelled \
         raised while it was in flight, got {reason:?}"
    );
    let recorded = last_error.lock().unwrap().clone();
    assert!(
        recorded.is_some(),
        "the pre_stop panic must be recorded via record_failure"
    );
}

// ---------------------------------------------------------------------------
// A Kill accepted at any point up to the terminal status is the final reason
// ---------------------------------------------------------------------------

use std::sync::atomic::AtomicU64;
use std::time::Duration;

use tokio::time::{sleep, timeout, Instant};
use tokio_actors::{ActorError, ActorHandle, ChildEvent, RestartType, SendError};

mod common;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("final-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

type Ends = Arc<Mutex<Vec<(String, StopReason)>>>;

/// A supervisor that starts a `Temporary` child of its own on command and
/// records how every child ended: the only place the final reason of an
/// actor is visible.
struct Top<C: Actor> {
    factory: Arc<dyn Fn() -> C + Send + Sync>,
    kill_grace: Duration,
    supervisor_child: bool,
    ends: Ends,
}

impl<C: Actor> Actor for Top<C> {
    type Message = String;
    type Response = ();

    async fn handle(&mut self, name: String, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let factory = self.factory.clone();
        let mut builder = ctx
            .spawn_child(move || factory())
            .named(name)
            .restart_type(RestartType::Temporary)
            .shutdown(Shutdown::Timeout(Duration::from_secs(5)))
            .kill_grace(self.kill_grace);
        if self.supervisor_child {
            builder = builder.with_config(ActorConfig::default().supervisor());
        }
        builder.await?;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.ends
            .lock()
            .unwrap()
            .push((event.child_id.to_string(), event.reason.clone()));
        Ok(())
    }
}

async fn top<C: Actor>(
    sys: &Arc<ActorSystem>,
    kill_grace: Duration,
    supervisor_child: bool,
    factory: impl Fn() -> C + Send + Sync + 'static,
) -> (ActorHandle<Top<C>>, Ends) {
    let ends: Ends = Arc::new(Mutex::new(Vec::new()));
    let handle = Top {
        factory: Arc::new(factory),
        kill_grace,
        supervisor_child,
        ends: ends.clone(),
    }
    .spawn()
    .named(uname("top"))
    .on_system(sys)
    .supervisor()
    .await
    .unwrap();
    (handle, ends)
}

/// Starts the child named `name` under `top`, returns its handle.
async fn start_child<C: Actor, A: Actor>(
    top: &ActorHandle<Top<C>>,
    sys: &Arc<ActorSystem>,
    name: &str,
) -> ActorHandle<A> {
    top.notify(name.to_string()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(handle) = sys.get::<A>(name) {
            return handle;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child never registered"
        );
        sleep(Duration::from_millis(5)).await;
    }
}

/// The reason with which the named child ended, once reported.
async fn final_reason(ends: &Ends, name: &str) -> StopReason {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some((_, reason)) = ends.lock().unwrap().iter().find(|(id, _)| id == name) {
            return reason.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the end of {name} was never reported"
        );
        sleep(Duration::from_millis(5)).await;
    }
}

/// `on_stopped` records the reason it received, then holds until `release`
/// is set (for at most ten seconds).
struct SlowOnStopped {
    started: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    received: Arc<Mutex<Vec<StopReason>>>,
}

impl Actor for SlowOnStopped {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.received.lock().unwrap().push(reason.clone());
        self.started.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.release.load(Ordering::SeqCst) && Instant::now() < deadline {
            sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    }
}

#[test]
fn kill_raised_during_on_stopped_is_the_final_reason() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("on-stopped")).unwrap();
        let (started, release) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let received: Arc<Mutex<Vec<StopReason>>> = Arc::new(Mutex::new(Vec::new()));
        // The grace outlasts `on_stopped`, so the lane read and not the abort
        // decides the final reason.
        let (top, ends) = top(&sys, Duration::from_secs(5), false, {
            let (started, release, received) = (started.clone(), release.clone(), received.clone());
            move || SlowOnStopped {
                started: started.clone(),
                release: release.clone(),
                received: received.clone(),
            }
        })
        .await;
        let name = uname("slow");
        let child: ActorHandle<SlowOnStopped> = start_child(&top, &sys, &name).await;
        child.stop(StopReason::Graceful).await.unwrap();
        while !started.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(5)).await;
        }
        // `on_stopped` returns only after the Kill was accepted.
        child.stop(StopReason::Kill).await.expect("accepted");
        release.store(true, Ordering::SeqCst);
        timeout(Duration::from_secs(5), child.wait_stopped())
            .await
            .expect("the child stops");
        assert!(matches!(
            received.lock().unwrap().as_slice(),
            [StopReason::Graceful]
        ));
        assert!(matches!(final_reason(&ends, &name).await, StopReason::Kill));
    });
}

/// `on_stopped` yields a few times, so a `Kill` raised from another thread
/// can land while it runs or right after it returns.
struct QuickStop {
    on_stopped_started: Arc<AtomicBool>,
    on_stopped_returned: Arc<AtomicBool>,
}

impl Actor for QuickStop {
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
        self.on_stopped_started.store(true, Ordering::SeqCst);
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        self.on_stopped_returned.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// The flags of one `QuickStop`: `on_stopped` started, `on_stopped` returned.
type FlagPair = (Arc<AtomicBool>, Arc<AtomicBool>);

/// Fails after a slow turn, or after raising a `Kill` on itself; records
/// whether `on_stopped` ran.
#[derive(Clone, Copy)]
enum Failing {
    /// `handle` holds until the test releases it, then returns an error.
    SlowTurn,
    /// `handle` raises `Kill` on its own handle, then returns an error.
    KillSelfThenErr,
    /// The same, for a supervisor with no pending child report.
    SupervisorKillSelfThenErr,
}

struct FailsAfterKill {
    how: Failing,
    on_stopped_ran: Arc<AtomicBool>,
    /// The slow turn sets `in_turn`, then holds until `release` is set (for
    /// at most ten seconds).
    in_turn: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl Actor for FailsAfterKill {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match self.how {
            Failing::SlowTurn => {
                self.in_turn.store(true, Ordering::SeqCst);
                let deadline = Instant::now() + Duration::from_secs(10);
                while !self.release.load(Ordering::SeqCst) && Instant::now() < deadline {
                    sleep(Duration::from_millis(5)).await;
                }
            }
            Failing::KillSelfThenErr | Failing::SupervisorKillSelfThenErr => {
                ctx.self_handle()
                    .stop(StopReason::Kill)
                    .await
                    .expect("a Kill is accepted");
            }
        }
        Err(ActorError::user("the handler fails"))
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.on_stopped_ran.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// A supervisor whose `on_child_stopped` raises `Kill` on itself and fails.
struct KillsOnReport {
    on_stopped_ran: Arc<AtomicBool>,
}

impl Actor for KillsOnReport {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Crasher)
            .named("crasher")
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
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        ctx.self_handle()
            .stop(StopReason::Kill)
            .await
            .expect("a Kill is accepted");
        Err(ActorError::user("the report handler fails"))
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.on_stopped_ran.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Fails its initialization, which its supervisor reports as an end.
struct Crasher;

impl Actor for Crasher {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("the child fails to start"))
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// A Kill raised in the last instants before and after the stop lane is sealed
// ---------------------------------------------------------------------------

/// The answer a [`Detonator`] got.
type Answer = Arc<Mutex<Option<Result<(), SendError>>>>;

/// Raises `Kill` on the actor it belongs to when it is dropped, and records
/// the answer.
struct Detonator(Option<ActorHandle<Bombed>>, Answer);

impl Drop for Detonator {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            // `stop` never awaits for a lane request: one poll answers.
            let mut raise = std::pin::pin!(handle.stop(StopReason::Kill));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            if let std::task::Poll::Ready(answer) =
                std::future::Future::poll(raise.as_mut(), &mut cx)
            {
                *self.1.lock().unwrap() = Some(answer);
            }
        }
    }
}

/// A stream that never yields and owns a [`Detonator`]: the actor's end
/// drops it while its forwarders are ended, before the lane is sealed.
struct DetonatingStream(#[allow(dead_code)] Detonator);

impl futures_core::Stream for DetonatingStream {
    type Item = ();

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<()>> {
        std::task::Poll::Pending
    }
}

enum BombedMsg {
    /// A turn that lasts until the test opens its gate.
    Work(Gate),
    /// Queued behind the turn and never handled: dropping it is what counts.
    Bomb(#[allow(dead_code)] Detonator),
    /// Attaches a [`DetonatingStream`] recording into this answer.
    Arm(Answer),
    Stream,
}

impl From<tokio_actors::StreamEvent<()>> for BombedMsg {
    fn from(_: tokio_actors::StreamEvent<()>) -> Self {
        BombedMsg::Stream
    }
}

struct Bombed;

impl Actor for Bombed {
    type Message = BombedMsg;
    type Response = ();

    async fn handle(&mut self, msg: BombedMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            BombedMsg::Work(gate) => gate.pass().await,
            BombedMsg::Arm(answer) => {
                let handle = ctx.self_handle().clone();
                ctx.add_stream(DetonatingStream(Detonator(Some(handle), answer)));
            }
            BombedMsg::Bomb(_) | BombedMsg::Stream => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The inherited stop: its place among the forced reasons
// ---------------------------------------------------------------------------

/// A child whose `pre_stop` accepts once it is released, noting when it
/// began, and which records the reasons `on_stopped` received.
struct AcceptsSlowly {
    in_gate: Arc<AtomicBool>,
    release: Arc<tokio::sync::Notify>,
    received: Arc<Mutex<Vec<StopReason>>>,
}

impl Actor for AcceptsSlowly {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.in_gate.store(true, Ordering::SeqCst);
        self.release.notified().await;
        true
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.received.lock().unwrap().push(reason.clone());
        Ok(())
    }
}

/// A child whose turn holds until released, noting when it began, and which
/// records the reasons `on_stopped` received.
struct HoldsATurn {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    received: Arc<Mutex<Vec<StopReason>>>,
}

impl Actor for HoldsATurn {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.received.lock().unwrap().push(reason.clone());
        Ok(())
    }
}

/// Raises `first` and then `second` while the child is inside one turn, and
/// returns the reason `on_stopped` received and the reason the parent saw.
async fn two_requests_in_one_turn(
    base: &str,
    first: StopReason,
    second: StopReason,
) -> (StopReason, StopReason) {
    let sys = ActorSystem::create(uname(base)).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let received: Arc<Mutex<Vec<StopReason>>> = Arc::new(Mutex::new(Vec::new()));
    let (top, ends) = top(&sys, Duration::from_secs(5), false, {
        let (entered, release, received) = (entered.clone(), release.clone(), received.clone());
        move || HoldsATurn {
            entered: entered.clone(),
            release: release.clone(),
            received: received.clone(),
        }
    })
    .await;
    let name = uname("holding");
    let child: ActorHandle<HoldsATurn> = start_child(&top, &sys, &name).await;
    child.notify(()).await.unwrap();
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("the turn begins");
    // Both requests are accepted: the later, lower-ranked one is not stored.
    child.stop(first).await.unwrap();
    child.stop(second).await.unwrap();
    release.notify_one();
    timeout(Duration::from_secs(5), child.wait_stopped())
        .await
        .expect("the child stops");
    let reasons = received.lock().unwrap().clone();
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    (reasons[0].clone(), final_reason(&ends, &name).await)
}
