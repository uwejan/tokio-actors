#![allow(dead_code)]
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
//!
//! Every step is ordered by a handshake the test observes: the test actors
//! ring one shared bell at each change of the probe state, and a hook that
//! must stay in progress waits on a gate the test opens.

use std::collections::VecDeque;
use std::future::pending;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorSystem, ChildEvent, RestartType, Shutdown,
    StopChildOutcome, StopReason, SubtreeEnd, SupervisionAction, SupervisionConfig,
};

mod common;

// ---------------------------------------------------------------------------
// Waiting helpers
// ---------------------------------------------------------------------------

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "restart-plane-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Rung at every change of the probe state a test waits on: a construction,
/// a drop, a start hook entered or finished, a gate entered or opened, a
/// recorded `ChildEvent`. It rings the calling test's own change signal
/// (`common::changes`), installed on every thread of its runtime.
fn ring() {
    common::ping();
}

/// Resolves once `cond` holds. The `Notified` is enabled before each check,
/// so a ring that lands between the check and the wait is not lost.
async fn until(mut cond: impl FnMut() -> bool) {
    let changes = common::changes();
    loop {
        let changed = changes.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if cond() {
            return;
        }
        changed.await;
    }
}

/// [`until`] with a hang detector: the bound only limits how long a real
/// hang takes to report.
async fn wait_for(what: &str, cond: impl FnMut() -> bool) {
    if tokio::time::timeout(Duration::from_secs(60), until(cond))
        .await
        .is_err()
    {
        panic!("timed out after 60 s waiting for: {what}");
    }
}

/// Waits for `sup`'s end; the bound only limits how long a real hang takes
/// to report.
async fn stopped<A: Actor>(what: &str, sup: &ActorHandle<A>) -> StopReason {
    match tokio::time::timeout(Duration::from_secs(60), sup.wait_stopped()).await {
        Ok(reason) => reason,
        Err(_) => panic!("timed out after 60 s waiting for: {what}"),
    }
}

/// Waits until `sup` and every actor below it have ended, after which every
/// name they held is free; the bound only limits how long a real hang takes
/// to report.
async fn subtree_ended<A: Actor>(sup: &ActorHandle<A>) {
    let end = tokio::time::timeout(Duration::from_secs(60), sup.observe_subtree().wait_ended())
        .await
        .expect("timed out after 60 s waiting for the supervisor's subtree to end");
    assert_eq!(
        end,
        SubtreeEnd::Ended,
        "every incarnation below the supervisor ends"
    );
}

/// Stops `sup` with `Graceful` and waits for its end: the barrier after
/// which no restart starts and no report arrives. A child the supervisor's
/// own teardown stops gets no callback, so a report recorded after it is a
/// duplicate.
async fn barrier<A: Actor>(sup: &ActorHandle<A>) {
    let _ = sup.stop(StopReason::Graceful).await;
    stopped("the supervisor to stop", sup).await;
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

/// The handle of the incarnation that holds `name`, read once that
/// incarnation's start hook has finished: it is registered and alive.
fn handle_of<A: Actor>(sys: &ActorSystem, name: &str) -> ActorHandle<A> {
    let handle = sys
        .get::<A>(name)
        .unwrap_or_else(|| panic!("'{name}' is registered"));
    assert!(handle.is_alive(), "'{name}' is alive");
    handle
}

/// A point a hook waits at until the test opens it. Once open it stays open,
/// so every later incarnation passes straight through.
#[derive(Default)]
struct Gate {
    open: AtomicBool,
    entered: AtomicUsize,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    async fn pass(&self) {
        self.entered.fetch_add(1, Ordering::SeqCst);
        ring();
        until(|| self.open.load(Ordering::SeqCst)).await;
    }

    fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }

    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        ring();
    }
}

// ---------------------------------------------------------------------------
// Instance accounting (construction in the factory, drop-decrement, so a
// Kill-torn-down instance - which skips every lifecycle callback - is still
// counted). `live` only rises at a construction, and the construction records
// the value it reached, so `max_live` is the exact peak.
// ---------------------------------------------------------------------------

/// The order of constructions and drops across several counters, as
/// `<label>+<n>` and `<label>-<n>` for the n-th construction.
type Trail = Arc<Mutex<Vec<String>>>;

#[derive(Clone, Default)]
struct InstanceCounter {
    live: Arc<AtomicUsize>,
    max_live: Arc<AtomicUsize>,
    constructions: Arc<AtomicUsize>,
    inits: Arc<AtomicUsize>,
    started: Arc<AtomicUsize>,
    trail: Option<(Trail, &'static str)>,
}

impl InstanceCounter {
    fn traced(trail: &Trail, label: &'static str) -> Self {
        Self {
            trail: Some((trail.clone(), label)),
            ..Self::default()
        }
    }

    /// Counts a construction and returns its number, from 1.
    fn on_construct(&self) -> usize {
        let nth = self.constructions.fetch_add(1, Ordering::SeqCst) + 1;
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_live.fetch_max(now, Ordering::SeqCst);
        self.note('+', nth);
        ring();
        nth
    }

    fn on_drop(&self, nth: usize) {
        self.live.fetch_sub(1, Ordering::SeqCst);
        self.note('-', nth);
        ring();
    }

    /// An incarnation entered its start hook.
    fn on_init(&self) {
        self.inits.fetch_add(1, Ordering::SeqCst);
        ring();
    }

    /// An incarnation's start hook finished.
    fn on_started(&self) {
        self.started.fetch_add(1, Ordering::SeqCst);
        ring();
    }

    fn note(&self, step: char, nth: usize) {
        if let Some((trail, label)) = &self.trail {
            trail.lock().unwrap().push(format!("{label}{step}{nth}"));
        }
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

    fn inits(&self) -> usize {
        self.inits.load(Ordering::SeqCst)
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }
}

type EventLog = Arc<Mutex<Vec<ChildEvent>>>;

fn recorder() -> EventLog {
    Arc::new(Mutex::new(Vec::new()))
}

fn record(events: &EventLog, event: &ChildEvent) {
    events.lock().unwrap().push(event.clone());
    ring();
}

fn reports_of(events: &EventLog, name: &str) -> Vec<(StopReason, SupervisionAction)> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.child_id.as_str() == name)
        .map(|event| (event.reason.clone(), event.action))
        .collect()
}

// ---------------------------------------------------------------------------
// `Flex`: a child whose every incarnation's start hook is scripted in advance
// (a per-child queue of `Init`s, one popped per construction, `Ready` once
// exhausted) - the knob every scenario below uses to control exactly which
// restart is "the slow one".
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum CMsg {
    Crash,
}

/// How one incarnation's `on_started` behaves.
#[derive(Clone)]
enum Init {
    /// Returns at once.
    Ready,
    /// Returns once the test opens the gate.
    Gate(Arc<Gate>),
    /// Never returns: only a stop order, a start timeout or an abort ends it.
    Never,
}

struct Flex {
    name: String,
    counter: InstanceCounter,
    nth: usize,
    init: Init,
    /// Passed in `pre_stop` before accepting a `Graceful`/`ParentRequest`
    /// stop: lets a test hold this incarnation's own teardown open under
    /// `Shutdown::Timeout` (irrelevant under `Shutdown::Kill`, which bypasses
    /// every callback).
    teardown: Option<Arc<Gate>>,
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
        self.counter.on_init();
        match self.init.clone() {
            Init::Ready => {}
            Init::Gate(gate) => gate.pass().await,
            Init::Never => pending::<()>().await,
        }
        self.counter.on_started();
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        if let Some(gate) = self.teardown.clone() {
            gate.pass().await;
        }
        true
    }
}

impl Drop for Flex {
    fn drop(&mut self) {
        self.counter.on_drop(self.nth);
    }
}

/// One child's full spec for [`FlexSup`]: name, instance accounting, and the
/// scripted per-construction start hooks.
struct Member {
    name: String,
    counter: InstanceCounter,
    inits: Arc<Mutex<VecDeque<Init>>>,
    shutdown: Shutdown,
    start_timeout: Option<Duration>,
    teardown: Option<Arc<Gate>>,
}

impl Member {
    fn new(name: String, counter: InstanceCounter, inits: Vec<Init>) -> Self {
        Self {
            name,
            counter,
            inits: Arc::new(Mutex::new(inits.into())),
            shutdown: Shutdown::Kill,
            start_timeout: None,
            teardown: None,
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

    fn teardown(mut self, gate: &Arc<Gate>) -> Self {
        self.teardown = Some(gate.clone());
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
            let inits = m.inits.clone();
            let teardown = m.teardown.clone();
            let mut builder = ctx
                .spawn_child(move || {
                    let init = inits.lock().unwrap().pop_front().unwrap_or(Init::Ready);
                    let nth = counter.on_construct();
                    Flex {
                        name: name.clone(),
                        counter: counter.clone(),
                        nth,
                        init,
                        teardown: teardown.clone(),
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
        record(&self.events, event);
        Ok(())
    }
}

async fn op(sup: &ActorHandle<FlexSup>, cmd: SupCmd) -> Result<StopChildOutcome, String> {
    let SupReply::Done(res) = sup.send(cmd).await.expect("supervisor must answer");
    res
}

async fn flex_sup(
    sys: &Arc<ActorSystem>,
    members: Vec<Member>,
    events: &EventLog,
    config: SupervisionConfig,
) -> ActorHandle<FlexSup> {
    FlexSup {
        members,
        events: events.clone(),
    }
    .spawn()
    .named(uname("sup"))
    .on_system(sys)
    .with_supervision(config)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// Parent killed mid-restart - no orphaned incarnation, name freed.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// No two live incarnations under a tight back-to-back crash loop.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A restart attempt in flight at the group's evaluation is stopped with the
// group and started again in its slot.
// ---------------------------------------------------------------------------

/// A one-for-all group of three members, all started, where b's second
/// construction waits at `b_init` in its start hook and c's teardown is held
/// at `c_teardown`.
struct HeldGroup {
    sys: Arc<ActorSystem>,
    sup: ActorHandle<FlexSup>,
    events: EventLog,
    a_name: String,
    b_name: String,
    c_name: String,
    a_counter: InstanceCounter,
    b_counter: InstanceCounter,
    c_counter: InstanceCounter,
    b_init: Arc<Gate>,
    c_teardown: Arc<Gate>,
}

async fn held_group() -> HeldGroup {
    let a_name = uname("a");
    let b_name = uname("b");
    let c_name = uname("c");
    let a_counter = InstanceCounter::default();
    let b_counter = InstanceCounter::default();
    let c_counter = InstanceCounter::default();
    let b_init = Gate::new();
    let c_teardown = Gate::new();

    let members = vec![
        Member::new(a_name.clone(), a_counter.clone(), Vec::new()),
        // b's SECOND construction (the manual bounce) waits in its start
        // hook, so it is still in flight when a's crash evaluates the group.
        // b's policy is the `Member` default, `Shutdown::Kill`, so the
        // group's order, written in b's turn after c ended, kills the
        // incarnation the attempt adopted at once.
        Member::new(
            b_name.clone(),
            b_counter.clone(),
            vec![Init::Ready, Init::Gate(b_init.clone())],
        ),
        // c's own teardown is held open until b's attempt has finished its
        // initialization, so the group's stop phase is still open at c when
        // b's attempt is ready: the group stops members one at a time, c
        // first, so the attempt is adopted while c is being stopped and is
        // stopped in its turn, after c, never kept.
        Member::new(c_name.clone(), c_counter.clone(), Vec::new())
            .shutdown(Shutdown::Timeout(Duration::from_secs(3600)))
            .teardown(&c_teardown),
    ];

    let events = recorder();
    let sys = new_system();
    let sup = flex_sup(
        &sys,
        members,
        &events,
        SupervisionConfig::one_for_all().max_restarts(10, Duration::from_secs(60)),
    )
    .await;

    wait_for("all three members started", || {
        a_counter.started() >= 1 && b_counter.started() >= 1 && c_counter.started() >= 1
    })
    .await;
    HeldGroup {
        sys,
        sup,
        events,
        a_name,
        b_name,
        c_name,
        a_counter,
        b_counter,
        c_counter,
        b_init,
        c_teardown,
    }
}

// ---------------------------------------------------------------------------
// A restart attempt in flight at the group's evaluation, whose own start
// timeout ends it before the group's stop window does.
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

struct PanicOnce {
    started: Arc<AtomicUsize>,
}

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

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.started.fetch_add(1, Ordering::SeqCst);
        ring();
        Ok(())
    }
}

struct FactorySup {
    name: String,
    calls: Arc<AtomicUsize>,
    started: Arc<AtomicUsize>,
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
        let started = self.started.clone();
        ctx.spawn_child(move || {
            // The SECOND call (the first restart attempt) panics; every
            // other call succeeds.
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                panic!("factory boom on restart");
            }
            PanicOnce {
                started: started.clone(),
            }
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
        record(&self.events, event);
        Ok(())
    }
}

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
/// (every restart attempt) passes `gate` in `on_started` and then fails, so
/// every restart of it is a failed attempt.
struct AlwaysFailsOnRestart {
    counter: InstanceCounter,
    attempt: usize,
    gate: Arc<Gate>,
}

impl Actor for AlwaysFailsOnRestart {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.attempt <= 1 {
            self.counter.on_started();
            return Ok(());
        }
        self.gate.pass().await;
        Err(ActorError::user("restart attempt deliberately fails"))
    }
}

impl Drop for AlwaysFailsOnRestart {
    fn drop(&mut self) {
        self.counter.on_drop(self.attempt);
    }
}

/// `b`: every incarnation starts cleanly except the one immediately after
/// its own crash (`attempt == 2`), which passes `gate` first - the test
/// holds it there to keep b's own group restart in flight while `a`'s
/// independent attempt fails.
struct SlowToRecoverAfterCrash {
    counter: InstanceCounter,
    attempt: usize,
    gate: Arc<Gate>,
}

impl Actor for SlowToRecoverAfterCrash {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("b crashed on command")
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.attempt == 2 {
            self.gate.pass().await;
        }
        self.counter.on_started();
        Ok(())
    }
}

impl Drop for SlowToRecoverAfterCrash {
    fn drop(&mut self) {
        self.counter.on_drop(self.attempt);
    }
}

/// Supervisor for the independent-restart-failure scenario: spawns `a` then
/// `b` (RestForOne start order), exposes a budget-free manual bounce for
/// either child by name, and records every `ChildEvent` it receives.
struct IndependentFailureSup {
    a_name: String,
    a_counter: InstanceCounter,
    a_gate: Arc<Gate>,
    b_name: String,
    b_counter: InstanceCounter,
    b_gate: Arc<Gate>,
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
        let a_gate = self.a_gate.clone();
        ctx.spawn_child(move || AlwaysFailsOnRestart {
            counter: a_counter.clone(),
            attempt: a_counter.on_construct(),
            gate: a_gate.clone(),
        })
        .named(self.a_name.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Kill)
        .await?;

        let b_counter = self.b_counter.clone();
        let b_gate = self.b_gate.clone();
        ctx.spawn_child(move || SlowToRecoverAfterCrash {
            counter: b_counter.clone(),
            attempt: b_counter.on_construct(),
            gate: b_gate.clone(),
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
        record(&self.events, event);
        Ok(())
    }
}

/// The children whose failed start a supervisor has logged, read from the
/// `start_error` report: the supervisor logs it in the turn that takes the
/// failed attempt from its restart plane, before it reports or retries the
/// child, so a test can wait for that turn.
#[cfg(feature = "tracing")]
mod start_errors {
    use std::fmt::Debug;
    use std::sync::{Mutex, OnceLock};

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Metadata, Subscriber};

    static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    #[derive(Default)]
    struct Fields {
        report: Option<String>,
        child: Option<String>,
    }

    impl Fields {
        fn put(&mut self, field: &Field, value: String) {
            match field.name() {
                "report" => self.report = Some(value),
                "child" => self.child = Some(value),
                _ => {}
            }
        }
    }

    impl Visit for Fields {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.put(field, value.to_string());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
            self.put(field, format!("{value:?}"));
        }
    }

    struct StartErrors;

    impl Subscriber for StartErrors {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.target() == "tokio_actors::supervisor"
        }

        fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            if let (Some("start_error"), Some(child)) = (fields.report.as_deref(), fields.child) {
                LOGGED.lock().unwrap().push(child);
                super::ring();
            }
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }

    /// Installs the process's subscriber, once.
    pub fn install() {
        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| {
            tracing::subscriber::set_global_default(StartErrors)
                .expect("no other global subscriber is installed in this process");
        });
    }

    pub fn logged(child: &str) -> bool {
        LOGGED.lock().unwrap().iter().any(|c| c == child)
    }
}

/// Case: `a`'s independent (budget-free bounce) restart attempt fails
/// while it is NOT a member of `b`'s in-flight RestForOne group restart -
/// `a` precedes `b` in start order, so `b`'s own crash restarts `b` alone.
/// The failure must still be reported through `on_child_stopped` and
/// retried once `b`'s group completes.
#[test]
fn independent_restart_failure_outside_an_in_flight_group_is_not_swallowed() {
    #[cfg(feature = "tracing")]
    start_errors::install();
    common::run(0, async {
        let a_name = uname("indep-a");
        let b_name = uname("indep-b");
        let a_counter = InstanceCounter::default();
        let b_counter = InstanceCounter::default();
        let a_gate = Gate::new();
        let b_gate = Gate::new();
        let events = recorder();

        let sys = new_system();
        let sup = IndependentFailureSup {
            a_name: a_name.clone(),
            a_counter: a_counter.clone(),
            a_gate: a_gate.clone(),
            b_name: b_name.clone(),
            b_counter: b_counter.clone(),
            b_gate: b_gate.clone(),
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
            a_counter.started() >= 1 && b_counter.started() >= 1
        })
        .await;

        // Bounce a: a budget-free independent restart whose fresh
        // incarnation waits at its gate, then fails.
        let res = sup
            .send(a_name.clone())
            .await
            .expect("supervisor must answer");
        res.expect("stop_child(a) must succeed");
        wait_for("a's restart attempt to wait in its start hook", || {
            a_gate.entered() >= 1
        })
        .await;

        // Crash b: drives a RestForOne group restart for [b] alone (a
        // precedes b in start order, so it is not a member), independent of
        // a's own in-flight restart attempt. b's restart waits at its gate,
        // so the group stays in flight.
        handle_of::<SlowToRecoverAfterCrash>(&sys, &b_name)
            .notify(())
            .await
            .expect("deliver crash to b");
        wait_for("b's group restart to wait in b's start hook", || {
            b_gate.entered() >= 1
        })
        .await;

        // a's restart attempt fails while b's group restart is in flight.
        a_gate.open();
        #[cfg(feature = "tracing")]
        wait_for(
            "the supervisor to take a's failed attempt while b's group is in flight",
            || start_errors::logged(&a_name),
        )
        .await;
        // Without the log line the nearest observable step is the end of a's
        // attempt: its incarnation has been dropped.
        #[cfg(not(feature = "tracing"))]
        wait_for("a's failed attempt to end", || a_counter.live() == 0).await;

        // b's group completes; a's failure must then be reported and
        // retried, not dropped.
        b_gate.open();
        wait_for("a's failed restart attempt to be reported", || {
            reports_of(&events, &a_name).iter().any(|(reason, action)| {
                matches!(reason, StopReason::Failure(_))
                    && *action == SupervisionAction::RestartInitiated
            })
        })
        .await;
        wait_for("a's restart to actually be retried", || {
            a_counter.constructions() >= 3
        })
        .await;
    });
}
