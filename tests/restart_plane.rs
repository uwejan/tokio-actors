//! Behavioral suite for the restart plane: restarts run in a parent-owned
//! `JoinSet<RestartOutcome>`, and the outcome value itself - not a message -
//! carries either an adoptable fresh incarnation (with its own armed link
//! guard) or a typed failure.
//!
//! - Dropping the supervisor's restart plane mid-restart (a `Kill` teardown)
//!   kills the in-flight incarnation through the ordinary guard ladder, for
//!   every stage a restart can be at: a hung init bounded only by the guard
//!   (`start_timeout: None`), a bounded-but-not-yet-elapsed init wait, and the
//!   window around the crash/kill race itself.
//! - No two live incarnations of the same child ever coexist, even under a
//!   tight back-to-back crash loop.
//! - A restart attempt in flight when its group is evaluated (a manual
//!   bounce) is stopped with the group, when the group's stop reaches the
//!   member: its actor is stopped under its own policy and never kept, its
//!   end is delivered once, and the chain starts the member again in its
//!   slot.
//! - What ends such an attempt depends on the member's stop policy. Under
//!   `Shutdown::Kill` the group's order kills the attempt's incarnation at
//!   once and the attempt aborts it one kill grace later, long before its start
//!   timeout. Under a `Timeout` window longer than the start timeout the start
//!   timeout ends the attempt first and its end is reported as a failed start.
//!   Either way the end is reported once as part of the restart, and the chain
//!   then starts the member again in its slot.
//! - A `terminate_child`'d (`Down`) member rejoins a group restart in slot
//!   order, and the group as a whole still charges the budget exactly once
//!   per triggering failure.
//! - A factory panic inside the restart task surfaces as a typed failure and
//!   charges the budget like any other crash.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorSystem, ChildEvent, RestartType, Shutdown,
    StopChildOutcome, StopReason, SupervisionAction, SupervisionConfig,
};

mod common;

// ---------------------------------------------------------------------------
// Timing / waiting helpers
// ---------------------------------------------------------------------------

/// The longest a poll for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const DEADLINE: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(10);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "restart-plane-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Polls `cond` until it holds. The bound only limits how long a real hang takes to report.
async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if cond() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out after {DEADLINE:?} waiting for: {what}");
        }
        sleep(POLL).await;
    }
}

/// A fresh, dedicated [`ActorSystem`] per test - never the process-wide
/// `ActorSystem::default()`. Every actor and its reaper task then live and
/// die on exactly this one test's own runtime, with no risk of a concurrently
/// running test's `#[tokio::test]` runtime tearing down a reaper the shared
/// default system's `OnceLock` happened to bind to first (the reaper handle
/// is `get_or_init`'d against whichever caller's runtime asks for it first;
/// sharing one system across many independently-lived test runtimes would
/// make that binding's lifetime an accident of test scheduling order).
fn new_system() -> Arc<ActorSystem> {
    ActorSystem::create(uname("sys")).expect("system name must be unique")
}

/// Polls until `name` resolves in `sys` to a live handle. The bound only limits how long a real hang takes to report.
async fn live_handle<A: Actor>(sys: &ActorSystem, name: &str) -> ActorHandle<A> {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(h) = sys.get::<A>(name) {
            if h.is_alive() {
                return h;
            }
        }
        if Instant::now() >= deadline {
            panic!("timed out after {DEADLINE:?} waiting for live actor '{name}'");
        }
        sleep(POLL).await;
    }
}

// ---------------------------------------------------------------------------
// Instance accounting (construction in the factory, drop-decrement, so a
// Kill-torn-down instance - which skips every lifecycle callback - is still
// counted).
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct InstanceCounter {
    live: Arc<AtomicUsize>,
    max_live: Arc<AtomicUsize>,
    constructions: Arc<AtomicUsize>,
}

impl InstanceCounter {
    fn on_construct(&self) {
        self.constructions.fetch_add(1, Ordering::SeqCst);
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_live.fetch_max(now, Ordering::SeqCst);
    }

    fn on_drop(&self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }

    fn constructions(&self) -> usize {
        self.constructions.load(Ordering::SeqCst)
    }
}

type EventLog = Arc<Mutex<Vec<ChildEvent>>>;

fn recorder() -> EventLog {
    Arc::new(Mutex::new(Vec::new()))
}

// ---------------------------------------------------------------------------
// `Flex`: a child whose every incarnation's `on_started` delay is scripted in
// advance (a per-child queue of `Duration`s, one popped per construction,
// defaulting to zero once exhausted) - the knob every scenario below uses to
// control exactly which restart is "the slow one".
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Item 5: parent killed mid-restart - no orphaned incarnation, name freed.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// No two live incarnations under a tight back-to-back crash loop.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Item 3: a restart attempt in flight at the group's evaluation is stopped
// with the group and started again in its slot.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Item 4: a restart attempt in flight at the group's evaluation, whose own
// start timeout ends it before the group's stop window does.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A restart attempt in flight at the group's evaluation under the `Kill`
// policy: the group's order, written in the member's turn, ends it at once,
// before its start timeout.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Down-rejoin + single budget charge per triggering event.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Factory panic inside the restart task: typed failure, budget charged.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A start_timeout expiry on the restart path surfaces through the ordinary
// `on_child_stopped` event, carrying the typed `SpawnError::StartTimeout`.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// An independent restart attempt that fails while it is not a member of an
// in-flight group restart must still be reported and retried once that
// group completes, not silently dropped.
// ---------------------------------------------------------------------------

/// `a`: its first incarnation starts cleanly; every incarnation after that
/// (every restart attempt) sleeps `fail_after` in `on_started` and then
/// fails, so every restart of it is a failed attempt.
struct AlwaysFailsOnRestart {
    counter: InstanceCounter,
    attempt: usize,
    fail_after: Duration,
}

impl Actor for AlwaysFailsOnRestart {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.attempt <= 1 {
            return Ok(());
        }
        sleep(self.fail_after).await;
        Err(ActorError::user("restart attempt deliberately fails"))
    }
}

impl Drop for AlwaysFailsOnRestart {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

/// `b`: every incarnation starts cleanly except the one immediately after
/// its own crash (`attempt == 2`), which sleeps `slow_after_crash` first -
/// long enough to keep its own group restart in flight while `a`'s
/// independent attempt fails.
struct SlowToRecoverAfterCrash {
    counter: InstanceCounter,
    attempt: usize,
    slow_after_crash: Duration,
}

impl Actor for SlowToRecoverAfterCrash {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("b crashed on command")
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.attempt == 2 {
            sleep(self.slow_after_crash).await;
        }
        Ok(())
    }
}

impl Drop for SlowToRecoverAfterCrash {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

/// Supervisor for the independent-restart-failure scenario: spawns `a` then
/// `b` (RestForOne start order), exposes a budget-free manual bounce for
/// either child by name, and records every `ChildEvent` it receives.
struct IndependentFailureSup {
    a_name: String,
    a_counter: InstanceCounter,
    b_name: String,
    b_counter: InstanceCounter,
    events: EventLog,
}

impl Actor for IndependentFailureSup {
    type Message = String;
    type Response = Result<StopChildOutcome, String>;

    async fn handle(
        &mut self,
        name: String,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Result<StopChildOutcome, String>> {
        Ok(ctx.stop_child(name).await.map_err(|e| e.to_string()))
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let a_counter = self.a_counter.clone();
        ctx.spawn_child(move || {
            a_counter.on_construct();
            AlwaysFailsOnRestart {
                counter: a_counter.clone(),
                attempt: a_counter.constructions(),
                fail_after: Duration::from_millis(400),
            }
        })
        .named(self.a_name.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Kill)
        .await?;

        let b_counter = self.b_counter.clone();
        ctx.spawn_child(move || {
            b_counter.on_construct();
            SlowToRecoverAfterCrash {
                counter: b_counter.clone(),
                attempt: b_counter.constructions(),
                slow_after_crash: Duration::from_millis(500),
            }
        })
        .named(self.b_name.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Kill)
        .await?;

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

/// Waits for an event matching `pred` to appear in `events`, polling like
/// every other wait helper in this file. The bound only limits how long a real hang takes to report.
async fn wait_for_event(events: &EventLog, what: &str, pred: impl Fn(&ChildEvent) -> bool) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if events.lock().unwrap().iter().any(&pred) {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out after {DEADLINE:?} waiting for: {what}; events so far: {:?}",
                events.lock().unwrap()
            );
        }
        sleep(POLL).await;
    }
}

/// Case: `a`'s independent (budget-free bounce) restart attempt fails
/// while it is NOT a member of `b`'s in-flight RestForOne group restart -
/// `a` precedes `b` in start order, so `b`'s own crash restarts `b` alone.
/// The failure must still be reported through `on_child_stopped` and
/// retried once `b`'s group completes.
#[test]
fn independent_restart_failure_outside_an_in_flight_group_is_not_swallowed() {
    common::run(0, async {
        let a_name = uname("g3-a");
        let b_name = uname("g3-b");
        let a_counter = InstanceCounter::default();
        let b_counter = InstanceCounter::default();
        let events = recorder();

        let sys = new_system();
        let sup = IndependentFailureSup {
            a_name: a_name.clone(),
            a_counter: a_counter.clone(),
            b_name: b_name.clone(),
            b_counter: b_counter.clone(),
            events: events.clone(),
        }
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .with_supervision(
            SupervisionConfig::rest_for_one().max_restarts(20, Duration::from_secs(60)),
        )
        .await
        .unwrap();

        wait_for("both members started", || {
            a_counter.constructions() >= 1 && b_counter.constructions() >= 1
        })
        .await;

        // Bounce a: a budget-free independent restart whose fresh
        // incarnation will fail 400ms after it starts.
        let res = sup
            .send(a_name.clone())
            .await
            .expect("supervisor must answer");
        res.expect("stop_child(a) must succeed");
        wait_for("a's restart attempt to begin", || {
            a_counter.constructions() >= 2
        })
        .await;

        // Crash b: drives a RestForOne group restart for [b] alone (a
        // precedes b in start order, so it is not a member), independent of
        // a's own in-flight restart attempt.
        let hb = live_handle::<SlowToRecoverAfterCrash>(&sys, &b_name).await;
        hb.notify(()).await.expect("deliver crash to b");

        // a's restart attempt fails (at ~400ms) while b's group restart is
        // still in flight (b takes ~500ms): its failure must still be
        // reported and retried once the group completes, not dropped.
        wait_for_event(&events, "a's failed restart attempt to be reported", |e| {
            e.child_id.as_str() == a_name
                && matches!(&e.reason, StopReason::Failure(_))
                && e.action == SupervisionAction::RestartInitiated
        })
        .await;

        wait_for("a's restart to actually be retried", || {
            a_counter.constructions() >= 3
        })
        .await;
    });
}
