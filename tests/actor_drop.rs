//! An actor's value is dropped at the end of its teardown, inside the stop
//! bound of whoever stops it, and before `Stopped` is published.
//!
//! Covers: `stop_within`, `ActorSystem::kill`, system shutdown and a start
//! timeout across a value whose `Drop` is held by the test or panics; a
//! `Kill` answered `Closed` once the lane is sealed, while the value drops;
//! and a failed init reported as its error, never as a start timeout,
//! however long the value's `Drop` takes, for a root and for a supervised
//! restart.

mod common;

use std::future::IntoFuture;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorStatus, ActorSystem, ChildEvent,
    RestartType, SendError, SpawnError, StopOutcome, StopReason, StreamEvent,
};

/// The bound on every awaited outcome below: far above every window these
/// tests configure, so an outcome that never comes fails here instead of
/// hanging.
const BOUND: Duration = Duration::from_secs(10);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("actor-drop-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

/// Holds a `Drop` until the test releases it, and says when it began.
struct Gate {
    entered: Option<oneshot::Sender<tokio::time::Instant>>,
    release: std::sync::mpsc::Receiver<()>,
}

/// The test's side of a [`Gate`].
struct GateKey {
    entered: oneshot::Receiver<tokio::time::Instant>,
    release: std::sync::mpsc::Sender<()>,
}

fn gate() -> (Gate, GateKey) {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    (
        Gate {
            entered: Some(entered_tx),
            release: release_rx,
        },
        GateKey {
            entered: entered_rx,
            release: release_tx,
        },
    )
}

impl GateKey {
    /// Waits for the `Drop` to begin and returns the instant it began.
    async fn entered(&mut self) -> tokio::time::Instant {
        timeout(BOUND, &mut self.entered)
            .await
            .expect("the value's Drop begins")
            .expect("the gate is alive")
    }

    fn release(&self) {
        let _ = self.release.send(());
    }
}

/// What the actor value's `Drop` does.
enum OnDrop {
    Nothing,
    Panic,
    /// Blocks its worker thread until released (multi-thread runtimes only).
    Hold(Gate),
    /// Blocks its thread for this long.
    Sleep(Duration),
}

/// A forwarder the actor registers as it starts (or as its init fails).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Attach {
    Nothing,
    Timer,
    Stream,
}

struct Dropper {
    on_drop: OnDrop,
    fail_init: bool,
    attach: Attach,
    /// Holds the first turn until released; says when the turn began.
    turn: Option<(oneshot::Receiver<()>, oneshot::Sender<()>)>,
}

impl Dropper {
    fn new(on_drop: OnDrop) -> Self {
        Self {
            on_drop,
            fail_init: false,
            attach: Attach::Nothing,
            turn: None,
        }
    }

    async fn attach(attach: Attach, ctx: &mut ActorContext<Self>) {
        match attach {
            Attach::Nothing => {}
            Attach::Timer => {
                ctx.every_with(Duration::from_secs(3600), || Tick)
                    .await
                    .expect("the timer registers");
            }
            Attach::Stream => {
                ctx.add_stream(tokio_stream::pending::<()>());
            }
        }
    }
}

/// The message of the actors below.
struct Tick;

impl From<StreamEvent<()>> for Tick {
    fn from(_: StreamEvent<()>) -> Self {
        Tick
    }
}

impl Drop for Dropper {
    fn drop(&mut self) {
        match &mut self.on_drop {
            OnDrop::Nothing => {}
            OnDrop::Panic => panic!("the actor value panicked when dropped"),
            OnDrop::Hold(gate) => {
                if let Some(entered) = gate.entered.take() {
                    let _ = entered.send(tokio::time::Instant::now());
                }
                let release = &gate.release;
                tokio::task::block_in_place(|| {
                    let _ = release.recv();
                });
            }
            OnDrop::Sleep(time) => std::thread::sleep(*time),
        }
    }
}

impl Actor for Dropper {
    type Message = Tick;
    type Response = ();

    async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.fail_init {
            Self::attach(self.attach, ctx).await;
            return Err(ActorError::user("init refused"));
        }
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Self::attach(self.attach, ctx).await;
        Ok(())
    }

    async fn handle(&mut self, _msg: Tick, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some((release, began)) = self.turn.take() {
            let _ = began.send(());
            let _ = release.await;
        }
        Ok(())
    }
}

async fn spawn_root(sys: &Arc<ActorSystem>, on_drop: OnDrop) -> (ActorHandle<Dropper>, String) {
    spawn_root_attached(sys, on_drop, Attach::Nothing).await
}

async fn spawn_root_attached(
    sys: &Arc<ActorSystem>,
    on_drop: OnDrop,
    attach: Attach,
) -> (ActorHandle<Dropper>, String) {
    let name = uname("root");
    let mut actor = Dropper::new(on_drop);
    actor.attach = attach;
    let handle = actor
        .spawn()
        .named(name.clone())
        .on_system(sys)
        .kill_grace(Duration::from_millis(50))
        .await
        .expect("the actor starts");
    (handle, name)
}

// ---------------------------------------------------------------------------
// A value whose `Drop` the test holds (a blocked worker thread needs a
// multi-thread runtime)
// ---------------------------------------------------------------------------

/// While the value drops, the actor reads `Stopping`, its name is free, a
/// `Kill` is answered `Closed` (the lane sealed just before the drop), and
/// the graceful stop that is waiting has not returned; once the drop ends the
/// stop is reported `Graceful`.
#[test]
fn a_graceful_stop_includes_the_value_s_drop() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let (drop_gate, mut key) = gate();
        let (handle, name) = spawn_root(&sys, OnDrop::Hold(drop_gate)).await;
        let call = tokio::spawn({
            let handle = handle.clone();
            async move { handle.stop_within(StopReason::Graceful, BOUND).await }
        });
        key.entered().await;
        assert_eq!(handle.status(), ActorStatus::Stopping);
        assert!(sys.get::<Dropper>(&name).is_none(), "the name is free");
        assert!(matches!(
            handle.stop(StopReason::Kill).await,
            Err(SendError::Closed)
        ));
        assert!(matches!(sys.kill(&name).await, Err(SendError::NotFound)));
        assert!(!call.is_finished(), "the stop waits for the value's drop");
        key.release();
        let outcome = timeout(BOUND, call).await.expect("the stop returns");
        assert_eq!(outcome.unwrap().unwrap(), StopOutcome::Graceful);
        assert_eq!(handle.status(), ActorStatus::Stopped);
    });
}

/// A `Kill` whose actor ends, its value's drop included, inside the call's
/// window is reported `Killed`.
#[test]
fn a_kill_includes_the_value_s_drop() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let (drop_gate, mut key) = gate();
        let (handle, _name) = spawn_root(&sys, OnDrop::Hold(drop_gate)).await;
        let call = tokio::spawn({
            let handle = handle.clone();
            async move { handle.stop_within(StopReason::Kill, BOUND).await }
        });
        key.entered().await;
        assert_ne!(handle.status(), ActorStatus::Stopped);
        assert!(!call.is_finished(), "the kill waits for the value's drop");
        key.release();
        let outcome = timeout(BOUND, call).await.expect("the kill returns");
        assert_eq!(outcome.unwrap().unwrap(), StopOutcome::Killed);
    });
}

/// A value whose drop outlasts the whole ladder (the window, the kill grace,
/// the abort, which cannot interrupt a synchronous `Drop`, and the settle
/// time) gets the outcome of any actor that did not end in time:
/// `Unresponsive`, with the status not yet `Stopped`. The end follows the
/// drop.
#[test]
fn a_drop_longer_than_the_ladder_reads_unresponsive() {
    common::run_multi_thread_only(2, async {
        for reason in [StopReason::Graceful, StopReason::Kill] {
            let sys = ActorSystem::create(uname("sys")).unwrap();
            let (drop_gate, mut key) = gate();
            let (handle, _name) = spawn_root(&sys, OnDrop::Hold(drop_gate)).await;
            let outcome = timeout(
                BOUND,
                handle.stop_within(reason.clone(), Duration::from_millis(20)),
            )
            .await
            .expect("the call is bounded by its ladder")
            .unwrap();
            key.entered().await;
            assert_eq!(outcome, StopOutcome::Unresponsive, "{reason:?}");
            assert_ne!(handle.status(), ActorStatus::Stopped, "{reason:?}");
            key.release();
            timeout(BOUND, handle.wait_stopped())
                .await
                .expect("the actor ends once its value is dropped");
        }
    });
}

/// Shutdown waits for a root's value to drop inside its per-actor window and
/// reports the root `Graceful`.
#[test]
fn shutdown_includes_the_value_s_drop() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let (drop_gate, mut key) = gate();
        let (handle, _name) = spawn_root(&sys, OnDrop::Hold(drop_gate)).await;
        let id = handle.id().clone();
        let call = tokio::spawn({
            let sys = sys.clone();
            async move { sys.shutdown().await }
        });
        key.entered().await;
        assert_ne!(handle.status(), ActorStatus::Stopped);
        assert!(!call.is_finished(), "shutdown waits for the value's drop");
        key.release();
        let report = timeout(BOUND, call)
            .await
            .expect("shutdown returns")
            .unwrap();
        let outcome = report
            .outcomes
            .iter()
            .find(|(actor, _)| *actor == id)
            .map(|(_, report)| report.outcome);
        assert_eq!(outcome, Some(StopOutcome::Graceful));
    });
}

/// `ActorSystem::kill` returns at once; the actor's name is free while its
/// value drops, and its end follows the drop.
#[test]
fn kill_by_name_frees_the_name_before_the_value_s_drop() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let (drop_gate, mut key) = gate();
        let (handle, name) = spawn_root(&sys, OnDrop::Hold(drop_gate)).await;
        sys.kill(&name).await.expect("the kill is accepted");
        key.entered().await;
        assert!(sys.get::<Dropper>(&name).is_none(), "the name is free");
        assert_ne!(handle.status(), ActorStatus::Stopped);
        assert!(!handle.is_alive());
        key.release();
        timeout(BOUND, handle.wait_stopped())
            .await
            .expect("the actor ends once its value is dropped");
    });
}

/// A failed init is answered before the value drops: a start timeout far
/// shorter than the drop still reports the init error.
#[test]
fn a_failed_init_is_reported_before_a_held_drop() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let (drop_gate, key) = gate();
        let result = timeout(
            BOUND,
            Dropper {
                on_drop: OnDrop::Hold(drop_gate),
                fail_init: true,
                attach: Attach::Nothing,
                turn: None,
            }
            .spawn()
            .named(uname("init"))
            .on_system(&sys)
            .start_timeout(Duration::from_millis(50))
            .into_future(),
        )
        .await
        .expect("the spawn returns");
        // Released only once the spawn has answered.
        key.release();
        match result {
            Err(SpawnError::Init(err)) => {
                assert!(err.to_string().contains("init refused"), "{err}");
            }
            Err(other) => panic!("expected the init error, got {other:?}"),
            Ok(_) => panic!("expected the init error, the actor started"),
        }
    });
}

// ---------------------------------------------------------------------------
// A value whose `Drop` panics (both flavors)
// ---------------------------------------------------------------------------

/// A panicking drop is caught: the actor still publishes `Stopped`, so each
/// caller reads the outcome of its own request, never an abort.
#[test]
fn a_panicking_drop_keeps_every_caller_s_outcome() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        for (reason, expected) in [
            (StopReason::Kill, StopOutcome::Killed),
            (StopReason::Graceful, StopOutcome::Graceful),
        ] {
            let (handle, _name) = spawn_root(&sys, OnDrop::Panic).await;
            let outcome = timeout(BOUND, handle.stop_within(reason.clone(), BOUND))
                .await
                .expect("the call returns")
                .unwrap();
            assert_eq!(outcome, expected, "{reason:?}");
        }

        let (handle, name) = spawn_root(&sys, OnDrop::Panic).await;
        sys.kill(&name).await.expect("the kill is accepted");
        timeout(BOUND, handle.wait_stopped())
            .await
            .expect("the actor ends");

        let (handle, _name) = spawn_root(&sys, OnDrop::Panic).await;
        let id = handle.id().clone();
        let report = timeout(BOUND, sys.shutdown())
            .await
            .expect("shutdown returns");
        let outcome = report
            .outcomes
            .iter()
            .find(|(actor, _)| *actor == id)
            .map(|(_, report)| report.outcome);
        assert_eq!(outcome, Some(StopOutcome::Graceful));
    });
}

/// A failed init whose value panics when dropped reports the init error,
/// with and without a start timeout.
#[test]
fn a_failed_init_with_a_panicking_drop_reports_the_init_error() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        for start_timeout in [None, Some(BOUND)] {
            let mut builder = Dropper {
                on_drop: OnDrop::Panic,
                fail_init: true,
                attach: Attach::Nothing,
                turn: None,
            }
            .spawn()
            .named(uname("init"))
            .on_system(&sys);
            if let Some(dur) = start_timeout {
                builder = builder.start_timeout(dur);
            }
            let result = timeout(BOUND, builder.into_future())
                .await
                .expect("the spawn returns");
            match result {
                Err(SpawnError::Init(err)) => {
                    assert!(err.to_string().contains("init refused"), "{err}");
                }
                Err(other) => panic!("expected the init error, got {other:?}"),
                Ok(_) => panic!("expected the init error, the actor started"),
            }
        }
    });
}

// ---------------------------------------------------------------------------
// A supervised restart whose init fails while its value's drop is held
// ---------------------------------------------------------------------------

/// The child's first incarnation runs; a message makes it fail; its second
/// incarnation's `pre_start` fails and its value's drop is held.
struct Parent {
    /// What the failing incarnation's value does when it is dropped.
    on_drop: Arc<Mutex<Option<OnDrop>>>,
    reports: mpsc::UnboundedSender<StopReason>,
    child: String,
    /// The failing incarnation registers a timer before its init fails.
    timer: bool,
    start_timeout: Duration,
    kill_grace: Duration,
}

impl Actor for Parent {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let on_drop = self.on_drop.clone();
        let built = Arc::new(AtomicUsize::new(0));
        let timer = self.timer;
        ctx.spawn_child(move || {
            if built.fetch_add(1, Ordering::SeqCst) == 1 {
                if let Some(on_drop) = on_drop.lock().unwrap().take() {
                    return Failing {
                        inner: Dropper {
                            on_drop,
                            fail_init: true,
                            attach: Attach::Nothing,
                            turn: None,
                        },
                        timer,
                    };
                }
            }
            Failing {
                inner: Dropper::new(OnDrop::Nothing),
                timer: false,
            }
        })
        .named(self.child.clone())
        .restart_type(RestartType::Permanent)
        .start_timeout(self.start_timeout)
        .kill_grace(self.kill_grace)
        .await
        .map_err(ActorError::from)?;
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
        let _ = self.reports.send(event.reason.clone());
        Ok(())
    }
}

/// A child that fails its turn on any message.
struct Failing {
    inner: Dropper,
    timer: bool,
}

impl Actor for Failing {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.inner.fail_init {
            if self.timer {
                ctx.every_with(Duration::from_secs(3600), || ())
                    .await
                    .expect("the timer registers");
            }
            return Err(ActorError::user("init refused"));
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("the turn failed"))
    }
}

/// Runs a child whose first incarnation fails its turn and whose second
/// fails its init; that incarnation's value does `on_drop` when it is
/// dropped. A held drop is released `hold_for` after it began. Returns the
/// report of the second incarnation.
async fn restart_attempt_with_a_slow_drop(
    timer: bool,
    start_timeout: Duration,
    kill_grace: Duration,
    on_drop: OnDrop,
    held: Option<(GateKey, Duration)>,
) -> StopReason {
    let sys = ActorSystem::create(uname("sys")).unwrap();
    let (reports_tx, mut reports) = mpsc::unbounded_channel();
    let child = uname("child");
    let _parent = Parent {
        on_drop: Arc::new(Mutex::new(Some(on_drop))),
        reports: reports_tx,
        child: child.clone(),
        timer,
        start_timeout,
        kill_grace,
    }
    .spawn()
    .named(uname("parent"))
    .on_system(&sys)
    .supervisor()
    .await
    .expect("the parent starts");
    let first = timeout(BOUND, async {
        loop {
            if let Some(handle) = sys.get::<Failing>(&child) {
                break handle;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the child is registered");
    first.notify(()).await.expect("the message is queued");
    let crash = timeout(BOUND, reports.recv()).await.unwrap().unwrap();
    assert!(
        matches!(&crash, StopReason::Failure(ActorError::User(text)) if text == "the turn failed"),
        "{crash:?}"
    );
    if let Some((mut key, hold_for)) = held {
        key.entered().await;
        tokio::time::sleep(hold_for).await;
        key.release();
    }
    timeout(BOUND, reports.recv()).await.unwrap().unwrap()
}

/// The restart attempt reads the init error before the value's drop begins,
/// so a start timeout that passes while the drop is held does not turn the
/// attempt's end into a start timeout.
#[test]
fn a_restart_attempt_s_failed_init_is_reported_before_a_held_drop() {
    common::run_multi_thread_only(2, async {
        let (drop_gate, key) = gate();
        // Released past the attempt's 50 ms start timeout, by a margin a
        // loaded host keeps.
        let attempt = restart_attempt_with_a_slow_drop(
            false,
            Duration::from_millis(50),
            BOUND,
            OnDrop::Hold(drop_gate),
            Some((key, Duration::from_millis(300))),
        )
        .await;
        assert!(
            matches!(&attempt, StopReason::Failure(ActorError::User(text)) if text == "init refused"),
            "the attempt reports its init error: {attempt:?}"
        );
    });
}

/// A failed restart attempt whose child registered a timer, with a value
/// whose drop outlasts the attempt's 50 ms kill grace: the init ack is
/// answered after the lane is sealed, so the `Kill` the attempt raises on
/// reading the error is refused, the abort after the grace cannot interrupt
/// the drop, and the attempt reports the init error.
#[test]
fn a_restart_attempt_with_a_timer_keeps_its_init_error() {
    common::run(4, async {
        let attempt = restart_attempt_with_a_slow_drop(
            true,
            BOUND,
            Duration::from_millis(50),
            OnDrop::Sleep(Duration::from_millis(300)),
            None,
        )
        .await;
        assert!(
            matches!(&attempt, StopReason::Failure(ActorError::User(text)) if text == "init refused"),
            "the attempt reports its init error, not Kill: {attempt:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// An abort that lands while the value drops, with a timer or a stream
// ---------------------------------------------------------------------------

/// The kill grace of the actors below: the abort that `stop(Kill)`
/// schedules lands no sooner than this after it, far past the few steps from
/// that `Kill` to the value's drop.
const ABORT_AFTER: Duration = Duration::from_secs(1);

/// A `Kill` whose abort lands while the value's drop is held: the abort
/// cannot interrupt the drop and no await follows it, so the actor still
/// publishes `Stopped`, and a call waiting past the abort reads `Killed`,
/// with a timer or a stream as without one.
#[test]
fn a_kill_whose_abort_lands_during_the_value_s_drop_reads_killed() {
    common::run_multi_thread_only(2, async {
        for attach in [Attach::Timer, Attach::Stream] {
            let mut runs = 1;
            while !abort_during_the_value_s_drop(attach).await {
                assert!(
                    runs < 3,
                    "{attach:?}: no run's drop began before the abort's earliest deadline"
                );
                runs += 1;
            }
        }
    });
}

/// One run of the scenario above. The waiting call and the `Kill` that
/// schedules the abort are both raised inside a held turn, while the actor
/// is registered, and the turn is then released. A probe whose turn is never
/// released is killed right after with the same grace: the reaper fires its
/// entries in deadline order, so the probe ends only once the abort of the
/// actor under test has landed, and the drop is released after that.
/// Returns `false`, without checking the outcome, when the drop began only
/// after the abort's earliest deadline: the abort may then have landed at an
/// earlier await.
async fn abort_during_the_value_s_drop(attach: Attach) -> bool {
    let sys = ActorSystem::create(uname("sys")).unwrap();
    let (drop_gate, mut key) = gate();
    let mut actor = Dropper::new(OnDrop::Hold(drop_gate));
    actor.attach = attach;
    let (handle, release_turn) = spawn_held(&sys, actor).await;
    let (probe, _probe_turn) = spawn_held(&sys, Dropper::new(OnDrop::Nothing)).await;

    // The first poll raises the waiting call's Kill.
    let mut waiting = std::pin::pin!(handle.stop_within(StopReason::Kill, BOUND));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(waiting.as_mut(), &mut cx).is_pending());
    let raised = tokio::time::Instant::now();
    handle
        .stop(StopReason::Kill)
        .await
        .expect("the Kill is accepted");
    probe
        .stop(StopReason::Kill)
        .await
        .expect("the probe's Kill is accepted");
    let _ = release_turn.send(());

    let before_the_abort = key.entered().await < raised + ABORT_AFTER;
    timeout(BOUND, probe.wait_stopped())
        .await
        .expect("the probe is aborted");
    key.release();
    let outcome = timeout(BOUND, waiting)
        .await
        .expect("the call returns")
        .unwrap();
    if before_the_abort {
        assert_eq!(outcome, StopOutcome::Killed, "{attach:?}");
    }
    before_the_abort
}

/// Spawns `actor` on `sys` with the kill grace above and waits until its
/// first turn is held; returns its handle and the sender that releases it.
async fn spawn_held(
    sys: &Arc<ActorSystem>,
    mut actor: Dropper,
) -> (ActorHandle<Dropper>, oneshot::Sender<()>) {
    let (release_turn, turn_released) = oneshot::channel();
    let (turn_began, began) = oneshot::channel();
    actor.turn = Some((turn_released, turn_began));
    let handle = actor
        .spawn()
        .named(uname("root"))
        .on_system(sys)
        .kill_grace(ABORT_AFTER)
        .await
        .expect("the actor starts");
    handle
        .notify(Tick)
        .await
        .expect("the actor takes the message");
    timeout(BOUND, began)
        .await
        .expect("the turn begins")
        .expect("the actor holds its turn");
    (handle, release_turn)
}

// ---------------------------------------------------------------------------
// Queued messages
// ---------------------------------------------------------------------------

/// A message whose drop panics.
struct Bomb;

impl Drop for Bomb {
    fn drop(&mut self) {
        panic!("a queued message panicked when dropped");
    }
}

enum QMsg {
    /// Holds the turn until released; says when the turn began.
    Hold(oneshot::Receiver<()>, oneshot::Sender<()>),
    Queued(#[allow(dead_code)] Bomb),
}

struct Queues;

impl Actor for Queues {
    type Message = QMsg;
    type Response = ();

    async fn handle(&mut self, msg: QMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let QMsg::Hold(release, began) = msg {
            let _ = began.send(());
            let _ = release.await;
        }
        Ok(())
    }
}

/// Panicking messages queued on both lanes, two in each, are dropped at the
/// end one at a time: the actor still publishes `Stopped` (its `Kill` reads
/// `Killed`). A second panic inside the unwinding of the first would abort
/// the process, so the scenario runs in a child process of this test binary.
#[test]
fn panicking_queued_messages_on_both_lanes_end_in_one_reason() {
    if !common::in_a_child_process("panicking_queued_messages_on_both_lanes_end_in_one_reason") {
        return;
    }
    common::run(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let handle = Queues
            .spawn()
            .named(uname("queues"))
            .on_system(&sys)
            .with_config(ActorConfig::default().with_priority_lane(4))
            .await
            .expect("the actor starts");
        let (release, held) = oneshot::channel();
        let (began_tx, began) = oneshot::channel();
        handle.notify(QMsg::Hold(held, began_tx)).await.unwrap();
        timeout(BOUND, began).await.unwrap().unwrap();
        handle.notify(QMsg::Queued(Bomb)).await.unwrap();
        handle.notify(QMsg::Queued(Bomb)).await.unwrap();
        handle.notify_priority(QMsg::Queued(Bomb)).await.unwrap();
        handle.notify_priority(QMsg::Queued(Bomb)).await.unwrap();
        // The first poll raises the Kill; it is read at the end of the held
        // turn, with every message still queued.
        let mut call = std::pin::pin!(handle.stop_within(StopReason::Kill, BOUND));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(call.as_mut(), &mut cx).is_pending());
        let _ = release.send(());
        let outcome = timeout(BOUND, call).await.expect("the call returns");
        assert_eq!(outcome.unwrap(), StopOutcome::Killed);
    });
}

/// An actor whose init waits for the test, then fails.
struct GatedInit {
    go: Option<oneshot::Receiver<()>>,
}

/// A message whose drop is held until the test releases it.
struct HeldMessage(Gate);

impl Drop for HeldMessage {
    fn drop(&mut self) {
        if let Some(entered) = self.0.entered.take() {
            let _ = entered.send(tokio::time::Instant::now());
        }
        let release = &self.0.release;
        tokio::task::block_in_place(|| {
            let _ = release.recv();
        });
    }
}

impl Actor for GatedInit {
    type Message = HeldMessage;
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(go) = self.go.take() {
            let _ = go.await;
        }
        Err(ActorError::user("init refused"))
    }

    async fn handle(
        &mut self,
        _msg: HeldMessage,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        Ok(())
    }
}

/// A failed init is answered right after the name is freed, before the
/// queued messages drop: a message whose drop is held until the spawn has
/// answered does not turn the init error into a start timeout.
#[test]
fn a_failed_init_is_reported_before_a_held_queued_message_drop() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let name = uname("gated");
        let (go_tx, go) = oneshot::channel();
        let spawn = tokio::spawn({
            let (sys, name) = (sys.clone(), name.clone());
            async move {
                GatedInit { go: Some(go) }
                    .spawn()
                    .named(name)
                    .on_system(&sys)
                    .start_timeout(Duration::from_secs(1))
                    .await
                    .map(|_| ())
            }
        });
        let handle = timeout(BOUND, async {
            loop {
                if let Some(handle) = sys.get::<GatedInit>(&name) {
                    break handle;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the actor is registered");
        let (message_gate, key) = gate();
        handle
            .notify(HeldMessage(message_gate))
            .await
            .expect("the message is queued");
        let _ = go_tx.send(());
        let result = timeout(BOUND, spawn)
            .await
            .expect("the spawn returns")
            .unwrap();
        // Released only once the spawn has answered.
        key.release();
        match result {
            Err(SpawnError::Init(err)) => {
                assert!(err.to_string().contains("init refused"), "{err}");
            }
            other => panic!("expected the init error, got {other:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// The same outcomes with the actor on a current-thread runtime of its own,
// its value's drop sleeping on that runtime's only thread, and the caller on
// the test's runtime
// ---------------------------------------------------------------------------

/// A current-thread runtime on a thread of its own, kept running until the
/// value is dropped.
struct OwnRuntime {
    handle: tokio::runtime::Handle,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OwnRuntime {
    fn start() -> Self {
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let (stop, stopped) = oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("the runtime builds");
            let _ = handle_tx.send(runtime.handle().clone());
            runtime.block_on(async {
                let _ = stopped.await;
            });
        });
        Self {
            handle: handle_rx.recv().expect("the runtime starts"),
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    /// Spawns `actor` on this runtime and returns how the spawn ended.
    async fn spawn(
        &self,
        sys: &Arc<ActorSystem>,
        actor: Dropper,
        start_timeout: Option<Duration>,
    ) -> Result<ActorHandle<Dropper>, SpawnError> {
        let sys = sys.clone();
        let spawned = self.handle.spawn(async move {
            let mut builder = actor
                .spawn()
                .named(uname("own"))
                .on_system(&sys)
                .kill_grace(Duration::from_millis(50));
            if let Some(dur) = start_timeout {
                builder = builder.start_timeout(dur);
            }
            builder.await
        });
        timeout(BOUND, spawned)
            .await
            .expect("the spawn returns")
            .expect("the spawn task ends")
    }
}

impl Drop for OwnRuntime {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A start timeout is not covered here: the spawn awaits its ack on the
/// actor's own runtime, whose only thread the drop blocks, so the ack's
/// place in the end cannot be seen. On the current-thread runtime it is
/// covered by `a_failed_init_answers_its_ack_before_its_queued_messages_and_value_drop`
/// (a unit test reading the ack at each drop) and by
/// `a_restart_attempt_with_a_timer_keeps_its_init_error`.
#[test]
fn a_slow_drop_on_a_current_thread_runtime_keeps_every_caller_s_outcome() {
    common::run_current_thread_only(async {
        let own = OwnRuntime::start();
        let sys = ActorSystem::create(uname("sys")).unwrap();

        // A drop inside the window: the graceful stop waits for it.
        let handle = own
            .spawn(
                &sys,
                Dropper::new(OnDrop::Sleep(Duration::from_millis(300))),
                None,
            )
            .await
            .expect("the actor starts");
        let outcome = timeout(BOUND, handle.stop_within(StopReason::Graceful, BOUND))
            .await
            .expect("the call returns")
            .unwrap();
        assert_eq!(outcome, StopOutcome::Graceful);

        // A drop longer than the whole ladder (20 ms window, 50 ms kill
        // grace, the abort and its 100 ms settle wait): Unresponsive, and the
        // end follows the drop.
        let handle = own
            .spawn(
                &sys,
                Dropper::new(OnDrop::Sleep(Duration::from_millis(800))),
                None,
            )
            .await
            .expect("the actor starts");
        let outcome = timeout(
            BOUND,
            handle.stop_within(StopReason::Graceful, Duration::from_millis(20)),
        )
        .await
        .expect("the call returns")
        .unwrap();
        assert_eq!(outcome, StopOutcome::Unresponsive);
        timeout(BOUND, handle.wait_stopped())
            .await
            .expect("the actor ends once its value is dropped");

        // Shutdown waits for the drop and reports the root Graceful.
        let handle = own
            .spawn(
                &sys,
                Dropper::new(OnDrop::Sleep(Duration::from_millis(300))),
                None,
            )
            .await
            .expect("the actor starts");
        let id = handle.id().clone();
        let report = timeout(BOUND, sys.shutdown())
            .await
            .expect("shutdown returns");
        let outcome = report
            .outcomes
            .iter()
            .find(|(actor, _)| *actor == id)
            .map(|(_, report)| report.outcome);
        assert_eq!(outcome, Some(StopOutcome::Graceful));
    });
}

/// A stream whose drop is held until the test releases it.
struct HeldDropStream(Gate);

impl futures_core::Stream for HeldDropStream {
    type Item = ();

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<()>> {
        std::task::Poll::Pending
    }
}

impl Drop for HeldDropStream {
    fn drop(&mut self) {
        if let Some(entered) = self.0.entered.take() {
            let _ = entered.send(tokio::time::Instant::now());
        }
        let release = &self.0.release;
        tokio::task::block_in_place(|| {
            let _ = release.recv();
        });
    }
}

/// An actor that attaches a [`HeldDropStream`] and then fails its init.
struct StreamThenFail {
    stream: Option<HeldDropStream>,
}

impl Actor for StreamThenFail {
    type Message = Tick;
    type Response = ();

    async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(stream) = self.stream.take() {
            ctx.add_stream(stream);
        }
        Err(ActorError::user("init refused"))
    }

    async fn handle(&mut self, _msg: Tick, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A stream's drop runs while the forwarders are ended, before the lane is
/// sealed and the init ack answered: a slow one delays a failed init's
/// answer, so a start timeout shorter than that drop reports
/// `StartTimeout`.
#[test]
fn a_slow_stream_drop_delays_a_failed_init_s_answer() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let (stream_gate, key) = gate();
        let result = timeout(
            BOUND,
            StreamThenFail {
                stream: Some(HeldDropStream(stream_gate)),
            }
            .spawn()
            .named(uname("stream-init"))
            .on_system(&sys)
            .start_timeout(Duration::from_millis(50))
            .into_future(),
        )
        .await
        .expect("the spawn returns");
        // Released only once the spawn has answered.
        key.release();
        match result {
            Err(SpawnError::StartTimeout) => {}
            Err(other) => panic!("expected a start timeout, got {other:?}"),
            Ok(_) => panic!("expected a start timeout, the actor started"),
        }
    });
}
