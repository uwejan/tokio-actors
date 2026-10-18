use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, ActorSystem, Shutdown, StopOutcome, StopReason,
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

/// Kill still bypasses pre_stop/on_stopped even with a message left queued
/// in the mailbox: the stop lane is a separate, higher-priority signal, not
/// just another entry that would need the queue drained first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_bypasses_callbacks_even_with_pending_mailbox_traffic() {
    let pre_stop = Arc::new(AtomicBool::new(false));
    let on_stopped = Arc::new(AtomicBool::new(false));

    let actor = StubActor {
        pre_stop_called: Arc::clone(&pre_stop),
        on_stopped_called: Arc::clone(&on_stopped),
    };

    let config = ActorConfig::default().with_mailbox_capacity(1);
    let handle = actor
        .spawn()
        .named("kill-with-pending-mailbox")
        .with_config(config)
        .await
        .unwrap();

    // Queue a message the actor has had no chance to process yet.
    let _ = handle.try_notify(());

    handle.stop(StopReason::Kill).await.unwrap();
    timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the killed actor stops");

    assert!(
        !pre_stop.load(Ordering::SeqCst),
        "pre_stop must NOT be called on Kill, even with a message still queued"
    );
    assert!(
        !on_stopped.load(Ordering::SeqCst),
        "on_stopped must NOT be called on Kill, even with a message still queued"
    );
}

/// Graceful stop DOES call pre_stop and on_stopped (sanity check).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_stop_calls_callbacks() {
    let pre_stop = Arc::new(AtomicBool::new(false));
    let on_stopped = Arc::new(AtomicBool::new(false));

    let actor = StubActor {
        pre_stop_called: Arc::clone(&pre_stop),
        on_stopped_called: Arc::clone(&on_stopped),
    };

    let handle = actor
        .spawn()
        .named("graceful-test")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the stopped actor stops");

    assert!(
        pre_stop.load(Ordering::SeqCst),
        "pre_stop must be called on Graceful"
    );
    assert!(
        on_stopped.load(Ordering::SeqCst),
        "on_stopped must be called on Graceful"
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

    async fn entered(&self) {
        let deadline = Instant::now() + BOUND;
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

/// A `Cancelled` raised while an accepting `pre_stop` is still in flight also
/// wins over the stale `Graceful` the gate was called with, but unlike
/// `Kill`, `on_stopped` still runs - with the escalated reason, not the
/// stale one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_raised_during_pre_stop_wins_over_graceful() {
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
        .named("cancelled-during-pre-stop")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    gate.entered().await;
    handle.stop(StopReason::Cancelled).await.unwrap();
    gate.open();
    timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the cancelled actor stops");

    let reason = on_stopped_reason.lock().unwrap().clone();
    assert!(
        matches!(reason, Some(StopReason::Cancelled)),
        "on_stopped must run with the escalated Cancelled reason, got {reason:?}"
    );
}

/// Control: a vetoing `pre_stop` already sends the run loop back through
/// `stop_rx.changed()`, so a `Kill` raised after the veto returns is picked
/// up correctly independent of the re-read fix above - `on_stopped` must
/// never run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_after_a_vetoing_pre_stop_bypasses_on_stopped() {
    let on_stopped_reason = Arc::new(Mutex::new(None));
    let actor = GatedStopActor {
        pre_stop_accepts: false,
        on_stopped_reason: Arc::clone(&on_stopped_reason),
        ..Default::default()
    };
    let answered = Arc::clone(&actor.answered);

    let handle = actor
        .spawn()
        .named("kill-after-vetoing-pre-stop")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    // The veto has answered before Kill lands.
    let deadline = Instant::now() + BOUND;
    while answered.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "pre_stop never answered");
        sleep(Duration::from_millis(1)).await;
    }
    handle.stop(StopReason::Kill).await.unwrap();
    timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the killed actor stops");

    let reason = on_stopped_reason.lock().unwrap().clone();
    assert!(
        reason.is_none(),
        "on_stopped must never run once Kill is raised after a veto, got {reason:?}"
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

/// A same-severity retry (`Graceful` after `Graceful`) that lands while the
/// first request's `pre_stop` gate is still in flight must not be lost to
/// that gate's veto: it must re-fire `pre_stop` on the next iteration, exactly
/// as `ActorHandle::stop`'s own contract promises.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_retried_during_a_vetoing_pre_stop_re_fires_the_gate() {
    let gate = Gate::default();
    let actor = VetoOnceActor {
        first_gate: Some(gate.clone()),
        ..Default::default()
    };
    let pre_stop_calls = Arc::clone(&actor.pre_stop_calls);

    let handle = actor
        .spawn()
        .named("graceful-retry-refires-gate")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    // The first pre_stop is at its gate, so the retry lands while it is
    // still in flight.
    gate.entered().await;
    handle.stop(StopReason::Graceful).await.unwrap();
    gate.open();

    tokio::time::timeout(std::time::Duration::from_secs(2), handle.wait_stopped())
        .await
        .expect("the retried Graceful raise must re-fire pre_stop and let the actor stop");

    assert_eq!(
        pre_stop_calls.load(Ordering::SeqCst),
        2,
        "pre_stop must run once for the vetoed raise and once more for the retry"
    );
}

/// A `ParentRequest` raised by `ActorSystem::shutdown` while a user's own
/// `Graceful` stop is being vetoed must not be lost either: the retried gate
/// is honored, and the actor's own `on_stopped` still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_request_raised_during_a_vetoed_user_stop_is_honored() {
    let sys = ActorSystem::create(format!("prq-{}", uuid::Uuid::new_v4())).unwrap();

    let gate = Gate::default();
    let _opens = OpensOnDrop(gate.clone());
    let actor = VetoOnceActor {
        first_gate: Some(gate.clone()),
        ..Default::default()
    };
    let on_stopped_reason = Arc::clone(&actor.on_stopped_reason);

    let handle = actor
        .spawn()
        .named("prq-root")
        .on_system(&sys)
        .await
        .unwrap();

    handle.stop(StopReason::Graceful).await.unwrap();
    // The user's `pre_stop` is at its gate before the shutdown raises
    // `ParentRequest`. The pause makes the raise likely to land while the
    // gate is still held; the assertions hold whichever lands first (a raise
    // after the veto re-fires the gate on an idle actor), and nothing public
    // reads the stop lane to order this by observation.
    gate.entered().await;
    let shutdown = {
        let sys = sys.clone();
        tokio::spawn(async move { sys.shutdown().await })
    };
    sleep(Duration::from_millis(50)).await;
    gate.open();
    let report = timeout(BOUND, shutdown)
        .await
        .expect("the shutdown returns")
        .unwrap();

    assert_eq!(report.outcomes.len(), 1, "got {:?}", report.outcomes);
    assert!(
        matches!(report.outcomes[0].1.outcome, StopOutcome::Graceful),
        "the retried gate must resolve without ever needing Kill, got {:?}",
        report.outcomes[0].1
    );
    let reason = on_stopped_reason.lock().unwrap().clone();
    assert!(
        reason.is_some(),
        "on_stopped must run once the retried gate is honored, got {reason:?}"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_of_an_infinity_child_stops_when_the_child_vetoes_once_during_teardown() {
    let sys = ActorSystem::create(format!("infinity-veto-{}", uuid::Uuid::new_v4())).unwrap();

    let gate = Gate::default();
    let _opens = OpensOnDrop(gate.clone());
    let sup = InfinityChildSup { gate: gate.clone() }
        .spawn()
        .named("infinity-sup")
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();

    let child = loop {
        if let Some(h) = sys.get::<VetoOnceActor>("infinity-child") {
            break h;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };

    // The user's own request starts the once-vetoing gate, held open until
    // the parent's stop is accepted.
    child.stop(StopReason::Graceful).await.unwrap();
    gate.entered().await;
    // The parent's own teardown raises ParentRequest on the child's lane.
    // The pause makes it likely to land while the gate is still held; the
    // parent stops whichever lands first (a raise after the veto re-fires the
    // gate on an idle child), and nothing public reads the stop lane to order
    // this by observation.
    sup.stop(StopReason::Graceful).await.unwrap();
    sleep(Duration::from_millis(50)).await;
    gate.open();

    tokio::time::timeout(std::time::Duration::from_secs(5), sup.wait_stopped())
        .await
        .expect("the parent must not hang behind an Infinity child's re-fired gate");
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

#[test]
fn every_accepted_kill_is_the_final_reason_up_to_the_terminal_status() {
    // A real race between a hammering caller and the stopping actor.
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("race")).unwrap();
        let on_started_flags: Arc<Mutex<Vec<FlagPair>>> = Arc::new(Mutex::new(Vec::new()));
        let (top, ends) = top(&sys, Duration::from_secs(5), false, {
            let flags = on_started_flags.clone();
            move || {
                let pair = (
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(AtomicBool::new(false)),
                );
                flags.lock().unwrap().push(pair.clone());
                QuickStop {
                    on_stopped_started: pair.0,
                    on_stopped_returned: pair.1,
                }
            }
        })
        .await;

        let mut accepted_after_on_stopped = 0usize;
        let mut accepted_kills = 0usize;
        for batch in 0..10 {
            let mut set = tokio::task::JoinSet::new();
            for i in 0..20 {
                let name = uname(&format!("racer-{batch}-{i}"));
                let child: ActorHandle<QuickStop> = start_child(&top, &sys, &name).await;
                let (started, returned) = on_started_flags.lock().unwrap().last().cloned().unwrap();
                set.spawn(async move {
                    let hammer = {
                        let child = child.clone();
                        tokio::spawn(async move {
                            // Waits for `on_stopped`, then hammers `Kill` until the
                            // lane answers `Closed`, remembering the answers.
                            while !started.load(Ordering::SeqCst) {
                                tokio::task::yield_now().await;
                            }
                            let (mut accepted, mut after) = (0usize, 0usize);
                            loop {
                                let returned_before = returned.load(Ordering::SeqCst);
                                match child.stop(StopReason::Kill).await {
                                    Ok(()) => {
                                        accepted += 1;
                                        if returned_before {
                                            after += 1;
                                        }
                                        tokio::task::yield_now().await;
                                    }
                                    Err(SendError::Closed) => break,
                                    Err(other) => panic!("unexpected answer {other:?}"),
                                }
                            }
                            // Once a call returned `Closed`, every later call does.
                            for _ in 0..5 {
                                assert!(matches!(
                                    child.stop(StopReason::Kill).await,
                                    Err(SendError::Closed)
                                ));
                            }
                            (accepted, after)
                        })
                    };
                    child.stop(StopReason::Graceful).await.ok();
                    let (accepted, after) = timeout(Duration::from_secs(10), hammer)
                        .await
                        .expect("the hammer ends")
                        .unwrap();
                    (name, accepted, after)
                });
            }
            while let Some(done) = set.join_next().await {
                let (name, accepted, after) = done.unwrap();
                let reason = final_reason(&ends, &name).await;
                if accepted > 0 {
                    accepted_kills += 1;
                    assert!(
                        matches!(reason, StopReason::Kill),
                        "an accepted Kill is the final reason, got {reason:?} for {name}"
                    );
                }
                if after > 0 {
                    accepted_after_on_stopped += 1;
                }
            }
        }
        eprintln!(
            "iterations with an accepted Kill: {accepted_kills} of 200; \
             with one accepted after on_stopped returned: {accepted_after_on_stopped}"
        );
    });
}

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

#[test]
fn kill_raised_during_a_turn_whose_handler_then_fails_runs_no_on_stopped() {
    common::run(0, async {
        let bound = Duration::from_secs(2);
        // The grace is long, so the abort cannot cut any of these turns.
        let grace = Duration::from_secs(5);

        // Case 1: a slow turn that fails, a Kill raised from outside.
        // Case 2: the handler raises the Kill itself, then fails.
        // Case 3: the same for a supervisor with no pending report.
        for (case, how) in [
            (1, Failing::SlowTurn),
            (2, Failing::KillSelfThenErr),
            (3, Failing::SupervisorKillSelfThenErr),
        ] {
            let sys = ActorSystem::create(uname("fails-after-kill")).unwrap();
            let ran = Arc::new(AtomicBool::new(false));
            let (in_turn, release) = (
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
            );
            let supervisor = matches!(how, Failing::SupervisorKillSelfThenErr);
            let (top, ends) = top(&sys, grace, supervisor, {
                let (ran, in_turn, release) = (ran.clone(), in_turn.clone(), release.clone());
                move || FailsAfterKill {
                    how,
                    on_stopped_ran: ran.clone(),
                    in_turn: in_turn.clone(),
                    release: release.clone(),
                }
            })
            .await;
            let name = uname("fails");
            let child: ActorHandle<FailsAfterKill> = start_child(&top, &sys, &name).await;
            child.notify(()).await.unwrap();
            if matches!(how, Failing::SlowTurn) {
                // The Kill is raised inside the turn, which fails only after.
                timeout(bound, async {
                    while !in_turn.load(Ordering::SeqCst) {
                        sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("case {case}: the turn begins"));
                child.stop(StopReason::Kill).await.expect("accepted");
                release.store(true, Ordering::SeqCst);
            }
            timeout(bound, child.wait_stopped())
                .await
                .unwrap_or_else(|_| panic!("case {case}: the child stops"));
            assert!(
                !ran.load(Ordering::SeqCst),
                "case {case}: on_stopped must not run after a Kill"
            );
            assert!(
                matches!(final_reason(&ends, &name).await, StopReason::Kill),
                "case {case}: the final reason is Kill"
            );
        }

        // Case 4: a supervisor whose report handler raises the Kill and fails.
        let sys = ActorSystem::create(uname("report-fails-after-kill")).unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let (top, ends) = top(&sys, grace, true, {
            let ran = ran.clone();
            move || KillsOnReport {
                on_stopped_ran: ran.clone(),
            }
        })
        .await;
        let name = uname("reports");
        // The supervisor ends on its own as soon as it starts: its end is the
        // only thing to wait for.
        top.notify(name.clone()).await.unwrap();
        assert!(
            matches!(final_reason(&ends, &name).await, StopReason::Kill),
            "case 4: the final reason is Kill"
        );
        assert!(
            !ran.load(Ordering::SeqCst),
            "case 4: on_stopped must not run after a Kill"
        );
    });
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

#[test]
fn kill_raised_while_the_forwarders_end_is_the_final_reason() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("forwarder-end")).unwrap();
        let (top, ends) = top(&sys, Duration::from_secs(5), false, || Bombed).await;
        let name = uname("bombed");
        let child: ActorHandle<Bombed> = start_child(&top, &sys, &name).await;
        let answer = Answer::default();
        // Answered once handled: the stream is attached before the stop.
        child.send(BombedMsg::Arm(answer.clone())).await.unwrap();
        child.stop(StopReason::Graceful).await.unwrap();
        timeout(Duration::from_secs(5), child.wait_stopped())
            .await
            .expect("the child stops");
        assert!(
            matches!(*answer.lock().unwrap(), Some(Ok(()))),
            "the Kill is accepted while the forwarders end: {:?}",
            answer.lock().unwrap()
        );
        assert!(
            matches!(final_reason(&ends, &name).await, StopReason::Kill),
            "a Kill accepted before the seal is read by it"
        );
    });
}

#[test]
fn kill_raised_while_the_queued_messages_drop_is_refused() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("mailbox-drop")).unwrap();
        let (top, ends) = top(&sys, Duration::from_secs(5), false, || Bombed).await;
        let name = uname("bombed");
        let child: ActorHandle<Bombed> = start_child(&top, &sys, &name).await;
        let gate = Gate::default();
        child.notify(BombedMsg::Work(gate.clone())).await.unwrap();
        // The bomb is queued behind a turn in flight, and the stop is raised
        // before that turn ends, so the bomb is never handled.
        gate.entered().await;
        let answer = Answer::default();
        child
            .notify(BombedMsg::Bomb(Detonator(
                Some(child.clone()),
                answer.clone(),
            )))
            .await
            .unwrap();
        child.stop(StopReason::Graceful).await.unwrap();
        gate.open();
        timeout(Duration::from_secs(5), child.wait_stopped())
            .await
            .expect("the child stops");
        assert!(
            matches!(*answer.lock().unwrap(), Some(Err(SendError::Closed))),
            "the lane is sealed before the queued messages drop: {:?}",
            answer.lock().unwrap()
        );
        assert!(
            matches!(final_reason(&ends, &name).await, StopReason::Graceful),
            "a Kill refused after the seal changes nothing"
        );
    });
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

#[test]
fn parent_killed_raised_during_an_accepting_pre_stop_is_the_final_reason() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("parent-killed-gate")).unwrap();
        let in_gate = Arc::new(AtomicBool::new(false));
        let release = Arc::new(tokio::sync::Notify::new());
        let received: Arc<Mutex<Vec<StopReason>>> = Arc::new(Mutex::new(Vec::new()));
        let (top, ends) = top(&sys, Duration::from_secs(5), false, {
            let (in_gate, release, received) = (in_gate.clone(), release.clone(), received.clone());
            move || AcceptsSlowly {
                in_gate: in_gate.clone(),
                release: release.clone(),
                received: received.clone(),
            }
        })
        .await;
        let name = uname("gated");
        let child: ActorHandle<AcceptsSlowly> = start_child(&top, &sys, &name).await;
        child.stop(StopReason::Graceful).await.unwrap();
        while !in_gate.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(2)).await;
        }
        // The gate is in flight, held: the inherited stop lands inside it and
        // is the reason the stop ends with.
        child.stop(StopReason::ParentKilled).await.unwrap();
        release.notify_one();
        timeout(Duration::from_secs(5), child.wait_stopped())
            .await
            .expect("the child stops");
        assert!(
            matches!(
                received.lock().unwrap().as_slice(),
                [StopReason::ParentKilled]
            ),
            "on_stopped received {:?}",
            received.lock().unwrap()
        );
        assert!(
            matches!(final_reason(&ends, &name).await, StopReason::ParentKilled),
            "and so did the parent's `on_child_stopped`"
        );
    });
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

#[test]
fn failure_is_kept_over_a_later_parent_killed() {
    common::run(0, async {
        let (stopped, seen) = two_requests_in_one_turn(
            "failure-kept",
            StopReason::Failure(ActorError::user("fail")),
            StopReason::ParentKilled,
        )
        .await;
        assert!(matches!(stopped, StopReason::Failure(_)), "{stopped:?}");
        assert!(matches!(seen, StopReason::Failure(_)), "{seen:?}");
    });
}

#[test]
fn parent_killed_is_kept_over_a_later_cancelled() {
    common::run(0, async {
        let (stopped, seen) = two_requests_in_one_turn(
            "parent-killed-kept",
            StopReason::ParentKilled,
            StopReason::Cancelled,
        )
        .await;
        assert!(matches!(stopped, StopReason::ParentKilled), "{stopped:?}");
        assert!(matches!(seen, StopReason::ParentKilled), "{seen:?}");
    });
}
