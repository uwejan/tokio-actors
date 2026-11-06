//! Behavioral suite for the OneForAll / RestForOne group-restart machinery,
//! including the interleaved-failure storm:
//!
//! - When one child crashes (panic in `handle`), the runtime stops the affected
//!   LIVE siblings with `StopReason::ParentRequest` (OneForAll: all of them;
//!   RestForOne: only those started AFTER the failed child), awaits their
//!   watcher-delivered deaths, then restarts the whole affected set: each
//!   member's restart is initiated in START order, the next only after the
//!   previous has re-registered; start-callback completion may interleave.
//!   Children outside the affected set are untouched.
//! - ONE budget charge per triggering failure; group-member deaths during the
//!   group do NOT charge (`group_restart_single_budget_charge`).
//! - `Shutdown::Timeout(d)` escalates to Kill at expiry, so a `pre_stop` veto
//!   cannot stall a group forever (`vetoing_child_killed_by_timeout_escalation`).
//! - No duplicate live instances during restart storms (incarnation guard),
//!   proven with factory-increment / Drop-decrement instance accounting. The
//!   decrement lives in `Drop`, NOT `on_stopped`, because the Kill escalation
//!   path bypasses all lifecycle callbacks while `Drop` always runs.
//! - Restart INIT completion is sequential within a group: the next member's
//!   restart is initiated only after the previous member's fresh incarnation
//!   has acked `pre_start`/`on_started`, so consecutive members' init windows
//!   never overlap (`one_for_all_restart_sequential_init`).
//! - A member whose restart attempt itself fails (factory panic, or a
//!   start_timeout expiry) retries the group: under OneForAll every child is
//!   stopped and started again from the first, under RestForOne the failed
//!   member and every child after it. Later members stay down until the retry
//!   succeeds, and the retry charges the budget exactly once, never once per
//!   member (`one_for_all_mid_group_failure_retries`,
//!   `one_for_all_hung_init_escalates`).
//! - A hung `on_started` during a restart is bounded by the child's
//!   `start_timeout`; the supervisor keeps answering ordinary messages the
//!   whole time (`one_for_all_hung_init_escalates`).
//!
//! Idioms: multi_thread runtime, per-test-unique actor names (the default
//! `ActorSystem` registry is process-global and tests run concurrently), and
//! event-based waiting (bounded polling) instead of long fixed sleeps.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::sleep;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorSystem, ChildEvent, RestartType, Shutdown, StopChildOutcome,
    StopReason, SupervisionAction, SupervisionConfig,
};

mod common;

// ---------------------------------------------------------------------------
// Timing constants: deadlines are generous, polling is tight.
// ---------------------------------------------------------------------------

/// The longest a poll for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const DEADLINE: Duration = Duration::from_secs(60);
/// Poll interval for event-based waiting.
const POLL: Duration = Duration::from_millis(10);
/// Settling window after a wait condition first holds; used to detect
/// late/duplicate events (double-restart storms) before asserting exact counts.
const QUIET: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// Instance accounting
// ---------------------------------------------------------------------------

/// Counts actor instances. Increment happens in the child FACTORY (so every
/// construction is counted, including restart attempts that later fail);
/// decrement happens in the actor's `Drop` impl (so even a `Kill`ed instance,
/// which skips all lifecycle callbacks, is still counted down).
///
/// `max_live` is updated at the increment site itself (`fetch_max` on the new
/// value), which catches every transient spike exactly; the background sampler
/// task (see `spawn_max_sampler`) is belt-and-braces on top of that.
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

    fn constructions(&self) -> usize {
        self.constructions.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Event-based waiting helpers
// ---------------------------------------------------------------------------

/// Polls `cond` every `POLL` until it holds or `DEADLINE` expires (panic). The bound only limits how long a real hang takes to report.
async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if cond() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out after {DEADLINE:?} waiting for: {what}");
        }
        sleep(POLL).await;
    }
}

/// True when `name` resolves in the default system to a live handle of type `A`.
fn alive_as<A: Actor>(name: &str) -> bool {
    ActorSystem::default()
        .get::<A>(name)
        .map(|h| h.is_alive())
        .unwrap_or(false)
}

/// True when every name in `names` is registered and alive as an `A`.
fn all_alive<A: Actor>(names: &[String]) -> bool {
    names.iter().all(|n| alive_as::<A>(n))
}

/// Polls the default-system registry until `name` resolves to a live handle,
/// then returns that handle. Handles are invalidated by restarts, so tests
/// re-lookup before every interaction instead of caching. The bound only limits how long a real hang takes to report.
async fn live_handle<A: Actor>(name: &str) -> ActorHandle<A> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if let Some(h) = ActorSystem::default().get::<A>(name) {
            if h.is_alive() {
                return h;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out after {DEADLINE:?} waiting for live actor '{name}'");
        }
        sleep(POLL).await;
    }
}

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum ChildMsg {
    /// Panic inside `handle` (the crash under test).
    Crash,
}

/// Group member: logs "start:<name>" in `on_started` and "stop:<name>" in
/// `on_stopped`, decrements the live counter in `Drop`, and panics on command.
struct GroupChild {
    name: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
}

impl Actor for GroupChild {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Crash => panic!("{} crashed on command", self.name),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.log
            .lock()
            .unwrap()
            .push(format!("start:{}", self.name));
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.log.lock().unwrap().push(format!("stop:{}", self.name));
        Ok(())
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

// ---------------------------------------------------------------------------
// Test 9: OneForAll ordering - stop phase fully precedes restart phase, and
// restarts fire in START order (a, b, c).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 10: RestForOne - only the failed child and those started AFTER it are
// affected; earlier siblings see nothing.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 11: one budget charge per triggering failure. Budget 2 in a 60s window
// survives two full group restarts; the third trigger exhausts it and stops
// the supervisor. Per-member charging would already die on the first group.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 12: a vetoing child cannot stall a group restart. pre_stop returning
// false refuses ParentRequest; Shutdown::Timeout(150ms) escalates to Kill at
// expiry, and the group still completes.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Plus: two near-simultaneous failures. The second trigger either folds into
// the pending group or queues behind it; either way the system
// settles with all four children alive, no duplicate instances, and the
// supervisor healthy. Assertions stay coarse (liveness + counts).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 13: sequential init COMPLETION. Restart INITIATION was already known
// sequential; this proves the ack is actually awaited, so consecutive
// members' `on_started` windows never overlap.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 14: mid-group restart failure. A member whose OWN restart attempt
// fails (factory panic) retries the whole group; later members stay down
// until the retry succeeds, and the retry charges the budget exactly once.
// ---------------------------------------------------------------------------

/// Supervisor spawning a, b, c as [`GroupChild`]s, where b's factory panics
/// on its first restart attempt (the second call overall) and succeeds on
/// every other call.
struct FlakyGroupSup {
    a: String,
    b: String,
    c: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    events: Arc<Mutex<Vec<ChildEvent>>>,
    b_calls: Arc<AtomicUsize>,
}

impl Actor for FlakyGroupSup {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let log = self.log.clone();
        let counter = self.counter.clone();
        let a_name = self.a.clone();
        ctx.spawn_child(move || {
            counter.on_construct();
            GroupChild {
                name: a_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
            }
        })
        .named(self.a.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
        .await?;

        let log = self.log.clone();
        let counter = self.counter.clone();
        let b_name = self.b.clone();
        let b_calls = self.b_calls.clone();
        ctx.spawn_child(move || {
            let call = b_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 2 {
                panic!("{b_name} factory failed on its first restart attempt");
            }
            counter.on_construct();
            GroupChild {
                name: b_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
            }
        })
        .named(self.b.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
        .await?;

        let log = self.log.clone();
        let counter = self.counter.clone();
        let c_name = self.c.clone();
        ctx.spawn_child(move || {
            counter.on_construct();
            GroupChild {
                name: c_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
            }
        })
        .named(self.c.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
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
fn one_for_all_mid_group_failure_retries() {
    common::run(0, async {
        const P: &str = "one_for_all_mid_group_failure_retries";
        let a = format!("{P}-a");
        let b = format!("{P}-b");
        let c = format!("{P}-c");
        let names = vec![a.clone(), b.clone(), c.clone()];

        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let counter = InstanceCounter::default();
        let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let b_calls = Arc::new(AtomicUsize::new(0));

        // Budget 2 covers exactly the trigger charge and the one retry charge
        // (the "threshold technique" from `group_restart_single_budget_charge`):
        // a per-sibling overcharge would exceed it and stop the supervisor.
        let sup = FlakyGroupSup {
            a: a.clone(),
            b: b.clone(),
            c: c.clone(),
            log: log.clone(),
            counter: counter.clone(),
            events: events.clone(),
            b_calls: b_calls.clone(),
        }
        .spawn()
        .named(format!("{P}-sup"))
        .with_supervision(SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)))
        .await
        .expect("supervisor spawn");

        wait_for("initial startup of a, b, c", || {
            counter.constructions() == 3
                && counter.live() == 3
                && log.lock().unwrap().len() == 3
                && all_alive::<GroupChild>(&names)
        })
        .await;
        let baseline = log.lock().unwrap().len();

        let hb = live_handle::<GroupChild>(&b).await;
        hb.notify(ChildMsg::Crash)
            .await
            .expect("deliver crash to b");

        wait_for("full recovery after the retried restart", || {
            b_calls.load(Ordering::SeqCst) == 3
                && counter.constructions() == 7
                && counter.live() == 3
                && all_alive::<GroupChild>(&names)
                && sup.is_alive()
        })
        .await;
        sleep(QUIET).await;

        assert!(
            sup.is_alive(),
            "budget 2 must cover exactly the trigger charge and the one retry charge; \
         a per-sibling overcharge would have exhausted it and stopped the supervisor"
        );

        assert_eq!(
            b_calls.load(Ordering::SeqCst),
            3,
            "b's factory must be called 3 times: initial construction, the failed first \
         restart attempt, and the successful retry"
        );
        assert_eq!(
            counter.constructions(),
            7,
            "the failed factory attempt never constructs an instance; the 3 initial ones \
         count, plus a twice (the retry restarts the whole group, so a is stopped and \
         started again), b once and c once"
        );

        let tail: Vec<String> = log.lock().unwrap()[baseline..].to_vec();
        let starts: Vec<&str> = tail
            .iter()
            .filter_map(|e| e.strip_prefix("start:"))
            .collect();
        assert_eq!(
            starts,
            [a.as_str(), a.as_str(), b.as_str(), c.as_str()],
            "after the crash a starts, b's attempt fails before it starts, the retry stops \
         a and starts a, b and c in start order: {tail:?}"
        );
        let start_b = tail
            .iter()
            .position(|e| e == &format!("start:{b}"))
            .expect("b must eventually start via its retry");
        let start_c = tail
            .iter()
            .position(|e| e == &format!("start:{c}"))
            .expect("c must start once the chain reaches it");
        assert!(
            start_b < start_c,
            "c's restart must wait for b's retried restart to complete: {tail:?}"
        );

        let b_events = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.child_id.as_str() == b)
            .count();
        assert!(
            b_events >= 2,
            "the supervisor must observe both b's failed attempt and its retry as \
         separate ChildStopped events"
        );

        let _ = sup.stop(StopReason::Graceful).await;
    });
}

// ---------------------------------------------------------------------------
// Test 15: a hung restart init is bounded by start_timeout. The chain neither
// deadlocks nor stalls the supervisor's own message loop; the timed-out
// incarnation is killed/aborted and the failure retries until the budget
// (deliberately small here) is exhausted.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 16: SimpleOneForOne restarts never enter the sequential group chain -
// each dynamic child restarts independently, so concurrent restarts overlap
// instead of queueing behind one another.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 17: a `terminate_child`'d (Down, non-temporary) member rejoins a
// OneForAll restart in its own slot order, and the group as a whole still
// charges the restart budget exactly once for the triggering crash.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 18: a member's own independent restart attempt ending WHILE a
// sibling group's `Stopping` phase is still in flight must not wedge that
// member's ledger forever. Before its own crash forms the group, b is
// already bouncing independently (`stop_child`) and its attempt hangs in its
// initialization; the group orders the attempt, which ends strictly during
// the group's teardown of the slow sibling c. Neither may be left stuck: b
// in `Restarting`, or c queued behind it.
// ---------------------------------------------------------------------------

/// Group member whose restart attempt (but never its initial spawn) hangs in
/// its initialization, on the SECOND factory call only - every later call
/// (the chain's own eventual revival) completes immediately. `calls` is shared with the test so the exact call sequence
/// (initial, failed bounce attempt, successful chain revival) is verifiable.
struct SlowInitChild {
    name: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    calls: Arc<AtomicUsize>,
}

impl Actor for SlowInitChild {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Crash => panic!("{} crashed on command", self.name),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == 2 {
            // The bounce-triggered restart attempt: hang in the
            // initialization, so the attempt is never adopted and only the
            // group's order (or a start timeout) ends it.
            std::future::pending::<()>().await;
        }
        self.log
            .lock()
            .unwrap()
            .push(format!("start:{}", self.name));
        Ok(())
    }
}

impl Drop for SlowInitChild {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

/// Group member whose `pre_stop` holds the stop gate open for `stop_delay`
/// before allowing it - long enough to outlast a sibling's `start_timeout` -
/// so its own death is what the group's `Stopping` phase is still awaiting
/// when that sibling's independent restart attempt fails.
struct SlowStopChild {
    name: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    stop_delay: Duration,
}

impl Actor for SlowStopChild {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Crash => panic!("{} crashed on command", self.name),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.log
            .lock()
            .unwrap()
            .push(format!("start:{}", self.name));
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        sleep(self.stop_delay).await;
        true
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.log.lock().unwrap().push(format!("stop:{}", self.name));
        Ok(())
    }
}

impl Drop for SlowStopChild {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

#[derive(Clone)]
enum WedgeCmd {
    /// `stop_child` (budget-free bounce) issued from inside the supervisor's
    /// own handler, exactly like every other manual-stop API.
    Bounce(String),
}

#[derive(Debug)]
enum WedgeReply {
    Done(Result<StopChildOutcome, String>),
}

/// Supervisor spawning a ([`GroupChild`]), b ([`SlowInitChild`], short
/// `start_timeout`), and c ([`SlowStopChild`], slow `pre_stop`) - the exact
/// shape needed to interleave an independent restart failure with an
/// in-flight sibling group teardown.
struct WedgeSup {
    a: String,
    b: String,
    c: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    events: Arc<Mutex<Vec<ChildEvent>>>,
    b_calls: Arc<AtomicUsize>,
    c_stop_delay: Duration,
}

impl Actor for WedgeSup {
    type Message = WedgeCmd;
    type Response = WedgeReply;

    async fn handle(
        &mut self,
        msg: WedgeCmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<WedgeReply> {
        let WedgeCmd::Bounce(name) = msg;
        let res = ctx.stop_child(name).await.map_err(|e| e.to_string());
        Ok(WedgeReply::Done(res))
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let log = self.log.clone();
        let counter = self.counter.clone();
        let a_name = self.a.clone();
        ctx.spawn_child(move || {
            counter.on_construct();
            GroupChild {
                name: a_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
            }
        })
        .named(self.a.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(300)))
        .await?;

        let log = self.log.clone();
        let counter = self.counter.clone();
        let b_name = self.b.clone();
        let b_calls = self.b_calls.clone();
        ctx.spawn_child(move || {
            counter.on_construct();
            SlowInitChild {
                name: b_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
                calls: b_calls.clone(),
            }
        })
        .named(self.b.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_millis(300)))
        // Longer than anything the test waits for: the hung attempt is ended by
        // the group's order, never by this bound.
        .start_timeout(Duration::from_secs(5))
        .await?;

        let log = self.log.clone();
        let counter = self.counter.clone();
        let c_name = self.c.clone();
        let c_stop_delay = self.c_stop_delay;
        ctx.spawn_child(move || {
            counter.on_construct();
            SlowStopChild {
                name: c_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
                stop_delay: c_stop_delay,
            }
        })
        .named(self.c.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_secs(2)))
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
fn group_chain_survives_a_member_restart_failing_mid_group_stop() {
    common::run(0, async {
        const P: &str = "group_chain_survives_a_member_restart_failing_mid_group_stop";
        let a = format!("{P}-a");
        let b = format!("{P}-b");
        let c = format!("{P}-c");
        let c_stop_delay = Duration::from_millis(500);

        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let counter = InstanceCounter::default();
        let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let b_calls = Arc::new(AtomicUsize::new(0));

        // Budget 2: exactly one charge per triggering crash below - a per-event
        // overcharge (a superseded-attempt event being queued and
        // evaluated as an ordinary failure) would exhaust it on the very first
        // cycle and stop the supervisor before the second crash ever gets a
        // chance to prove the chain is not permanently stuck.
        let sup = WedgeSup {
            a: a.clone(),
            b: b.clone(),
            c: c.clone(),
            log: log.clone(),
            counter: counter.clone(),
            events: events.clone(),
            b_calls: b_calls.clone(),
            c_stop_delay,
        }
        .spawn()
        .named(format!("{P}-sup"))
        .with_supervision(SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)))
        .await
        .expect("supervisor spawn");

        wait_for("initial startup of a, b, c", || {
            counter.constructions() == 3 && counter.live() == 3
        })
        .await;

        // b starts bouncing independently (budget-free): its restart attempt
        // enters `on_started` and hangs, so it is `Restarting` and will fail via
        // `start_timeout` shortly.
        let WedgeReply::Done(res) = sup
            .send(WedgeCmd::Bounce(b.clone()))
            .await
            .expect("supervisor must answer");
        assert_eq!(
            res.expect("stop_child(b) must succeed"),
            StopChildOutcome::Stopped
        );
        // b's bounce attempt is inside `on_started`, the call that hangs.
        wait_for("b's bounce attempt to enter on_started", || {
            b_calls.load(Ordering::SeqCst) >= 2
        })
        .await;

        // b's attempt hangs in its initialization, and its start timeout is far
        // away, so it is in flight whenever a crashes: OneForAll forms a group
        // that commits both c and b's attempt, and orders the attempt to stop.
        // The attempt ends by its window and the kill that follows, being
        // shorter than c's stop. c's `pre_stop` holds the group's `Stopping`
        // phase open for `c_stop_delay`, longer than b's window - the exact
        // interleaving under test. The attempt's end is reported once and
        // charges nothing, and the chain starts b in its slot.
        let ha = live_handle::<GroupChild>(&a).await;
        ha.notify(ChildMsg::Crash)
            .await
            .expect("deliver crash to a");

        // Full first recovery. `is_alive()` only reflects "task not yet
        // finished" - the hung (failing) bounce attempt itself reports alive the
        // whole time it is stuck in `on_started`, so it cannot distinguish real
        // recovery from the still-failing middle of the sequence. `b_calls`
        // reaching 3 is unambiguous: it only increments from inside a
        // successfully-entered `on_started`, and the chain-revival attempt is
        // the only possible source of a 3rd call (a wedged member leaves this
        // count at 2, and the `wait_for` deadline then panics).
        // `counter.live() == 3` confirms
        // every member (including c, and the hung bounce instance's replacement)
        // is back to a genuinely live instance, not just constructed.
        wait_for(
            "first recovery: b's chain-revival attempt completes",
            || b_calls.load(Ordering::SeqCst) >= 3 && counter.live() == 3,
        )
        .await;
        sleep(QUIET).await;

        assert!(
            sup.is_alive(),
            "the crash trigger must be the only budget charge for the first cycle - a \
         superseded restart-attempt event double-charging the budget would have \
         exhausted it here"
        );

        assert_eq!(
            b_calls.load(Ordering::SeqCst),
            3,
            "b's factory must run exactly 3 times: the initial spawn, the failed bounce \
         attempt, and the chain's own successful revival"
        );

        let starts_b = log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e == &&format!("start:{b}"))
            .count();
        assert_eq!(
            starts_b, 2,
            "b's on_started must actually complete twice: the initial start, and the \
         chain-revived incarnation - the hung bounce attempt never reaches this line"
        );

        // No wedge: c, queued behind b in the group, really did restart (not
        // just "is alive" from before - it was stopped and rebuilt).
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.child_id.as_str() == c
                    && matches!(e.action, SupervisionAction::RestartInitiated)),
            "c must have been reported as restarted, not left permanently queued: {:?}",
            events.lock().unwrap()
        );

        // b's second report is the attempt's end: killed under the group's
        // order, reported as the restart it is part of.
        let b_reports: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.child_id.as_str() == b)
            .cloned()
            .collect();
        assert!(
            b_reports.len() >= 2
                && matches!(&b_reports[1].reason, StopReason::Kill)
                && b_reports[1].action == SupervisionAction::RestartInitiated,
            "the attempt's end is reported once as a kill under the order: {b_reports:?}"
        );

        // No residual wedge: a completely independent SECOND crash still cycles
        // the group normally (the chain state left behind by the first cycle,
        // if any, does not block further supervision).
        let ha2 = live_handle::<GroupChild>(&a).await;
        ha2.notify(ChildMsg::Crash)
            .await
            .expect("deliver second crash to a");

        wait_for("second recovery: b restarts a 4th time, cleanly", || {
            b_calls.load(Ordering::SeqCst) >= 4 && counter.live() == 3
        })
        .await;
        sleep(QUIET).await;

        assert!(
            sup.is_alive(),
            "the second crash must be exactly the second and final affordable budget charge, \
         proving neither cycle over-charged"
        );

        let _ = sup.stop(StopReason::Graceful).await;
    });
}
