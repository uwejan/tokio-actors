#![allow(dead_code)]
//! One terminal reason for every party that waits on an actor's end.
//!
//! The runtime status plane carries the reason an actor ended with, written
//! once and together with the terminal status. `wait_stopped()`, a supervisor's
//! report of its child, a request or status query that was queued when the
//! actor stopped, and the reports of `kill_by_id` and of a system shutdown all
//! read that one value. The rows of the first test compare the readers of one
//! ending with each other, for every way an actor can end.

use std::future::Future;
use std::pin::{pin, Pin};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorId, ActorResult, ActorSystem, AskError, ChildEvent, RestartType,
    SendError, Shutdown, SpawnError, StopOutcome, StopReason,
};

mod common;

/// The bound on every outcome a test awaits that is not derived from a
/// configured window.
const BOUND: Duration = Duration::from_secs(10);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// The longest a held `Block` turn holds its thread when the test never
/// releases it: past every bound a test awaits.
const GATE_CAP: Duration = Duration::from_secs(15);

/// How long a test watches for a late duplicate or a late change.
const QUIET: Duration = Duration::from_millis(300);

/// The time between a `Kill` and the abort of an actor whose builder sets no
/// kill grace. An integration test cannot read the crate's constant, so the
/// value is named here once.
const ASSUMED_KILL_GRACE: Duration = Duration::from_millis(120);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("terminal-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

/// Awaits `fut` for at most `bound`; a reverted build fails here instead of
/// hanging.
async fn within<T>(bound: Duration, what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(bound, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {bound:?} waiting for: {what}"),
    }
}

/// Polls `fut` once with a waker nobody waits on. A future that sends its
/// request before its first await has sent it when this returns.
fn poll_once<F: Future + ?Sized>(fut: Pin<&mut F>) -> Poll<F::Output> {
    fut.poll(&mut Context::from_waker(Waker::noop()))
}

/// The value of a future that must be ready at its first poll.
fn ready_now<F: Future>(fut: F, what: &str) -> F::Output {
    match poll_once(pin!(fut)) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("not ready at once: {what}"),
    }
}

/// A hold the test opens and closes. A waiter does not depend on the
/// runtime's driver, so a held turn still ends at once when it is released
/// while a worker thread is blocked.
#[derive(Debug, Default)]
struct Gate {
    held: AtomicBool,
    wake: tokio::sync::Notify,
}

impl Gate {
    fn hold(&self) {
        self.held.store(true, Ordering::SeqCst);
    }

    fn is_held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    /// Returns once the gate is not held.
    async fn wait(&self) {
        loop {
            let mut woken = pin!(self.wake.notified());
            woken.as_mut().enable();
            if !self.held.load(Ordering::SeqCst) {
                return;
            }
            woken.await;
        }
    }
}

/// Polls `pred` until it holds. The bound only limits how long a real hang takes to report. A shorter `bound` from a caller is
/// raised to `HANG_BOUND`.
async fn wait_until(bound: Duration, what: &str, mut pred: impl FnMut() -> bool) {
    let bound = bound.max(HANG_BOUND);
    let deadline = Instant::now() + bound;
    loop {
        if pred() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {bound:?} waiting for: {what}"
        );
        sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// The actor under test
// ---------------------------------------------------------------------------

/// A message that panics when it is dropped while it is armed, and that
/// blocks its thread for `block_ms` first.
struct Bomb {
    armed: Arc<AtomicBool>,
    /// Set when the drop began.
    dropping: Arc<AtomicBool>,
    block_ms: u64,
}

impl Bomb {
    fn armed(armed: &Arc<AtomicBool>) -> Self {
        Bomb {
            armed: armed.clone(),
            dropping: Arc::new(AtomicBool::new(false)),
            block_ms: 0,
        }
    }
}

impl Drop for Bomb {
    fn drop(&mut self) {
        self.dropping.store(true, Ordering::SeqCst);
        if self.block_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.block_ms));
        }
        if self.armed.load(Ordering::SeqCst) && !std::thread::panicking() {
            panic!("a queued message was dropped");
        }
    }
}

enum Msg {
    /// Answers at once.
    Ping,
    /// Returns an error from `handle`.
    Fail,
    /// Awaits forever inside `handle`.
    Park,
    /// Holds the turn open until the test clears `Knobs::hold_turn`.
    Hold,
    /// Holds the turn open the same way, then returns an error.
    HoldThenFail,
    /// A message that is never handled; its drop panics.
    Mail(Bomb),
    /// Blocks the thread inside `handle` while `Knobs::hold_block` is held
    /// (for at most `GATE_CAP`), or for 1500 ms when it is not: a turn that
    /// never reaches a boundary and cannot be aborted.
    Block,
}

/// What the tests set on and read from the actors they spawn.
#[derive(Clone, Debug, Default)]
struct Knobs {
    /// While set, the actor value's `Drop` panics.
    drop_panics: Arc<AtomicBool>,
    /// While set, `on_stopped` returns an error.
    on_stopped_fails: Arc<AtomicBool>,
    /// Set when a `Hold`, `HoldThenFail`, `Park` or `Block` turn began.
    in_turn: Arc<AtomicBool>,
    /// While set, an instance built after the first awaits forever in
    /// `pre_start`.
    hang_restarts: Arc<AtomicBool>,
    /// Instances built.
    built: Arc<AtomicUsize>,
    /// Instances dropped.
    dropped: Arc<AtomicUsize>,
    /// Calls of `on_stopped`.
    on_stopped_runs: Arc<AtomicUsize>,
    /// Calls of `pre_stop` that began.
    pre_stops: Arc<AtomicUsize>,
    /// Milliseconds `on_stopped` awaits before it returns.
    on_stopped_ms: Arc<AtomicU64>,
    /// Milliseconds `pre_start` awaits before it returns.
    init_ms: Arc<AtomicU64>,
    /// Milliseconds `pre_stop` awaits before it accepts.
    pre_stop_ms: Arc<AtomicU64>,
    /// While set, a `Hold` turn stays open.
    hold_turn: Arc<Gate>,
    /// While set, `pre_start` stays open.
    hold_init: Arc<Gate>,
    /// While set, `pre_stop` stays open.
    hold_pre_stop: Arc<Gate>,
    /// While set, `on_stopped` stays open.
    hold_on_stopped: Arc<Gate>,
    /// While set, the actor value's `Drop` blocks its thread.
    hold_drop: Arc<Gate>,
    /// While set, a `Block` turn holds its thread.
    hold_block: Arc<Gate>,
    /// Set when the actor value's `Drop` began.
    drop_entered: Arc<AtomicBool>,
}

impl Knobs {
    /// Knobs whose `Hold` turns stay open until [`release`](Self::release).
    fn held() -> Self {
        let knobs = Knobs::default();
        knobs.hold_turn.hold();
        knobs
    }

    /// Lets every held turn end.
    fn release(&self) {
        self.hold_turn.release();
    }

    /// A guard that releases every gate of these knobs when it drops, also
    /// while a failed assertion unwinds, so a blocked `Drop` or turn never
    /// outlives the test that holds it.
    fn release_on_drop(&self) -> ReleasesEveryGate {
        ReleasesEveryGate(self.clone())
    }
}

/// See [`Knobs::release_on_drop`].
struct ReleasesEveryGate(Knobs);

impl Drop for ReleasesEveryGate {
    fn drop(&mut self) {
        for gate in [
            &self.0.hold_turn,
            &self.0.hold_init,
            &self.0.hold_pre_stop,
            &self.0.hold_on_stopped,
            &self.0.hold_drop,
            &self.0.hold_block,
        ] {
            gate.release();
        }
    }
}

#[derive(Debug)]
struct Subject {
    knobs: Knobs,
    nth: usize,
    fail_init: bool,
}

impl Subject {
    fn new(knobs: &Knobs) -> Self {
        Subject {
            nth: knobs.built.fetch_add(1, Ordering::SeqCst),
            knobs: knobs.clone(),
            fail_init: false,
        }
    }
}

impl Actor for Subject {
    type Message = Msg;
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let nap = self.knobs.init_ms.load(Ordering::SeqCst);
        if nap > 0 {
            sleep(Duration::from_millis(nap)).await;
        }
        self.knobs.hold_init.wait().await;
        if self.fail_init {
            return Err(ActorError::user("the initialization fails"));
        }
        while self.nth > 0 && self.knobs.hang_restarts.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Ping => Ok(()),
            Msg::Fail => Err(ActorError::user("the handler fails")),
            Msg::Park => {
                self.knobs.in_turn.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok(())
            }
            Msg::Mail(bomb) => {
                std::mem::forget(bomb);
                Ok(())
            }
            Msg::Hold => {
                self.knobs.in_turn.store(true, Ordering::SeqCst);
                self.knobs.hold_turn.wait().await;
                Ok(())
            }
            Msg::HoldThenFail => {
                self.knobs.in_turn.store(true, Ordering::SeqCst);
                self.knobs.hold_turn.wait().await;
                Err(ActorError::user("the handler fails"))
            }
            Msg::Block => {
                self.knobs.in_turn.store(true, Ordering::SeqCst);
                if self.knobs.hold_block.is_held() {
                    let cap = std::time::Instant::now() + GATE_CAP;
                    while self.knobs.hold_block.is_held() && std::time::Instant::now() < cap {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(1500));
                }
                Ok(())
            }
        }
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.knobs.pre_stops.fetch_add(1, Ordering::SeqCst);
        let nap = self.knobs.pre_stop_ms.load(Ordering::SeqCst);
        if nap > 0 {
            sleep(Duration::from_millis(nap)).await;
        }
        self.knobs.hold_pre_stop.wait().await;
        true
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.knobs.on_stopped_runs.fetch_add(1, Ordering::SeqCst);
        let nap = self.knobs.on_stopped_ms.load(Ordering::SeqCst);
        if nap > 0 {
            sleep(Duration::from_millis(nap)).await;
        }
        self.knobs.hold_on_stopped.wait().await;
        if self.knobs.on_stopped_fails.load(Ordering::SeqCst) {
            return Err(ActorError::user("on_stopped fails"));
        }
        Ok(())
    }
}

impl Drop for Subject {
    fn drop(&mut self) {
        self.knobs.drop_entered.store(true, Ordering::SeqCst);
        let hold = || {
            while self.knobs.hold_drop.is_held() {
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        // On a multi-thread runtime the blocked worker hands its queue to
        // another thread first, so the tasks the end woke (a request whose
        // reply was dropped) are not parked behind the hold.
        match tokio::runtime::Handle::try_current() {
            Ok(rt) if rt.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(hold)
            }
            _ => hold(),
        }
        self.knobs.dropped.fetch_add(1, Ordering::SeqCst);
        if self.knobs.drop_panics.load(Ordering::SeqCst) && !std::thread::panicking() {
            panic!("the actor value's drop panics");
        }
    }
}

// ---------------------------------------------------------------------------
// The supervisor that reports its child
// ---------------------------------------------------------------------------

type Events = Arc<Mutex<Vec<ChildEvent>>>;

/// The message that holds a [`Top`] turn open until the knobs release it.
const HOLD: u64 = u64::MAX;

struct Top {
    child: String,
    knobs: Knobs,
    restart: RestartType,
    shutdown: Shutdown,
    start_timeout: Option<Duration>,
    kill_grace: Duration,
    events: Events,
}

impl Actor for Top {
    /// Milliseconds the handler awaits.
    type Message = u64;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let knobs = self.knobs.clone();
        let mut builder = ctx
            .spawn_child(move || Subject::new(&knobs))
            .named(self.child.clone())
            .restart_type(self.restart)
            .shutdown(self.shutdown)
            .kill_grace(self.kill_grace);
        if let Some(timeout) = self.start_timeout {
            builder = builder.start_timeout(timeout);
        }
        builder.await?;
        Ok(())
    }

    async fn handle(&mut self, msg: u64, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if msg == HOLD {
            self.knobs.in_turn.store(true, Ordering::SeqCst);
            self.knobs.hold_turn.wait().await;
        } else {
            sleep(Duration::from_millis(msg)).await;
        }
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

/// A supervisor and its child, spawned on the default system.
struct Tree {
    top: ActorHandle<Top>,
    child: String,
    events: Events,
    knobs: Knobs,
}

async fn tree(
    restart: RestartType,
    shutdown: Shutdown,
    start_timeout: Option<Duration>,
    knobs: Knobs,
) -> Tree {
    tree_with_grace(restart, shutdown, start_timeout, ASSUMED_KILL_GRACE, knobs).await
}

async fn tree_with_grace(
    restart: RestartType,
    shutdown: Shutdown,
    start_timeout: Option<Duration>,
    kill_grace: Duration,
    knobs: Knobs,
) -> Tree {
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let child = uname("subject");
    let top = Top {
        child: child.clone(),
        knobs: knobs.clone(),
        restart,
        shutdown,
        start_timeout,
        kill_grace,
        events: events.clone(),
    }
    .spawn()
    .named(uname("top"))
    .supervisor()
    .await
    .expect("the supervisor spawns");
    Tree {
        top,
        child,
        events,
        knobs,
    }
}

impl Tree {
    fn handle(&self) -> ActorHandle<Subject> {
        ActorSystem::default()
            .get::<Subject>(&self.child)
            .expect("the child is registered")
    }

    fn id(&self) -> ActorId {
        ActorId::from(self.child.clone())
    }

    /// The supervisor's reports of the child, oldest first.
    fn reports(&self) -> Vec<ChildEvent> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.child_name.as_deref() == Some(self.child.as_str()))
            .cloned()
            .collect()
    }

    /// The supervisor's first report of the child, awaited.
    async fn first_report(&self) -> ChildEvent {
        wait_until(BOUND, "the supervisor's report of the child", || {
            !self.reports().is_empty()
        })
        .await;
        self.reports()[0].clone()
    }

    async fn finish(self) {
        let _ = self.top.stop(StopReason::Graceful).await;
        let _ = timeout(BOUND, self.top.wait_stopped()).await;
    }
}

/// The reason one ending reads as on the plane and in the supervisor's
/// report: asserted equal, and returned.
async fn one_reason(tree: &Tree, handle: &ActorHandle<Subject>, label: &str) -> StopReason {
    let plane = within(BOUND, label, handle.wait_stopped()).await;
    let reported = tree.first_report().await;
    assert_eq!(plane, reported.reason, "{label}: the plane and the report");
    plane
}

// ---------------------------------------------------------------------------
// Every reader reads the one reason
// ---------------------------------------------------------------------------

/// With the `tracing` feature: a drop panic after the seal left exactly one
/// warning naming the actor. The capture is installed when the test starts.
fn assert_one_late_warning(id: &ActorId, label: &str) {
    #[cfg(feature = "tracing")]
    {
        let token = format!("actor_id={id} ");
        let lines: Vec<String> = warnings::install()
            .lock()
            .unwrap()
            .iter()
            .filter(|line| {
                line.contains(&token) && line.contains("after the actor's reason was final")
            })
            .cloned()
            .collect();
        assert_eq!(lines.len(), 1, "{label}: one warning: {lines:?}");
    }
    #[cfg(not(feature = "tracing"))]
    let _ = (id, label);
}

/// Installs the warning capture of this binary when the `tracing` feature is
/// on; nothing otherwise.
fn capture_warnings() {
    #[cfg(feature = "tracing")]
    let _ = warnings::install();
}

#[test]
fn terminal_reason_is_one_value_on_every_plane() {
    capture_warnings();
    common::run(2, async {
        // A graceful stop.
        let t = tree(
            RestartType::Temporary,
            Shutdown::Timeout(BOUND),
            None,
            Knobs::default(),
        )
        .await;
        let h = t.handle();
        h.stop(StopReason::Graceful).await.unwrap();
        assert_eq!(one_reason(&t, &h, "graceful").await, StopReason::Graceful);
        t.finish().await;

        // `on_stopped` returns an error.
        let knobs = Knobs::default();
        knobs.on_stopped_fails.store(true, Ordering::SeqCst);
        let t = tree(
            RestartType::Temporary,
            Shutdown::Timeout(BOUND),
            None,
            knobs,
        )
        .await;
        let h = t.handle();
        h.stop(StopReason::Graceful).await.unwrap();
        let reason = one_reason(&t, &h, "on_stopped fails").await;
        assert_eq!(
            reason,
            StopReason::Failure(ActorError::user("on_stopped fails"))
        );
        t.finish().await;

        // `handle` returns an error.
        let t = tree(
            RestartType::Temporary,
            Shutdown::Timeout(BOUND),
            None,
            Knobs::default(),
        )
        .await;
        let h = t.handle();
        let err = h.send(Msg::Fail).await.unwrap_err();
        assert!(matches!(err, AskError::Actor(_)), "{err:?}");
        let reason = one_reason(&t, &h, "handle fails").await;
        assert_eq!(
            reason,
            StopReason::Failure(ActorError::user("the handler fails"))
        );
        t.finish().await;

        // `pre_start` returns an error: the awaited spawn and the plane of a
        // detached spawn read the same error.
        let spawned = {
            let knobs = Knobs::default();
            let mut subject = Subject::new(&knobs);
            subject.fail_init = true;
            subject.spawn().named(uname("init-fails")).await
        };
        let err = match spawned {
            Err(SpawnError::Init(err)) => *err,
            other => panic!("expected SpawnError::Init, got {other:?}"),
        };
        let detached = {
            let knobs = Knobs::default();
            let mut subject = Subject::new(&knobs);
            subject.fail_init = true;
            subject
                .spawn()
                .named(uname("init-fails-detached"))
                .detached()
                .await
                .unwrap()
        };
        let plane = within(BOUND, "the detached spawn's end", detached.wait_stopped()).await;
        assert_eq!(plane, StopReason::Failure(err), "pre_start fails");

        // A `Kill` at a turn boundary.
        let t = tree(
            RestartType::Temporary,
            Shutdown::Timeout(BOUND),
            None,
            Knobs::default(),
        )
        .await;
        let h = t.handle();
        h.stop(StopReason::Kill).await.unwrap();
        assert_eq!(
            one_reason(&t, &h, "kill at a boundary").await,
            StopReason::Kill
        );
        t.finish().await;

        // An abort of a turn parked at an await, through `kill_by_id`.
        let t = tree(
            RestartType::Temporary,
            Shutdown::Timeout(BOUND),
            None,
            Knobs::default(),
        )
        .await;
        let h = t.handle();
        h.notify(Msg::Park).await.unwrap();
        wait_until(BOUND, "the turn to begin", || {
            t.knobs.in_turn.load(Ordering::SeqCst)
        })
        .await;
        let report = ActorSystem::default()
            .kill_by_id(&t.id())
            .await
            .expect("registered");
        assert_eq!(report.reason, Some(StopReason::Kill), "{report:?}");
        assert_eq!(
            one_reason(&t, &h, "abort of a parked turn").await,
            StopReason::Kill
        );
        t.finish().await;

        // Queued mail whose drop panics: the mail drops after the reason is
        // final, so the reason stands: `Graceful` after a graceful stop,
        // `Kill` after a `Kill` at a turn boundary, an earlier failure after
        // a failing handler.
        for (label, how) in [("graceful", 0), ("kill", 1), ("failure", 2)] {
            // A kill grace of five seconds: the `Kill` is read at the turn
            // boundary, and the abort backstop never cuts the turn.
            let t = tree_with_grace(
                RestartType::Temporary,
                Shutdown::Timeout(BOUND),
                None,
                Duration::from_secs(5),
                Knobs::held(),
            )
            .await;
            let h = t.handle();
            let armed = Arc::new(AtomicBool::new(false));
            match how {
                2 => h.notify(Msg::HoldThenFail).await.unwrap(),
                _ => h.notify(Msg::Hold).await.unwrap(),
            }
            wait_until(BOUND, "the turn to begin", || {
                t.knobs.in_turn.load(Ordering::SeqCst)
            })
            .await;
            armed.store(true, Ordering::SeqCst);
            h.notify(Msg::Mail(Bomb::armed(&armed))).await.unwrap();
            match how {
                0 => h.stop(StopReason::Graceful).await.unwrap(),
                1 => h.stop(StopReason::Kill).await.unwrap(),
                _ => {}
            }
            t.knobs.release();
            let reason = one_reason(&t, &h, &format!("mail drop panics, {label}")).await;
            match how {
                0 => assert_eq!(reason, StopReason::Graceful, "{label}"),
                1 => assert_eq!(reason, StopReason::Kill, "{label}"),
                _ => assert_eq!(
                    reason,
                    StopReason::Failure(ActorError::user("the handler fails")),
                    "{label}"
                ),
            }
            assert_one_late_warning(&t.id(), &format!("mail drop panics, {label}"));
            // The lane is sealed: nothing is accepted any more.
            assert!(matches!(
                h.stop(StopReason::Kill).await,
                Err(SendError::Closed)
            ));
            assert!(ActorSystem::default().kill(&t.child).await.is_err());
            armed.store(false, Ordering::SeqCst);
            t.finish().await;
        }

        // An actor value whose drop panics: the value drops after the reason
        // is final, so the reason stands: `Graceful`, `Kill`, or the failing
        // handler's failure.
        for (label, how) in [("graceful", 0), ("kill", 1), ("failure", 2)] {
            let knobs = Knobs::default();
            knobs.drop_panics.store(true, Ordering::SeqCst);
            let t = tree(
                RestartType::Temporary,
                Shutdown::Timeout(BOUND),
                None,
                knobs,
            )
            .await;
            let h = t.handle();
            match how {
                0 => h.stop(StopReason::Graceful).await.unwrap(),
                1 => h.stop(StopReason::Kill).await.unwrap(),
                _ => {
                    let _ = h.send(Msg::Fail).await;
                }
            }
            let reason = one_reason(&t, &h, &format!("value drop panics, {label}")).await;
            match how {
                0 => assert_eq!(reason, StopReason::Graceful, "{label}"),
                1 => assert_eq!(reason, StopReason::Kill, "{label}"),
                _ => assert_eq!(
                    reason,
                    StopReason::Failure(ActorError::user("the handler fails")),
                    "{label}"
                ),
            }
            assert_one_late_warning(&t.id(), &format!("value drop panics, {label}"));
            t.finish().await;
        }

        // A start its own start timeout cut short: `Kill` on the plane, and
        // `Failure(StartTimeout)` in its supervisor's report.
        let knobs = Knobs::default();
        let t = tree(
            RestartType::Permanent,
            Shutdown::Timeout(BOUND),
            Some(Duration::from_millis(150)),
            knobs,
        )
        .await;
        t.knobs.hang_restarts.store(true, Ordering::SeqCst);
        let first = t.handle();
        let _ = first.notify(Msg::Fail).await;
        wait_until(BOUND, "the restart attempt to be built", || {
            t.knobs.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let attempt = t.handle();
        let plane = within(BOUND, "the attempt's end", attempt.wait_stopped()).await;
        assert_eq!(plane, StopReason::Kill, "the plane of a cut short start");
        wait_until(
            BOUND,
            "the supervisor's report of the cut short start",
            || {
                t.reports().iter().any(|event| {
                    event.reason == StopReason::Failure(ActorError::Spawn(SpawnError::StartTimeout))
                })
            },
        )
        .await;
        t.knobs.hang_restarts.store(false, Ordering::SeqCst);
        t.finish().await;
    });
}

// ---------------------------------------------------------------------------
// A final collection over an actor whose drop blocks
// ---------------------------------------------------------------------------

/// A child whose `handle` fails and whose value's `Drop` blocks its thread.
struct Blocker {
    entered: Arc<AtomicBool>,
    returned: Arc<AtomicBool>,
}

impl Actor for Blocker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("the child fails"))
    }
}

impl Drop for Blocker {
    fn drop(&mut self) {
        self.entered.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1500));
        self.returned.store(true, Ordering::SeqCst);
    }
}

/// A supervisor whose handler waits for a signal the test owns, and whose
/// child is a [`Blocker`].
struct BusySup {
    entered: Arc<AtomicBool>,
    returned: Arc<AtomicBool>,
    /// Set when the handler began to wait.
    handling: Arc<AtomicBool>,
    child: String,
}

impl Actor for BusySup {
    type Message = tokio::sync::oneshot::Receiver<()>;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (entered, returned) = (self.entered.clone(), self.returned.clone());
        ctx.spawn_child(move || Blocker {
            entered: entered.clone(),
            returned: returned.clone(),
        })
        .named(self.child.clone())
        .restart_type(RestartType::Temporary)
        .shutdown(Shutdown::Timeout(Duration::from_millis(100)))
        .await?;
        Ok(())
    }

    async fn handle(
        &mut self,
        msg: tokio::sync::oneshot::Receiver<()>,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.handling.store(true, Ordering::SeqCst);
        let _ = msg.await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// wait_stopped, and the callers of stop_within
// ---------------------------------------------------------------------------

/// A root actor on the default system.
async fn root(knobs: &Knobs) -> ActorHandle<Subject> {
    Subject::new(knobs)
        .spawn()
        .named(uname("root"))
        .await
        .expect("the actor spawns")
}

// ---------------------------------------------------------------------------
// A request that was queued when its actor stopped
// ---------------------------------------------------------------------------

/// Waits until the actor's mailbox holds `n` messages. The bound only limits how
/// long a real hang takes to report.
async fn until_queued(h: &ActorHandle<Subject>, n: usize) {
    wait_until(BOUND, "the request to be queued", || h.mailbox_len() >= n).await;
}

/// A root actor with a kill grace long enough that a stop is read at a turn
/// boundary, never cut by the abort.
async fn patient_root(knobs: &Knobs) -> ActorHandle<Subject> {
    Subject::new(knobs)
        .spawn()
        .named(uname("patient"))
        .kill_grace(Duration::from_secs(5))
        .await
        .expect("the actor spawns")
}

fn dropped_reason(result: Result<(), AskError>) -> StopReason {
    match result {
        Err(AskError::ResponseDropped { reason }) => reason,
        other => panic!("expected ResponseDropped, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// A status query that was queued when its actor stopped
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// kill_by_id, and the shutdown report
// ---------------------------------------------------------------------------

#[test]
fn kill_by_id_reports_unresponsive_for_a_never_yielding_turn() {
    // A turn that blocks its thread needs a worker thread besides the test's.
    common::run_multi_thread_only(4, async {
        let knobs = Knobs::default();
        // The turn holds its thread until the report is read.
        knobs.hold_block.hold();
        let release = knobs.release_on_drop();
        let h = root(&knobs).await;
        h.notify(Msg::Block).await.unwrap();
        wait_until(BOUND, "the turn to begin", || {
            knobs.in_turn.load(Ordering::SeqCst)
        })
        .await;
        let report = within(
            BOUND,
            "kill_by_id",
            ActorSystem::default().kill_by_id(h.id()),
        )
        .await
        .expect("the id is registered");
        assert_eq!(report.outcome, StopOutcome::Unresponsive, "{report:?}");
        assert_eq!(report.reason, None, "{report:?}");
        // The turn ends and the abort lands at the next await.
        drop(release);
        let reason = within(BOUND, "the end", h.wait_stopped()).await;
        assert_eq!(reason, StopReason::Kill);
    });
}

// ---------------------------------------------------------------------------
// A task dropped before its first poll
// ---------------------------------------------------------------------------

/// What the value of a never-polled actor sees when it is dropped.
#[derive(Default)]
struct DropView {
    /// Whether the actor's lifecycle ever ran.
    started: AtomicBool,
    /// Set when the value was dropped.
    dropped: AtomicBool,
    /// Whether the actor's status already read `Stopped` inside the drop.
    ended_at_drop: AtomicBool,
    /// Whether the actor's name was still registered inside the drop.
    registered_at_drop: AtomicBool,
    /// The actor's own handle and system, for the drop to look through.
    lookup: Mutex<Option<(ActorHandle<Unpolled>, Arc<ActorSystem>)>>,
}

struct Unpolled {
    view: Arc<DropView>,
}

impl Actor for Unpolled {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.view.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

impl Drop for Unpolled {
    fn drop(&mut self) {
        if let Some((handle, system)) = self.view.lookup.lock().unwrap().as_ref() {
            self.view.ended_at_drop.store(
                handle.status() == tokio_actors::ActorStatus::Stopped,
                Ordering::SeqCst,
            );
            self.view.registered_at_drop.store(
                system.get_by_id::<Unpolled>(handle.id()).is_some(),
                Ordering::SeqCst,
            );
        }
        self.view.dropped.store(true, Ordering::SeqCst);
    }
}

/// Spawns `count` actors on `runtime` and drops the runtime. The actors'
/// values are dropped by the runtime's shutdown, most of them before their
/// task ran once; for every one the plane must stay open until its value is
/// dropped and close only after its name is free. Returns how many never
/// started.
fn drop_the_runtime_before_the_first_poll(runtime: tokio::runtime::Runtime, count: usize) -> usize {
    let system = ActorSystem::create(uname("unpolled-system")).unwrap();
    let mut spawned = Vec::new();
    for _ in 0..count {
        let view = Arc::new(DropView::default());
        let handle = runtime.block_on(async {
            Unpolled { view: view.clone() }
                .spawn()
                .named(uname("unpolled"))
                .on_system(&system)
                .detached()
                .await
                .expect("the spawn is accepted")
        });
        *view.lookup.lock().unwrap() = Some((handle.clone(), system.clone()));
        spawned.push((view, handle));
    }
    drop(runtime);
    let mut unstarted = 0;
    for (view, handle) in &spawned {
        assert!(
            view.dropped.load(Ordering::SeqCst),
            "the runtime's shutdown drops the actor's value"
        );
        let never_polled = !view.started.load(Ordering::SeqCst);
        if never_polled {
            unstarted += 1;
        }
        assert!(
            !view.ended_at_drop.load(Ordering::SeqCst),
            "the plane read the actor's end before its value was dropped"
        );
        assert!(
            !never_polled || view.registered_at_drop.load(Ordering::SeqCst),
            "the name of a never-polled actor is still registered while its value drops"
        );
        assert_eq!(
            handle.status(),
            tokio_actors::ActorStatus::Stopped,
            "the plane ends once the runtime is gone"
        );
        assert!(
            system.get_by_id::<Unpolled>(handle.id()).is_none(),
            "the name is free when the plane ends"
        );
        view.lookup.lock().unwrap().take();
    }
    unstarted
}

/// A queued message whose drop signals that it began and then waits for
/// its release.
struct HeldMailDrop {
    began: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

impl Drop for HeldMailDrop {
    fn drop(&mut self) {
        let _ = self.began.send(());
        let _ = self.release.recv_timeout(BOUND);
    }
}

enum NeverPolledMsg {
    Request,
    Held(#[allow(dead_code)] HeldMailDrop),
}

struct NeverPolled;

impl Actor for NeverPolled {
    type Message = NeverPolledMsg;
    type Response = ();

    async fn handle(
        &mut self,
        _msg: NeverPolledMsg,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        Ok(())
    }
}

/// Releases a held drop when it goes, a failed assertion included.
struct Release(Option<std::sync::mpsc::Sender<()>>);

impl Drop for Release {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            let _ = release.send(());
        }
    }
}

// ---------------------------------------------------------------------------
// A request answered while the actor value's drop blocks
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A discarded panic in the drop of queued mail
// ---------------------------------------------------------------------------

#[cfg(feature = "tracing")]
mod warnings {
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    /// Collects every WARN event as one line of `name=value` pairs.
    #[derive(Clone, Default)]
    pub struct Warnings {
        pub lines: Arc<Mutex<Vec<String>>>,
    }

    /// Installs the one global subscriber of this test binary on first use
    /// and returns its lines; a test finds its own lines by its actor's id.
    pub fn install() -> Arc<Mutex<Vec<String>>> {
        static GLOBAL: std::sync::OnceLock<Warnings> = std::sync::OnceLock::new();
        GLOBAL
            .get_or_init(|| {
                let warnings = Warnings::default();
                tracing::subscriber::set_global_default(warnings.clone())
                    .expect("no other global subscriber is installed in this test binary");
                warnings
            })
            .lines
            .clone()
    }

    struct Text(String);

    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    impl Subscriber for Warnings {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == Level::WARN
        }

        fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut text = Text(String::new());
            event.record(&mut text);
            self.lines.lock().unwrap().push(text.0);
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }
}

/// Ends an actor with queued mail whose drop panics, after `how`: a graceful
/// stop, a `Kill` or a failing turn. The mail drops after the reason is final,
/// so the reason stands (`Graceful`, `Kill` or the turn's failure) and the
/// panic is logged once. Returns the actor's id text and its reason.
#[cfg(feature = "tracing")]
async fn end_with_a_panicking_mail_drop(how: u8) -> (String, StopReason) {
    let knobs = Knobs::held();
    let name = uname("warned");
    let h = Subject::new(&knobs)
        .spawn()
        .named(&name)
        .kill_grace(Duration::from_secs(5))
        .await
        .expect("the actor spawns");
    h.notify(if how == 2 {
        Msg::HoldThenFail
    } else {
        Msg::Hold
    })
    .await
    .unwrap();
    wait_until(BOUND, "the turn to begin", || {
        knobs.in_turn.load(Ordering::SeqCst)
    })
    .await;
    let armed = Arc::new(AtomicBool::new(true));
    h.notify(Msg::Mail(Bomb::armed(&armed))).await.unwrap();
    match how {
        0 => h.stop(StopReason::Graceful).await.unwrap(),
        1 => h.stop(StopReason::Kill).await.unwrap(),
        _ => {}
    }
    knobs.release();
    let reason = within(BOUND, "the end", h.wait_stopped()).await;
    armed.store(false, Ordering::SeqCst);
    (h.id().to_string(), reason)
}

// ---------------------------------------------------------------------------
// An aborted target whose queued mail panics in its drop
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A runtime dropped during an unwind that is not the actor's
// ---------------------------------------------------------------------------

/// An actor that answers every message.
struct Quiet;

impl Actor for Quiet {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A current-thread runtime is dropped while its thread unwinds from a panic
/// of the test's own code: the actor's task is dropped, never polled into a
/// panic, so its reason is `Kill`. Current-thread only: a multi-thread
/// runtime drops its tasks on its worker threads, which are not unwinding.
#[test]
fn a_runtime_dropped_during_an_unrelated_unwind_reads_kill() {
    let (tx, rx) = std::sync::mpsc::channel();
    let joined = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let handle = runtime.block_on(async {
            Quiet
                .spawn()
                .named(uname("quiet"))
                .await
                .expect("the actor spawns")
        });
        tx.send(handle).expect("the test waits");
        let _runtime = runtime;
        if std::hint::black_box(true) {
            panic!("a panic of the test's own code on the runtime's thread");
        }
    })
    .join();
    assert!(joined.is_err());
    let handle = rx.recv().expect("the handle");
    common::run_current_thread_only(async move {
        let reason = within(BOUND, "the actor's end", handle.wait_stopped()).await;
        assert_eq!(
            reason,
            StopReason::Kill,
            "the task was dropped, not panicked"
        );
    });
}
