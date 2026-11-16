//! Behavioral suite for the OneForAll / RestForOne group-restart machinery,
//! including the interleaved-failure storm:
//!
//! - When one child crashes (panic in `handle`), the runtime stops the affected
//!   LIVE siblings with `StopReason::ParentRequest` (OneForAll: all of them;
//!   RestForOne: only those started AFTER the failed child), awaits their
//!   watcher-delivered deaths, then restarts the whole affected set in START
//!   order, each member only after the previous one finished its init.
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
//!   has acked `pre_start`/`on_started`, so consecutive members' inits never
//!   overlap (`one_for_all_restart_sequential_init`).
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
//! Idioms: per-test-unique actor names (the default `ActorSystem` registry is
//! process-global and tests run concurrently). Every change the test actors
//! make to the shared probe state (logs, counters, reports) wakes the waiting
//! test through that test's own change signal, so a test waits for exactly
//! the state it needs.
//! Hooks that must stay inside a step wait on a gate the test opens. A check
//! that nothing more happened runs after a barrier: the supervisor is stopped
//! and its end awaited, after which no restart can follow and every child
//! death still pending has been reported (as `SupervisorStopping`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorSystem, ChildEvent, RestartType, Shutdown, StopChildOutcome,
    StopReason, SupervisionAction, SupervisionConfig,
};

mod common;

// ---------------------------------------------------------------------------
// Probe signal and waiting helpers
// ---------------------------------------------------------------------------

/// Wakes every waiter of the calling test on its probe state, through the
/// test's own change signal (`common::changes`), installed on every thread
/// of its runtime.
fn ping() {
    common::ping();
}

/// Appends `entry` to an ordered log and wakes the waiting tests.
fn record(log: &Mutex<Vec<String>>, entry: String) {
    log.lock().unwrap().push(entry);
    ping();
}

/// Number of entries of `log` from `from` on that equal `entry`.
fn count_from(log: &Mutex<Vec<String>>, from: usize, entry: &str) -> usize {
    log.lock().unwrap()[from..]
        .iter()
        .filter(|e| e.as_str() == entry)
        .count()
}

/// Waits until `cond` holds, checking it after every probe change. The
/// waiter is enabled before the check, so no change between the check and
/// the wait is lost. The 60 s bound only reports a hang.
async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let changes = common::changes();
    let waited = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let notified = changes.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if cond() {
                return;
            }
            notified.await;
        }
    })
    .await;
    if waited.is_err() {
        panic!("no progress within 60 s waiting for: {what}");
    }
}

/// [`wait_until`] that fails at once, with the supervisor's reason, when the
/// supervisor ends before `cond` holds.
async fn wait_while_up<A: Actor>(sup: &ActorHandle<A>, what: &str, cond: impl FnMut() -> bool) {
    tokio::select! {
        biased;
        () = wait_until(what, cond) => {}
        reason = sup.wait_stopped() => {
            panic!("the supervisor ended ({reason:?}) before: {what}")
        }
    }
}

/// The barrier: stops the supervisor gracefully and returns the reason it
/// ended with. Afterwards no restart can start, and every child death still
/// pending was reported before the supervisor's end.
async fn stop_and_wait<A: Actor>(sup: &ActorHandle<A>) -> StopReason {
    let _ = sup.stop(StopReason::Graceful).await;
    tokio::time::timeout(Duration::from_secs(60), sup.wait_stopped())
        .await
        .expect("the supervisor ends within 60 s of its stop")
}

/// Asserts that the barrier found no child death still pending: every
/// report the scenario owed was delivered as its restart before the stop.
fn assert_no_pending_report(events: &Mutex<Vec<ChildEvent>>) {
    let evs = events.lock().unwrap();
    assert!(
        !evs.iter()
            .any(|e| e.action == SupervisionAction::SupervisorStopping),
        "a child death was still pending at the barrier, a late or duplicate report: {evs:?}"
    );
}

/// The live handle `name` resolves to. Called only once the probe state shows
/// the incarnation started: a child is registered before its start hooks run.
fn handle_of<A: Actor>(name: &str) -> ActorHandle<A> {
    ActorSystem::default()
        .get::<A>(name)
        .filter(|h| h.is_alive())
        .unwrap_or_else(|| panic!("'{name}' is registered and alive"))
}

// ---------------------------------------------------------------------------
// Instance accounting
// ---------------------------------------------------------------------------

/// Counts actor instances. Increment happens in the child FACTORY (so every
/// construction is counted, including restart attempts that later fail);
/// decrement happens in the actor's `Drop` impl (so even a `Kill`ed instance,
/// which skips all lifecycle callbacks, is still counted down).
///
/// `max_live` is updated at the increment site itself (`fetch_max` on the new
/// value): the live count only peaks at an increment, so every spike is
/// caught exactly.
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
        ping();
    }

    fn on_drop(&self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
        ping();
    }

    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    fn constructions(&self) -> usize {
        self.constructions.load(Ordering::SeqCst)
    }
}

/// Records a supervisor's report and wakes the waiting tests.
fn push_event(events: &Mutex<Vec<ChildEvent>>, event: &ChildEvent) {
    events.lock().unwrap().push(event.clone());
    ping();
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
        record(&self.log, format!("start:{}", self.name));
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        record(&self.log, format!("stop:{}", self.name));
        Ok(())
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

// ---------------------------------------------------------------------------
// OneForAll ordering: the stop phase fully precedes the restart phase, the
// live siblings stop in reverse start order and every member restarts once.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// RestForOne: only the failed child and those started AFTER it are affected;
// earlier siblings see nothing.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// One budget charge per triggering failure. Budget 2 in a 60s window survives
// two full group restarts; the third trigger exhausts it and stops the
// supervisor. Per-member charging would already die on the first group.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A vetoing child cannot stall a group restart. pre_stop returning false
// refuses ParentRequest; Shutdown::Timeout(150ms) escalates to Kill at
// expiry, and the group still completes.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Two near-simultaneous failures. The second death belongs to the group the
// first one formed: a member's end is absorbed by its group wherever its
// report waits, so the system settles after one group restart with all four
// children alive, no duplicate instances, and the supervisor healthy.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Sequential init COMPLETION: the chain constructs and starts the next member
// only after the previous member's restarted init finished. Each restarted
// init is held open until the test releases it, so a chain that went on
// without the acknowledgment would construct the next member while the
// previous init is still held.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Mid-group restart failure. A member whose OWN restart attempt fails
// (factory panic) retries the whole group; later members stay down until the
// retry succeeds, and the retry charges the budget exactly once.
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
            ping();
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
        push_event(&self.events, event);
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

        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let counter = InstanceCounter::default();
        let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let b_calls = Arc::new(AtomicUsize::new(0));

        // Budget 2 covers exactly the trigger charge and the one retry charge
        // (the threshold technique of `group_restart_single_budget_charge`):
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

        wait_while_up(&sup, "initial startup of a, b, c", || {
            counter.constructions() == 3 && counter.live() == 3 && log.lock().unwrap().len() == 3
        })
        .await;
        let baseline = log.lock().unwrap().len();

        handle_of::<GroupChild>(&b)
            .notify(ChildMsg::Crash)
            .await
            .expect("deliver crash to b");

        // c is the last member the retried chain starts.
        let start_c = format!("start:{c}");
        wait_while_up(&sup, "full recovery after the retried restart", || {
            b_calls.load(Ordering::SeqCst) == 3
                && count_from(&log, baseline, &start_c) == 1
                && counter.live() == 3
        })
        .await;

        assert!(
            sup.is_alive(),
            "budget 2 must cover exactly the trigger charge and the one retry charge; \
         a per-sibling overcharge would have exhausted it and stopped the supervisor"
        );

        // Barrier: the counts and the log read below are final.
        let reason = stop_and_wait(&sup).await;
        assert!(
            matches!(reason, StopReason::Graceful),
            "the supervisor ends only by the test's stop, never by an exhausted budget: {reason:?}"
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
            .position(|e| e == &start_c)
            .expect("c must start once the chain reaches it");
        assert!(
            start_b < start_c,
            "c's restart must wait for b's retried restart to complete: {tail:?}"
        );

        assert_no_pending_report(&events);
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
    });
}

// ---------------------------------------------------------------------------
// A hung restart init is bounded by start_timeout. The chain neither
// deadlocks nor stalls the supervisor's own message loop; the timed-out
// incarnation is killed/aborted and the failure retries until the budget
// (deliberately small here) is exhausted. The windows run on the paused
// clock, so they expire on their exact virtual instants and a round trip to
// the supervisor takes no virtual time.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// SimpleOneForOne restarts never enter the sequential group chain: each
// dynamic child restarts independently, so concurrent restarts overlap
// instead of queueing behind one another.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A `terminate_child`'d (Down, non-temporary) member rejoins a OneForAll
// restart in its own slot order, and the group as a whole still charges the
// restart budget exactly once for the triggering crash.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A member's own independent restart attempt ending WHILE a sibling group's
// `Stopping` phase is still in flight must not wedge that member's ledger
// forever. Before its own crash forms the group, b is already bouncing
// independently (`stop_child`) and its attempt hangs in its initialization.
// The group's stop phase reaches c first and is held there, in c's
// `pre_stop`, while b's attempt is still hung; once the test lets c go, the
// group orders the attempt at its turn and the attempt ends under that
// order, inside the group's stop phase. Neither may be left stuck: b in
// `Restarting`, or c queued behind it.
// ---------------------------------------------------------------------------

/// Group member whose restart attempt (but never its initial spawn) hangs in
/// its initialization, on the SECOND `on_started` call only; every later call
/// (the chain's own eventual revival) completes immediately. `calls` is shared
/// with the test so the exact call sequence (initial, failed bounce attempt,
/// successful chain revival) is verifiable.
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
        ping();
        if call == 2 {
            // The bounce-triggered restart attempt: hang in the
            // initialization, so the attempt is never adopted and only the
            // group's order ends it.
            std::future::pending::<()>().await;
        }
        record(&self.log, format!("start:{}", self.name));
        Ok(())
    }
}

impl Drop for SlowInitChild {
    fn drop(&mut self) {
        self.counter.on_drop();
    }
}

/// Group member whose `pre_stop` logs "pre_stop:<name>" and allows the stop
/// only once the test opened `gate`, so the test holds the group's
/// `Stopping` phase at this member for as long as it needs.
struct SlowStopChild {
    name: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    gate: watch::Receiver<bool>,
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
        record(&self.log, format!("start:{}", self.name));
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        record(&self.log, format!("pre_stop:{}", self.name));
        let mut gate = self.gate.clone();
        let _ = gate.wait_for(|open| *open).await;
        true
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        record(&self.log, format!("stop:{}", self.name));
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

/// Supervisor spawning a ([`GroupChild`]), b ([`SlowInitChild`]), and c
/// ([`SlowStopChild`], gated `pre_stop`): the exact shape needed to
/// interleave an independent restart failure with an in-flight sibling group
/// teardown.
struct WedgeSup {
    a: String,
    b: String,
    c: String,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    events: Arc<Mutex<Vec<ChildEvent>>>,
    b_calls: Arc<AtomicUsize>,
    c_gate: watch::Receiver<bool>,
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
        // Far beyond the scenario: the hung attempt is ended by the group's
        // order, never by this bound.
        .start_timeout(Duration::from_secs(600))
        .await?;

        let log = self.log.clone();
        let counter = self.counter.clone();
        let c_name = self.c.clone();
        let c_gate = self.c_gate.clone();
        ctx.spawn_child(move || {
            counter.on_construct();
            SlowStopChild {
                name: c_name.clone(),
                log: log.clone(),
                counter: counter.clone(),
                gate: c_gate.clone(),
            }
        })
        .named(self.c.clone())
        .restart_type(RestartType::Permanent)
        // Far beyond the scenario: c's stop ends when the test opens its gate,
        // never by this window.
        .shutdown(Shutdown::Timeout(Duration::from_secs(600)))
        .await?;

        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        push_event(&self.events, event);
        Ok(())
    }
}

/// The wedge scenario's supervisor with its members started, and the handles
/// the test reads: `c_gate_tx` is the sender of c's stop gate and must outlive
/// the test, since c's `pre_stop` returns at once when it is dropped.
struct WedgeRig {
    sup: ActorHandle<WedgeSup>,
    log: Arc<Mutex<Vec<String>>>,
    counter: InstanceCounter,
    events: Arc<Mutex<Vec<ChildEvent>>>,
    b_calls: Arc<AtomicUsize>,
    a: String,
    b: String,
    c: String,
    c_gate_tx: watch::Sender<bool>,
}

impl WedgeRig {
    /// Every member has started `n` times and three instances are live.
    fn started(&self, n: usize) -> bool {
        count_from(&self.log, 0, &format!("start:{}", self.a)) == n
            && count_from(&self.log, 0, &format!("start:{}", self.b)) == n
            && count_from(&self.log, 0, &format!("start:{}", self.c)) == n
            && self.counter.live() == 3
    }
}

/// Spawns the wedge supervisor and waits for its first start of a, b and c.
async fn wedge_rig() -> WedgeRig {
    const P: &str = "group_chain_survives_a_member_restart_failing_mid_group_stop";
    let a = format!("{P}-a");
    let b = format!("{P}-b");
    let c = format!("{P}-c");

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = InstanceCounter::default();
    let events: Arc<Mutex<Vec<ChildEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let b_calls = Arc::new(AtomicUsize::new(0));
    let (c_gate_tx, c_gate_rx) = watch::channel(false);

    // Budget 2: exactly one charge per triggering crash in the test. A
    // per-event overcharge (a superseded-attempt event being queued and
    // evaluated as an ordinary failure) would exhaust it on the very first
    // cycle and stop the supervisor before the second crash ever gets a chance
    // to prove the chain is not permanently stuck.
    let sup = WedgeSup {
        a: a.clone(),
        b: b.clone(),
        c: c.clone(),
        log: log.clone(),
        counter: counter.clone(),
        events: events.clone(),
        b_calls: b_calls.clone(),
        c_gate: c_gate_rx,
    }
    .spawn()
    .named(format!("{P}-sup"))
    .with_supervision(SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)))
    .await
    .expect("supervisor spawn");

    let rig = WedgeRig {
        sup,
        log,
        counter,
        events,
        b_calls,
        a,
        b,
        c,
        c_gate_tx,
    };
    wait_while_up(&rig.sup, "initial startup of a, b, c", || rig.started(1)).await;
    rig
}

#[test]
fn group_chain_survives_a_member_restart_failing_mid_group_stop() {
    common::run(0, async {
        let rig = wedge_rig().await;
        let WedgeRig {
            sup,
            log,
            events,
            b_calls,
            a,
            b,
            c,
            c_gate_tx,
            ..
        } = &rig;
        let start_b = format!("start:{b}");
        let started = |n: usize| rig.started(n);

        // b starts bouncing independently (budget-free): its restart attempt
        // enters `on_started` and hangs, so it is `Restarting`.
        let WedgeReply::Done(res) = sup
            .send(WedgeCmd::Bounce(b.clone()))
            .await
            .expect("supervisor must answer");
        assert_eq!(
            res.expect("stop_child(b) must succeed"),
            StopChildOutcome::Stopped
        );
        wait_while_up(sup, "b's bounce attempt to enter on_started", || {
            b_calls.load(Ordering::SeqCst) == 2
        })
        .await;
        // The attempt is registered before its start hooks run.
        let attempt = ActorSystem::default()
            .get::<SlowInitChild>(b)
            .expect("b's hung attempt is registered");

        // OneForAll forms a group that commits both c and b's attempt. Its
        // stop phase works in reverse start order, so it reaches c first,
        // where c's `pre_stop` holds it while b's attempt is still hung: the
        // exact interleaving under test. Once c is let go, the group orders
        // the attempt at its turn; the attempt ends by its window and the kill
        // that follows, its end is reported once and charges nothing, and the
        // chain starts b in its slot.
        handle_of::<GroupChild>(a)
            .notify(ChildMsg::Crash)
            .await
            .expect("deliver crash to a");
        let pre_stop_c = format!("pre_stop:{c}");
        wait_while_up(sup, "the group's stop phase reached c", || {
            count_from(log, 0, &pre_stop_c) == 1
        })
        .await;
        assert!(
            attempt.is_alive() && b_calls.load(Ordering::SeqCst) == 2,
            "b's attempt is still hung while the group's stop phase holds at c"
        );
        c_gate_tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(60), attempt.wait_stopped())
            .await
            .expect("b's attempt ends under the group's order");

        // Full first recovery. `b_calls` reaching 3 is unambiguous: it only
        // increments from inside an entered `on_started`, and the chain's
        // revival is the only possible source of a 3rd call (a wedged member
        // leaves this count at 2). Every member started a second time and is
        // a live instance again.
        wait_while_up(
            sup,
            "first recovery: b's chain-revival attempt completes",
            || b_calls.load(Ordering::SeqCst) == 3 && started(2),
        )
        .await;

        assert!(
            sup.is_alive(),
            "the crash trigger must be the only budget charge for the first cycle; a \
         superseded restart-attempt event double-charging the budget would have \
         exhausted it here"
        );
        assert_eq!(
            b_calls.load(Ordering::SeqCst),
            3,
            "b's on_started must run exactly 3 times: the initial spawn, the failed bounce \
         attempt, and the chain's own successful revival"
        );
        assert_eq!(
            count_from(log, 0, &start_b),
            2,
            "b's on_started must actually complete twice: the initial start, and the \
         chain-revived incarnation; the hung bounce attempt never reaches that line"
        );

        // No residual wedge: a completely independent SECOND crash still cycles
        // the group normally (the chain state left behind by the first cycle,
        // if any, does not block further supervision).
        handle_of::<GroupChild>(a)
            .notify(ChildMsg::Crash)
            .await
            .expect("deliver second crash to a");

        wait_while_up(
            sup,
            "second recovery: b restarts a 4th time, cleanly",
            || b_calls.load(Ordering::SeqCst) == 4 && started(3),
        )
        .await;

        // Barrier: the second crash was the second and final affordable charge,
        // so the supervisor ends only by the test's stop.
        let reason = stop_and_wait(sup).await;
        assert!(
            matches!(reason, StopReason::Graceful),
            "neither cycle over-charged the budget: {reason:?}"
        );
        assert_eq!(b_calls.load(Ordering::SeqCst), 4, "no late restart of b");
        assert_no_pending_report(events);

        // No wedge: c, queued behind b in the group, really did restart (not
        // just "is alive" from before: it was stopped and rebuilt).
        let evs = events.lock().unwrap().clone();
        assert!(
            evs.iter().any(|e| e.child_id.as_str() == c
                && matches!(e.action, SupervisionAction::RestartInitiated)),
            "c must have been reported as restarted, not left permanently queued: {evs:?}"
        );

        // b's second report is the attempt's end: killed under the group's
        // order, reported as the restart it is part of.
        let b_reports: Vec<_> = evs.iter().filter(|e| e.child_id.as_str() == b).collect();
        assert!(
            b_reports.len() >= 2
                && matches!(&b_reports[1].reason, StopReason::Kill)
                && b_reports[1].action == SupervisionAction::RestartInitiated,
            "the attempt's end is reported once as a kill under the order: {b_reports:?}"
        );
    });
}
