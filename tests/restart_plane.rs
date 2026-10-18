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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorSystem, ChildEvent, RestartType, Shutdown,
    SpawnError, StopChildOutcome, StopReason, SupervisionAction, SupervisionConfig,
    SupervisionError,
};

mod common;

// ---------------------------------------------------------------------------
// Timing / waiting helpers
// ---------------------------------------------------------------------------

const DEADLINE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "restart-plane-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

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

async fn wait_until<F: FnMut() -> bool>(timeout_ms: u64, mut pred: F) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
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

fn alive_as<A: Actor>(sys: &ActorSystem, name: &str) -> bool {
    sys.get::<A>(name).map(|h| h.is_alive()).unwrap_or(false)
}

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

    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    fn max_live(&self) -> usize {
        self.max_live.load(Ordering::SeqCst)
    }

    fn constructions(&self) -> usize {
        self.constructions.load(Ordering::SeqCst)
    }
}

/// Background sampler catching a transient two-live spike the increment-site
/// `fetch_max` alone might land between polls of.
fn spawn_max_sampler(counter: &InstanceCounter) -> tokio::task::JoinHandle<()> {
    let live = counter.live.clone();
    let max_live = counter.max_live.clone();
    tokio::spawn(async move {
        loop {
            max_live.fetch_max(live.load(Ordering::SeqCst), Ordering::SeqCst);
            sleep(Duration::from_millis(1)).await;
        }
    })
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

#[derive(Clone)]
enum CMsg {
    Crash,
}

struct Flex {
    name: String,
    counter: InstanceCounter,
    delay: Duration,
    /// Held in `pre_stop` before accepting a `Graceful`/`ParentRequest` stop:
    /// lets a test control exactly how long this incarnation's own teardown
    /// takes under `Shutdown::Timeout` (irrelevant under `Shutdown::Kill`,
    /// which bypasses every callback).
    teardown_delay: Duration,
}

impl Actor for Flex {
    type Message = CMsg;
    type Response = ();

    async fn handle(&mut self, msg: CMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            CMsg::Crash => panic!("{} crashed on command", self.name),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if !self.delay.is_zero() {
            sleep(self.delay).await;
        }
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        if !self.teardown_delay.is_zero() {
            sleep(self.teardown_delay).await;
        }
        true
    }
}

impl Drop for Flex {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

/// One child's full spec for [`FlexSup`]: name, instance accounting, and the
/// scripted per-construction `on_started` delay queue.
struct Member {
    name: String,
    counter: InstanceCounter,
    delays: Arc<Mutex<VecDeque<Duration>>>,
    shutdown: Shutdown,
    start_timeout: Option<Duration>,
    teardown_delay: Duration,
}

impl Member {
    fn new(name: String, counter: InstanceCounter, delays: Vec<Duration>) -> Self {
        Self {
            name,
            counter,
            delays: Arc::new(Mutex::new(delays.into())),
            shutdown: Shutdown::Kill,
            start_timeout: None,
            teardown_delay: Duration::ZERO,
        }
    }

    fn shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }

    fn start_timeout(mut self, dur: Duration) -> Self {
        self.start_timeout = Some(dur);
        self
    }

    fn teardown_delay(mut self, dur: Duration) -> Self {
        self.teardown_delay = dur;
        self
    }
}

/// Supervisor spawning `members` (Vec order = start order) in `on_started`,
/// exposing manual child-management commands, and recording every
/// `ChildEvent` it receives.
struct FlexSup {
    members: Vec<Member>,
    events: EventLog,
}

#[derive(Clone)]
enum SupCmd {
    StopChild(String),
    TerminateChild(String),
}

#[derive(Debug)]
enum SupReply {
    Done(Result<StopChildOutcome, String>),
}

impl Actor for FlexSup {
    type Message = SupCmd;
    type Response = SupReply;

    async fn handle(&mut self, msg: SupCmd, ctx: &mut ActorContext<Self>) -> ActorResult<SupReply> {
        let res = match msg {
            SupCmd::StopChild(name) => ctx.stop_child(name).await.map_err(|e| e.to_string()),
            SupCmd::TerminateChild(name) => ctx
                .terminate_child(name)
                .await
                .map(|()| StopChildOutcome::Stopped)
                .map_err(|e| e.to_string()),
        };
        Ok(SupReply::Done(res))
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for m in &self.members {
            let name = m.name.clone();
            let counter = m.counter.clone();
            let delays = m.delays.clone();
            let teardown_delay = m.teardown_delay;
            let mut builder = ctx
                .spawn_child(move || {
                    let delay = delays.lock().unwrap().pop_front().unwrap_or(Duration::ZERO);
                    counter.on_construct();
                    Flex {
                        name: name.clone(),
                        counter: counter.clone(),
                        delay,
                        teardown_delay,
                    }
                })
                .named(m.name.clone())
                .restart_type(RestartType::Permanent)
                .shutdown(m.shutdown);
            if let Some(dur) = m.start_timeout {
                builder = builder.start_timeout(dur);
            }
            builder.await?;
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

async fn op(sup: &ActorHandle<FlexSup>, cmd: SupCmd) -> Result<StopChildOutcome, String> {
    let SupReply::Done(res) = sup.send(cmd).await.expect("supervisor must answer");
    res
}

// ---------------------------------------------------------------------------
// Item 5: parent killed mid-restart - no orphaned incarnation, name freed.
// ---------------------------------------------------------------------------

/// Arm: a hung init with `start_timeout: None` - the guard, not the ack, is
/// the only backstop. Killing the parent must still free the name.
#[test]
fn parent_killed_mid_restart_hung_init_start_timeout_none_frees_the_name() {
    common::run(0, async {
        let name = uname("hung-init");
        let counter = InstanceCounter::default();
        let member = Member::new(
            name.clone(),
            counter.clone(),
            vec![Duration::ZERO, Duration::from_secs(3600)],
        )
        .shutdown(Shutdown::Timeout(Duration::from_millis(100)));

        let sys = new_system();
        let sup = FlexSup {
            members: vec![member],
            events: recorder(),
        }
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .with_supervision(SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(60)))
        .await
        .unwrap();

        let h = live_handle::<Flex>(&sys, &name).await;
        h.notify(CMsg::Crash).await.unwrap();

        // The crashed incarnation's restart spawned a fresh one whose
        // `on_started` will never return - wait for the restart to actually be
        // in flight before pulling the rug.
        wait_for("the restart attempt to begin", || {
            counter.constructions() >= 2
        })
        .await;

        sup.stop(StopReason::Kill).await.unwrap();

        assert!(
            wait_until(5_000, || sys.get::<Flex>(&name).is_none()).await,
            "the hung incarnation's name must free once the guard's reaper backstop aborts it"
        );
        assert!(
            wait_until(5_000, || counter.live() == 0).await,
            "no incarnation may survive the parent's death"
        );
    });
}

/// Arm: a bounded `start_timeout` that has not yet elapsed when the parent
/// dies - the guard still kills the fresh incarnation immediately, well
/// before its own timeout would have fired.
#[test]
fn parent_killed_mid_restart_before_ack_frees_the_name() {
    common::run(0, async {
        let name = uname("mid-init");
        let counter = InstanceCounter::default();
        let member = Member::new(
            name.clone(),
            counter.clone(),
            vec![Duration::ZERO, Duration::from_secs(2)],
        )
        .shutdown(Shutdown::Timeout(Duration::from_millis(100)))
        .start_timeout(Duration::from_secs(5));

        let sys = new_system();
        let sup = FlexSup {
            members: vec![member],
            events: recorder(),
        }
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .with_supervision(SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(60)))
        .await
        .unwrap();

        let h = live_handle::<Flex>(&sys, &name).await;
        h.notify(CMsg::Crash).await.unwrap();

        wait_for("the restart attempt to begin", || {
            counter.constructions() >= 2
        })
        .await;

        // Kill the parent well inside the 2s on_started sleep and well before the
        // 5s start_timeout: only the guard can be responsible for what happens
        // next.
        sup.stop(StopReason::Kill).await.unwrap();

        assert!(
            wait_until(5_000, || sys.get::<Flex>(&name).is_none()).await,
            "the mid-init incarnation's name must free"
        );
        assert!(
            wait_until(5_000, || counter.live() == 0).await,
            "no incarnation may survive the parent's death"
        );
    });
}

/// Arm (stress): the parent is killed the instant the crash is notified, with
/// no synchronization between the two - across enough iterations this lands
/// on both the pre-spawn window (the restart task never gets a chance to run
/// at all) and the post-init/pre-adoption window (the attempt's `Adopted`
/// value is sitting in the restart plane, unconsumed) without the test ever
/// needing to force either specifically.
#[test]
fn parent_killed_racing_the_crash_never_leaks_an_incarnation() {
    common::run(0, async {
        const ITERATIONS: usize = 15;
        for _ in 0..ITERATIONS {
            let name = uname("race");
            let counter = InstanceCounter::default();
            let member = Member::new(name.clone(), counter.clone(), Vec::new());
            let sys = new_system();

            let sup = FlexSup {
                members: vec![member],
                events: recorder(),
            }
            .spawn()
            .named(uname("sup"))
            .on_system(&sys)
            .with_supervision(
                SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(60)),
            )
            .await
            .unwrap();

            let h = live_handle::<Flex>(&sys, &name).await;
            // No await between these two: whatever the scheduler does with the
            // restart task it triggers is exactly the race under test.
            let _ = h.notify(CMsg::Crash).await;
            let _ = sup.stop(StopReason::Kill).await;

            assert!(
                wait_until(5_000, || sys.get::<Flex>(&name).is_none()).await,
                "the child's name must free regardless of how the crash/kill race landed"
            );
            assert!(
                wait_until(5_000, || counter.live() == 0).await,
                "no incarnation may leak regardless of how the crash/kill race landed"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// No two live incarnations under a tight back-to-back crash loop.
// ---------------------------------------------------------------------------

#[test]
fn no_two_live_incarnations_under_rapid_consecutive_crashes() {
    common::run(0, async {
        let name = uname("rapid");
        let counter = InstanceCounter::default();
        let member = Member::new(name.clone(), counter.clone(), Vec::new());
        let sys = new_system();

        let sup = FlexSup {
            members: vec![member],
            events: recorder(),
        }
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .with_supervision(
            SupervisionConfig::one_for_one().max_restarts(50, Duration::from_secs(60)),
        )
        .await
        .unwrap();

        let sampler = spawn_max_sampler(&counter);
        wait_for("initial instance constructed", || {
            counter.constructions() >= 1
        })
        .await;

        const N: usize = 20;
        for i in 0..N {
            let h = live_handle::<Flex>(&sys, &name).await;
            h.notify(CMsg::Crash).await.unwrap();
            wait_for("the restart to land", || counter.constructions() >= i + 2).await;
        }
        sampler.abort();

        assert_eq!(
            counter.max_live(),
            1,
            "two incarnations of the same child must never coexist"
        );
        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &name)).await);
        let _ = sup.stop(StopReason::Graceful).await;
    });
}

// ---------------------------------------------------------------------------
// Item 3: a restart attempt in flight at the group's evaluation is stopped
// with the group and started again in its slot.
// ---------------------------------------------------------------------------

#[test]
fn in_flight_attempt_at_group_evaluation_is_stopped_and_restarted_in_its_slot() {
    common::run(0, async {
        let a_name = uname("a");
        let b_name = uname("b");
        let c_name = uname("c");
        let a_counter = InstanceCounter::default();
        let b_counter = InstanceCounter::default();
        let c_counter = InstanceCounter::default();

        let members = vec![
            Member::new(a_name.clone(), a_counter.clone(), Vec::new()),
            // b's SECOND construction (the manual bounce below) is deliberately
            // slow, so it is still in flight when a's crash evaluates the
            // group. b's policy is the `Member` default, `Shutdown::Kill`, so
            // the group's order, written in b's turn after c ended, kills the
            // incarnation the attempt adopted at once.
            Member::new(
                b_name.clone(),
                b_counter.clone(),
                vec![Duration::ZERO, Duration::from_millis(400)],
            ),
            // c's own teardown takes 700 ms, longer than b's attempt needs to
            // finish its initialization, so the group's stop phase is still
            // open at c when b's attempt is ready: the group stops members one
            // at a time, c first, so the attempt is adopted while c is being
            // stopped and is stopped in its turn, after c, never kept.
            Member::new(c_name.clone(), c_counter.clone(), Vec::new())
                .shutdown(Shutdown::Timeout(Duration::from_secs(5)))
                .teardown_delay(Duration::from_millis(700)),
        ];

        let events = recorder();
        let sys = new_system();
        let sup = FlexSup {
            members,
            events: events.clone(),
        }
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .with_supervision(
            SupervisionConfig::one_for_all().max_restarts(10, Duration::from_secs(60)),
        )
        .await
        .unwrap();

        wait_for("all three members started", || {
            a_counter.constructions() >= 1
                && b_counter.constructions() >= 1
                && c_counter.constructions() >= 1
        })
        .await;

        // Bounce b: a budget-free restart that returns once b's ORIGINAL
        // incarnation is confirmed gone, well before its slow replacement acks.
        // `initiate` only SCHEDULES the fresh incarnation's task onto the restart
        // plane - construction happens once the runtime actually polls it, an
        // instant that can trail this call's own return - so the second
        // construction is polled for rather than asserted immediately.
        op(&sup, SupCmd::StopChild(b_name.clone()))
            .await
            .expect("stop_child must succeed");
        wait_for("b's second incarnation to be under construction", || {
            b_counter.constructions() >= 2
        })
        .await;

        // Crash a: OneForAll evaluates the group while b's attempt is still in
        // flight - the group commits the attempt, and stops it in its turn.
        let events_before_the_crash = events.lock().unwrap().len();
        let ha = live_handle::<Flex>(&sys, &a_name).await;
        ha.notify(CMsg::Crash).await.expect("deliver crash to a");

        // The whole cycle completes: a and c restart once (group), b is built
        // three times: its first incarnation, the bounce's attempt (stopped by
        // the group, never kept) and the chain's own start in b's slot.
        wait_for("the whole group cycle to settle", || {
            a_counter.constructions() >= 2
                && b_counter.constructions() >= 3
                && c_counter.constructions() >= 2
        })
        .await;

        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &a_name)).await);
        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &b_name)).await);
        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &c_name)).await);

        // Settling window: no further, unexpected restarts.
        sleep(Duration::from_millis(300)).await;
        assert_eq!(a_counter.constructions(), 2);
        assert_eq!(b_counter.constructions(), 3);
        assert_eq!(c_counter.constructions(), 2);
        assert_eq!(
            b_counter.max_live(),
            1,
            "the attempt's incarnation is gone before the chain starts b again"
        );

        // After the crash, c ends once, then b's adopted incarnation ends once:
        // the group stops c, which was started last, before it reaches b, and
        // each end is delivered once as a restart.
        let later: Vec<(String, SupervisionAction)> = events.lock().unwrap()
            [events_before_the_crash..]
            .iter()
            .map(|event| (event.child_id.as_str().to_string(), event.action))
            .collect();
        let at = |name: &str| -> Vec<usize> {
            later
                .iter()
                .enumerate()
                .filter(|(_, (id, _))| id == name)
                .map(|(i, _)| i)
                .collect()
        };
        assert_eq!(at(&c_name).len(), 1, "c is reported once: {later:?}");
        assert_eq!(at(&b_name).len(), 1, "b is reported once: {later:?}");
        assert!(
            later
                .iter()
                .filter(|(id, _)| *id == c_name || *id == b_name)
                .all(|(_, action)| *action == SupervisionAction::RestartInitiated),
            "{later:?}"
        );
        assert!(
            at(&c_name)[0] < at(&b_name)[0],
            "c ends before the group reaches b: {later:?}"
        );

        // b's reports: the bounce's stop and the attempt the group stopped,
        // each delivered once as a restart; the chain's own start of b needs
        // no report.
        let b_events: Vec<SupervisionAction> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.child_id.as_str() == b_name)
            .map(|event| event.action)
            .collect();
        assert_eq!(
            b_events,
            [
                SupervisionAction::RestartInitiated,
                SupervisionAction::RestartInitiated
            ],
            "the stopped attempt is reported once: {b_events:?}"
        );

        let _ = sup.stop(StopReason::Graceful).await;
    });
}

// ---------------------------------------------------------------------------
// Item 4: a restart attempt in flight at the group's evaluation, whose own
// start timeout ends it before the group's stop window does.
// ---------------------------------------------------------------------------

#[test]
fn in_flight_attempt_with_a_hung_init_is_bounded_by_its_start_timeout_and_restarted_in_its_slot() {
    common::run(0, async {
        let a_name = uname("a");
        let b_name = uname("b");
        let c_name = uname("c");
        let a_counter = InstanceCounter::default();
        let b_counter = InstanceCounter::default();
        let c_counter = InstanceCounter::default();

        const B_START_TIMEOUT: Duration = Duration::from_millis(300);

        let members = vec![
            Member::new(a_name.clone(), a_counter.clone(), Vec::new()),
            // b's bounce attempt sleeps far longer than its own start_timeout,
            // so it is guaranteed to time out; its THIRD construction (the
            // chain's own start) is fast and succeeds. b's stop window is
            // five seconds, longer than its start timeout, so the start
            // timeout ends the attempt first.
            Member::new(
                b_name.clone(),
                b_counter.clone(),
                vec![Duration::ZERO, Duration::from_secs(2), Duration::ZERO],
            )
            .shutdown(Shutdown::Timeout(Duration::from_secs(5)))
            .start_timeout(B_START_TIMEOUT),
            Member::new(c_name.clone(), c_counter.clone(), Vec::new()),
        ];

        let sys = new_system();
        let events = recorder();
        let sup = FlexSup {
            members,
            events: events.clone(),
        }
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .with_supervision(
            SupervisionConfig::one_for_all().max_restarts(10, Duration::from_secs(60)),
        )
        .await
        .unwrap();

        wait_for("all three members started", || {
            a_counter.constructions() >= 1
                && b_counter.constructions() >= 1
                && c_counter.constructions() >= 1
        })
        .await;

        op(&sup, SupCmd::StopChild(b_name.clone()))
            .await
            .expect("stop_child must succeed");
        wait_for("b's second incarnation to be under construction", || {
            b_counter.constructions() >= 2
        })
        .await;

        let group_triggered_at = Instant::now();
        let ha = live_handle::<Flex>(&sys, &a_name).await;
        ha.notify(CMsg::Crash).await.expect("deliver crash to a");

        // The chain starts a, then b only once b's attempt ended by its own
        // start_timeout, so c is never touched before that.
        wait_for("b's start in its slot to land", || {
            b_counter.constructions() >= 3
        })
        .await;
        wait_for("the chain to reach and restart c", || {
            c_counter.constructions() >= 2
        })
        .await;

        let elapsed = group_triggered_at.elapsed();
        assert!(
            elapsed >= B_START_TIMEOUT.mul_f32(0.8),
            "the chain must not have reached c before b's own start_timeout could \
         plausibly have escalated (elapsed: {elapsed:?})"
        );

        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &a_name)).await);
        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &b_name)).await);
        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &c_name)).await);

        sleep(Duration::from_millis(300)).await;
        assert_eq!(a_counter.constructions(), 2);
        assert_eq!(b_counter.constructions(), 3);
        assert_eq!(c_counter.constructions(), 2);

        // The attempt ended by its start timeout, and was reported once as
        // the failed start it is; the chain's own start needs no report.
        let b_ends: Vec<(StopReason, SupervisionAction)> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.child_id.as_str() == b_name)
            .map(|event| (event.reason.clone(), event.action))
            .collect();
        assert_eq!(b_ends.len(), 2, "the bounce and the attempt: {b_ends:?}");
        assert!(
            matches!(
                &b_ends[1],
                (
                    StopReason::Failure(ActorError::Spawn(SpawnError::StartTimeout)),
                    SupervisionAction::RestartInitiated
                )
            ),
            "{b_ends:?}"
        );

        let _ = sup.stop(StopReason::Graceful).await;
    });
}

// ---------------------------------------------------------------------------
// A restart attempt in flight at the group's evaluation under the `Kill`
// policy: the group's order, written in the member's turn, ends it at once,
// before its start timeout.
// ---------------------------------------------------------------------------

#[test]
fn in_flight_attempt_under_kill_policy_is_killed_at_the_group_order() {
    common::run(0, async {
        let a_name = uname("a");
        let b_name = uname("b");
        let c_name = uname("c");
        let a_counter = InstanceCounter::default();
        let b_counter = InstanceCounter::default();
        let c_counter = InstanceCounter::default();

        const B_START_TIMEOUT: Duration = Duration::from_millis(300);

        let members = vec![
            Member::new(a_name.clone(), a_counter.clone(), Vec::new()),
            // b's bounce attempt sleeps far longer than its own start_timeout;
            // its THIRD construction (the chain's own start) is fast and
            // succeeds. b's policy is the `Member` default, `Shutdown::Kill`:
            // the group's order, written in b's turn, raises Kill on the
            // attempt's incarnation at once and the attempt aborts it one kill
            // grace later, so the start timeout is only the later bound.
            Member::new(
                b_name.clone(),
                b_counter.clone(),
                vec![Duration::ZERO, Duration::from_secs(2), Duration::ZERO],
            )
            .start_timeout(B_START_TIMEOUT),
            Member::new(c_name.clone(), c_counter.clone(), Vec::new()),
        ];

        let sys = new_system();
        let events = recorder();
        let sup = FlexSup {
            members,
            events: events.clone(),
        }
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .with_supervision(
            SupervisionConfig::one_for_all().max_restarts(10, Duration::from_secs(60)),
        )
        .await
        .unwrap();

        wait_for("all three members started", || {
            a_counter.constructions() >= 1
                && b_counter.constructions() >= 1
                && c_counter.constructions() >= 1
        })
        .await;

        op(&sup, SupCmd::StopChild(b_name.clone()))
            .await
            .expect("stop_child must succeed");
        wait_for("b's second incarnation to be under construction", || {
            b_counter.constructions() >= 2
        })
        .await;

        let ha = live_handle::<Flex>(&sys, &a_name).await;
        ha.notify(CMsg::Crash).await.expect("deliver crash to a");

        // The chain starts a, then b once the group's order has killed b's
        // attempt, and c after b.
        wait_for("b's start in its slot to land", || {
            b_counter.constructions() >= 3
        })
        .await;
        wait_for("the chain to reach and restart c", || {
            c_counter.constructions() >= 2
        })
        .await;

        let b_reports = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.child_id.as_str() == b_name)
            .count();
        assert_eq!(b_reports, 2, "the bounce and the attempt are both reported");

        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &a_name)).await);
        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &b_name)).await);
        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &c_name)).await);

        sleep(Duration::from_millis(300)).await;
        assert_eq!(a_counter.constructions(), 2);
        assert_eq!(b_counter.constructions(), 3);
        assert_eq!(c_counter.constructions(), 2);

        // The attempt was killed by the group's order, before its start
        // timeout could end it: its end is a `Kill`, not a failed start.
        let b_ends: Vec<(StopReason, SupervisionAction)> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.child_id.as_str() == b_name)
            .map(|event| (event.reason.clone(), event.action))
            .collect();
        assert!(
            matches!(
                &b_ends[1],
                (StopReason::Kill, SupervisionAction::RestartInitiated)
            ),
            "{b_ends:?}"
        );

        let _ = sup.stop(StopReason::Graceful).await;
    });
}

// ---------------------------------------------------------------------------
// Down-rejoin + single budget charge per triggering event.
// ---------------------------------------------------------------------------

#[test]
fn one_for_all_down_member_rejoins_chain_budget_charged_once() {
    common::run(0, async {
        let a_name = uname("a");
        let b_name = uname("b");
        let c_name = uname("c");
        let a_counter = InstanceCounter::default();
        let b_counter = InstanceCounter::default();
        let c_counter = InstanceCounter::default();

        let members = vec![
            Member::new(a_name.clone(), a_counter.clone(), Vec::new()),
            Member::new(b_name.clone(), b_counter.clone(), Vec::new()),
            Member::new(c_name.clone(), c_counter.clone(), Vec::new()),
        ];

        // Tight budget: exactly two triggering failures are affordable. If the
        // Down-rejoin of b (or any of the group's own internal restarts)
        // incorrectly charged the budget again, the second crash below would
        // already be rejected.
        let sys = new_system();
        let sup = FlexSup {
            members,
            events: recorder(),
        }
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .with_supervision(SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)))
        .await
        .unwrap();

        wait_for("all three members started", || {
            a_counter.constructions() >= 1
                && b_counter.constructions() >= 1
                && c_counter.constructions() >= 1
        })
        .await;

        // b is terminated (Down, non-temporary, spec kept) well before any group
        // restart is ever triggered.
        op(&sup, SupCmd::TerminateChild(b_name.clone()))
            .await
            .expect("terminate_child must succeed");
        assert!(!alive_as::<Flex>(&sys, &b_name));

        // Trigger 1 (charge 1 of 2): a crashes; OneForAll sweeps the group,
        // reviving b in its own slot order alongside a and c.
        let ha = live_handle::<Flex>(&sys, &a_name).await;
        ha.notify(CMsg::Crash).await.unwrap();

        wait_for("the group cycle to revive everyone, b included", || {
            a_counter.constructions() >= 2
                && b_counter.constructions() >= 2
                && c_counter.constructions() >= 2
        })
        .await;
        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &a_name)).await);
        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &b_name)).await);
        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &c_name)).await);

        // Trigger 2 (charge 2 of 2): still affordable.
        let ha = live_handle::<Flex>(&sys, &a_name).await;
        ha.notify(CMsg::Crash).await.unwrap();
        wait_for("the second group cycle to complete", || {
            a_counter.constructions() >= 3
        })
        .await;
        assert!(wait_until(5_000, || alive_as::<Flex>(&sys, &a_name)).await);
        assert!(
            sup.is_alive(),
            "budget must still allow exactly two charges"
        );

        // Trigger 3: exceeds the budget - the supervisor exits.
        let ha = live_handle::<Flex>(&sys, &a_name).await;
        ha.notify(CMsg::Crash).await.unwrap();
        assert!(
            wait_until(5_000, || !sup.is_alive()).await,
            "a third triggering failure must exhaust the two-charge budget"
        );
    });
}

// ---------------------------------------------------------------------------
// Factory panic inside the restart task: typed failure, budget charged.
// ---------------------------------------------------------------------------

struct PanicOnce;

#[derive(Clone)]
enum PMsg {
    Crash,
}

impl Actor for PanicOnce {
    type Message = PMsg;
    type Response = ();

    async fn handle(&mut self, msg: PMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            PMsg::Crash => panic!("crashed on command"),
        }
    }
}

struct FactorySup {
    name: String,
    calls: Arc<AtomicUsize>,
    events: EventLog,
}

impl Actor for FactorySup {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let calls = self.calls.clone();
        ctx.spawn_child(move || {
            // The SECOND call (the first restart attempt) panics; every
            // other call succeeds.
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                panic!("factory boom on restart");
            }
            PanicOnce
        })
        .named(self.name.clone())
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

#[test]
fn factory_panic_in_restart_task_surfaces_typed_and_charges_budget() {
    common::run(0, async {
        let name = uname("factory-panic");
        let calls = Arc::new(AtomicUsize::new(0));
        let events = recorder();

        let sys = new_system();
        let sup = FactorySup {
            name: name.clone(),
            calls: calls.clone(),
            events: events.clone(),
        }
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .with_supervision(SupervisionConfig::one_for_one().max_restarts(2, Duration::from_secs(60)))
        .await
        .unwrap();

        let h = live_handle::<PanicOnce>(&sys, &name).await;
        h.notify(PMsg::Crash).await.unwrap();

        // The factory panic (charge 2 of 2) is reported as its own typed event,
        // and the child recovers on the retry (call #3).
        let deadline = Instant::now() + DEADLINE;
        loop {
            let found = events.lock().unwrap().iter().any(|e| {
                e.child_id.as_str() == name
                    && matches!(
                        &e.reason,
                        StopReason::Failure(ActorError::Supervision(
                            SupervisionError::FactoryFailed(_)
                        ))
                    )
                    && e.action == SupervisionAction::RestartInitiated
            });
            if found {
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for a FactoryFailed event: {:?}",
                    events.lock().unwrap()
                );
            }
            sleep(POLL).await;
        }

        assert!(
            wait_until(5_000, || alive_as::<PanicOnce>(&sys, &name)).await,
            "the child must recover once the factory succeeds again"
        );
        assert!(
            sup.is_alive(),
            "exactly two charges (the crash, then the factory-panic retry) must fit the budget"
        );

        // A third triggering failure exceeds the two-charge budget.
        let h = live_handle::<PanicOnce>(&sys, &name).await;
        h.notify(PMsg::Crash).await.unwrap();
        assert!(
        wait_until(5_000, || !sup.is_alive()).await,
        "a third triggering failure must exhaust the budget the factory panic already spent one of"
    );
    });
}

// ---------------------------------------------------------------------------
// A start_timeout expiry on the restart path surfaces through the ordinary
// `on_child_stopped` event, carrying the typed `SpawnError::StartTimeout`.
// ---------------------------------------------------------------------------

#[test]
fn restart_start_timeout_surfaces_as_typed_spawn_error() {
    common::run(0, async {
        let name = uname("typed-timeout");
        let counter = InstanceCounter::default();
        let member = Member::new(
            name.clone(),
            counter.clone(),
            vec![Duration::ZERO, Duration::from_secs(3600)],
        )
        .start_timeout(Duration::from_millis(150));

        let events = recorder();
        let sys = new_system();
        let sup = FlexSup {
            members: vec![member],
            events: events.clone(),
        }
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .with_supervision(SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(60)))
        .await
        .unwrap();

        let h = live_handle::<Flex>(&sys, &name).await;
        h.notify(CMsg::Crash).await.unwrap();

        let is_start_timeout = |e: &ChildEvent| {
            e.child_id.as_str() == name
                && matches!(
                    &e.reason,
                    StopReason::Failure(ActorError::Spawn(SpawnError::StartTimeout))
                )
        };
        let evs = {
            let deadline = Instant::now() + DEADLINE;
            loop {
                let snapshot = events.lock().unwrap().clone();
                if snapshot.iter().any(is_start_timeout) {
                    break snapshot;
                }
                if Instant::now() >= deadline {
                    panic!("timed out waiting for a start_timeout event: {snapshot:?}");
                }
                sleep(POLL).await;
            }
        };
        assert!(
            evs.iter().any(is_start_timeout),
            "the hung restart must be reported as a start_timeout failure: {evs:?}"
        );

        let _ = sup.stop(StopReason::Graceful).await;
    });
}

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
/// every other wait helper in this file.
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

/// Control: the identical bounce-then-fail sequence for `a`, but `b` never
/// crashes, so no group is ever pending - `a`'s failure is evaluated the
/// instant it is reported, exactly as it always has been.
#[test]
fn independent_restart_failure_with_no_group_pending_behaves_the_same() {
    common::run(0, async {
        let a_name = uname("g3-a-control");
        let b_name = uname("g3-b-control");
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

        let res = sup
            .send(a_name.clone())
            .await
            .expect("supervisor must answer");
        res.expect("stop_child(a) must succeed");
        wait_for("a's restart attempt to begin", || {
            a_counter.constructions() >= 2
        })
        .await;

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
