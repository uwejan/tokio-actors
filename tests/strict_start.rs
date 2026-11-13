#![allow(dead_code)]
//! Strict start and `await_init`: both are opt in, and the default start is
//! unchanged.
//!
//! A supervisor with `strict_start` answers its own spawn only after every
//! child it spawned while starting has finished its initialization, and the
//! first child that fails fails that spawn. `spawn_child(..).await_init()`
//! returns after one child's initialization and, when it fails, keeps no
//! spec, no callback and no budget charge.
//!
//! Every wait is ordered by a gate or a handshake the test controls, never by
//! a sleep, and every wait is bounded so that a hang fails the test.

mod common;

use std::future::{poll_fn, Future, IntoFuture};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use tokio::sync::{mpsc, Barrier, Notify, Semaphore};
use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorId, ActorResult, ActorSystem, ChildEvent, RestartType, Shutdown,
    SpawnError, StopReason, SupervisionAction, SupervisionConfig, SupervisorRef,
};

/// Bounds every wait, so a hang fails the test.
const BOUND: Duration = Duration::from_secs(20);
/// A start bound short enough to pass in a test and long enough to be a bound.
const SHORT: Duration = Duration::from_millis(150);

type Log = Arc<Mutex<Vec<String>>>;

fn note(log: &Log, line: impl Into<String>) {
    log.lock().unwrap().push(line.into());
}

fn lines(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

fn unique(base: &str) -> String {
    format!("{base}-{}", uuid::Uuid::new_v4())
}

/// How a child's initialization goes wrong, if it does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fails {
    Never,
    PreStartErr,
    OnStartedErr,
    PreStartPanic,
}

/// What one child does while it starts and when it is stopped.
#[derive(Clone)]
struct Plan {
    fails: Fails,
    /// `pre_start` waits here for a permit before it goes on.
    gate: Option<Arc<Semaphore>>,
    /// `pre_start` waits here for the other party of the barrier.
    meet: Option<Arc<Barrier>>,
    /// `pre_start` never returns.
    hang: bool,
    /// `pre_stop` refuses every stop.
    veto: bool,
    /// `pre_start` calls the boss through this slot before it goes on.
    calls: Option<Arc<Mutex<Option<ActorHandle<Boss>>>>>,
    /// The call is a `SupervisorRef` read in place of a plain `send`.
    via_ref: bool,
}

impl Plan {
    fn ok() -> Self {
        Plan {
            fails: Fails::Never,
            gate: None,
            meet: None,
            hang: false,
            veto: false,
            calls: None,
            via_ref: false,
        }
    }

    fn failing(fails: Fails) -> Self {
        Plan {
            fails,
            ..Plan::ok()
        }
    }

    fn gated(gate: &Arc<Semaphore>) -> Self {
        Plan {
            gate: Some(gate.clone()),
            ..Plan::ok()
        }
    }

    fn hanging() -> Self {
        Plan {
            hang: true,
            ..Plan::ok()
        }
    }
}

/// Tells the test that a child's `pre_start` future was dropped, which is how
/// a cut or aborted child shows its end.
struct DropSignal(Arc<Notify>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

struct Child {
    name: &'static str,
    plan: Plan,
    log: Log,
    ready: mpsc::UnboundedSender<&'static str>,
    dropped_start: Arc<Notify>,
}

impl Actor for Child {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _signal = DropSignal(self.dropped_start.clone());
        note(&self.log, format!("enter:{}", self.name));
        if let Some(meet) = &self.plan.meet {
            meet.wait().await;
        }
        if let Some(slot) = &self.plan.calls {
            let boss = loop {
                if let Some(boss) = slot.lock().unwrap().clone() {
                    break boss;
                }
                tokio::task::yield_now().await;
            };
            let answered = if self.plan.via_ref {
                SupervisorRef::of(&boss).count_children().await.is_ok()
            } else {
                boss.send(Cmd::Ping).await.is_ok()
            };
            note(&self.log, format!("answered:{answered}"));
        }
        if self.plan.hang {
            std::future::pending::<()>().await;
        }
        if let Some(gate) = &self.plan.gate {
            gate.acquire().await.expect("the gate stays open").forget();
        }
        match self.plan.fails {
            Fails::PreStartErr => return Err(ActorError::user("refused")),
            Fails::PreStartPanic => panic!("refused by panic"),
            _ => {}
        }
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.plan.fails == Fails::OnStartedErr {
            return Err(ActorError::user("refused"));
        }
        note(&self.log, format!("started:{}", self.name));
        let _ = self.ready.send(self.name);
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        note(&self.log, format!("pre_stop:{}", self.name));
        !self.plan.veto
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        note(&self.log, format!("stopped:{}", self.name));
        Ok(())
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        note(&self.log, format!("dropped:{}", self.name));
    }
}

/// One child the boss spawns while it starts.
#[derive(Clone)]
struct Spec {
    name: &'static str,
    plan: Plan,
    shutdown: Shutdown,
    restart: RestartType,
}

impl Spec {
    fn new(name: &'static str, plan: Plan) -> Self {
        Spec {
            name,
            plan,
            shutdown: Shutdown::Timeout(Duration::from_secs(5)),
            restart: RestartType::Permanent,
        }
    }
}

/// What the boss does after it spawned its children, still in `on_started`.
#[derive(Clone)]
enum After {
    /// `terminate_child`, awaited: the supervisor's own order ends the child.
    Terminate(&'static str),
    /// A plain spawn of one more child.
    Spawn(Spec),
}

enum Cmd {
    /// Spawns the child with `await_init` (and a start bound when given) and
    /// answers `Spawned`.
    SpawnAwaited(Spec, Option<Duration>),
    /// Spawns the child plainly and answers `Spawned`.
    SpawnPlain(Spec),
    /// Starts an awaited spawn with a start bound, polls it once so the
    /// child is registered, drops it and answers `Pong`.
    SpawnAwaitedThenDrop(Spec, Duration),
    /// Answers `Children`: the ids of the specs the supervisor holds.
    Children,
    /// Answers `Pong`.
    Ping,
}

#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Spawned(Result<(), SpawnError>),
    Children(Vec<String>),
    Pong,
}

/// The shared state the boss and its children report to.
#[derive(Clone)]
struct Shared {
    log: Log,
    builds: Arc<AtomicUsize>,
    ready: mpsc::UnboundedSender<&'static str>,
    dropped_start: Arc<Notify>,
    events: mpsc::UnboundedSender<ChildEvent>,
}

struct Boss {
    specs: Vec<Spec>,
    after: Vec<After>,
    shared: Shared,
    /// Where the boss leaves its own handle while it starts.
    me: Arc<Mutex<Option<ActorHandle<Boss>>>>,
}

impl Boss {
    fn child_of(shared: &Shared, spec: &Spec) -> impl Fn() -> Child + Send + Sync + 'static {
        let shared = shared.clone();
        let name = spec.name;
        let plan = spec.plan.clone();
        move || {
            shared.builds.fetch_add(1, Ordering::SeqCst);
            Child {
                name,
                plan: plan.clone(),
                log: shared.log.clone(),
                ready: shared.ready.clone(),
                dropped_start: shared.dropped_start.clone(),
            }
        }
    }
}

impl Actor for Boss {
    type Message = Cmd;
    type Response = Reply;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        *self.me.lock().unwrap() = Some(ctx.self_handle());
        for spec in self.specs.clone() {
            ctx.spawn_child(Self::child_of(&self.shared, &spec))
                .named(spec.name)
                .restart_type(spec.restart)
                .shutdown(spec.shutdown)
                .await?;
        }
        for op in self.after.clone() {
            match op {
                After::Terminate(name) => ctx
                    .terminate_child(name)
                    .await
                    .map_err(|err| ActorError::user(err.to_string()))?,
                After::Spawn(spec) => {
                    ctx.spawn_child(Self::child_of(&self.shared, &spec))
                        .named(spec.name)
                        .restart_type(spec.restart)
                        .shutdown(spec.shutdown)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        Ok(match msg {
            Cmd::SpawnAwaited(spec, bound) => {
                let mut builder = ctx
                    .spawn_child(Self::child_of(&self.shared, &spec))
                    .named(spec.name)
                    .restart_type(spec.restart)
                    .shutdown(spec.shutdown)
                    .await_init();
                if let Some(bound) = bound {
                    builder = builder.start_timeout(bound);
                }
                Reply::Spawned(builder.await.map(|_| ()))
            }
            Cmd::SpawnPlain(spec) => Reply::Spawned(
                ctx.spawn_child(Self::child_of(&self.shared, &spec))
                    .named(spec.name)
                    .restart_type(spec.restart)
                    .shutdown(spec.shutdown)
                    .await
                    .map(|_| ()),
            ),
            Cmd::SpawnAwaitedThenDrop(spec, bound) => {
                let mut call = Box::pin(
                    ctx.spawn_child(Self::child_of(&self.shared, &spec))
                        .named(spec.name)
                        .restart_type(spec.restart)
                        .shutdown(spec.shutdown)
                        .await_init()
                        .start_timeout(bound)
                        .into_future(),
                );
                poll_fn(|cx| {
                    let _ = call.as_mut().poll(cx);
                    Poll::Ready(())
                })
                .await;
                drop(call);
                Reply::Pong
            }
            Cmd::Children => Reply::Children(
                ctx.children()
                    .iter()
                    .map(|child| child.id.to_string())
                    .collect(),
            ),
            Cmd::Ping => Reply::Pong,
        })
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        note(
            &self.shared.log,
            format!("child:{}:{:?}", event.child_id, event.action),
        );
        let _ = self.shared.events.send(event.clone());
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        note(&self.shared.log, "boss_stopped");
        Ok(())
    }
}

struct Rig {
    sys: Arc<ActorSystem>,
    shared: Shared,
    ready: mpsc::UnboundedReceiver<&'static str>,
    events: mpsc::UnboundedReceiver<ChildEvent>,
    me: Arc<Mutex<Option<ActorHandle<Boss>>>>,
}

impl Rig {
    fn new() -> Self {
        let sys = ActorSystem::create(unique("strict")).unwrap();
        let (ready_tx, ready) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();
        Rig {
            sys,
            shared: Shared {
                log: Arc::default(),
                builds: Arc::default(),
                ready: ready_tx,
                dropped_start: Arc::default(),
                events: events_tx,
            },
            ready,
            events,
            me: Arc::default(),
        }
    }

    /// Starts the boss as a task so a test can act while its spawn is
    /// pending: the answer of its spawn.
    fn start(
        &self,
        specs: Vec<Spec>,
        config: SupervisionConfig,
        start_timeout: Option<Duration>,
    ) -> tokio::task::JoinHandle<Result<ActorHandle<Boss>, SpawnError>> {
        self.start_with(specs, Vec::new(), config, start_timeout)
    }

    fn start_with(
        &self,
        specs: Vec<Spec>,
        after: Vec<After>,
        config: SupervisionConfig,
        start_timeout: Option<Duration>,
    ) -> tokio::task::JoinHandle<Result<ActorHandle<Boss>, SpawnError>> {
        let boss = Boss {
            specs,
            after,
            shared: self.shared.clone(),
            me: self.me.clone(),
        };
        let mut builder = boss
            .spawn()
            .named("boss")
            .on_system(&self.sys)
            .with_supervision(config);
        if let Some(bound) = start_timeout {
            builder = builder.start_timeout(bound);
        }
        tokio::spawn(builder.into_future())
    }

    async fn ready(&mut self) -> &'static str {
        timeout(BOUND, self.ready.recv())
            .await
            .expect("a child finishes its start")
            .expect("the rig is alive")
    }

    async fn event(&mut self) -> ChildEvent {
        timeout(BOUND, self.events.recv())
            .await
            .expect("an event arrives")
            .expect("the supervisor is alive")
    }
}

fn strict() -> SupervisionConfig {
    SupervisionConfig::one_for_one().with_strict_start()
}

async fn finished<T>(task: tokio::task::JoinHandle<T>) -> T {
    timeout(BOUND, task)
        .await
        .expect("the spawn answers")
        .expect("the spawn task did not panic")
}

// ---------------------------------------------------------------------------
// Strict start
// ---------------------------------------------------------------------------

/// An end the supervisor itself ordered while it starts is not a failed
/// start, and a child that took the place of the ended one is not judged by
/// the ended one's acknowledgment. Row 1: a `Transient` child whose spec is
/// kept is terminated before it acknowledged (the stop order reached its
/// incarnation). Row 2: a `Temporary` child is terminated, its spec goes, and
/// a healthy child of the same id is spawned in its place; the start ends
/// with the new child up, not with a wait on it.
#[test]
fn an_end_the_supervisor_ordered_during_its_start_is_not_a_failed_start() {
    for row in 0..2 {
        common::run(2, async move {
            let rig = Rig::new();
            let gate = Arc::new(Semaphore::new(0));
            let mut stuck = Spec::new("a", Plan::gated(&gate));
            stuck.shutdown = Shutdown::Kill;
            let after = if row == 0 {
                stuck.restart = RestartType::Transient;
                vec![After::Terminate("a")]
            } else {
                stuck.restart = RestartType::Temporary;
                vec![
                    After::Terminate("a"),
                    After::Spawn(Spec::new("a", Plan::ok())),
                ]
            };
            let started = finished(rig.start_with(vec![stuck], after, strict(), None)).await;
            let boss = started.unwrap_or_else(|err| panic!("row {row}: the start failed: {err}"));
            assert_eq!(
                boss.send(Cmd::Ping).await.unwrap(),
                Reply::Pong,
                "row {row}"
            );
            let seen = lines(&rig.shared.log);
            assert!(
                !seen.iter().any(|l| l == "boss_stopped"),
                "row {row}: {seen:?}"
            );
        });
    }
}

/// A failing child fails the supervisor's start, whichever of three it is and
/// however its initialization fails: the spawn is `Init` carrying
/// `ChildStart` with the child's id and its own error, and the supervisor
/// gets neither `on_child_stopped` nor `on_stopped`.
#[test]
fn a_failing_child_fails_the_supervisors_start() {
    let rows: [(usize, Fails); 3] = [
        (0, Fails::PreStartErr),
        (1, Fails::OnStartedErr),
        (2, Fails::PreStartPanic),
    ];
    for (at, fails) in rows {
        common::run(2, async move {
            let rig = Rig::new();
            let names = ["c0", "c1", "c2"];
            let specs: Vec<Spec> = names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let plan = if i == at {
                        Plan::failing(fails)
                    } else {
                        Plan::ok()
                    };
                    Spec::new(name, plan)
                })
                .collect();
            let started = finished(rig.start(specs, strict(), None)).await;
            let Err(SpawnError::Init(err)) = started else {
                panic!("row {at} {fails:?}: expected an Init error");
            };
            let ActorError::ChildStart { child, error } = *err else {
                panic!("row {at} {fails:?}: expected ChildStart, got {err:?}");
            };
            assert_eq!(child, ActorId::from(names[at]), "row {at} {fails:?}");
            match fails {
                Fails::PreStartPanic => assert!(
                    matches!(*error, ActorError::Panic(_)),
                    "row {at}: the child's panic is the error, got {error:?}"
                ),
                _ => assert_eq!(*error, ActorError::user("refused"), "row {at}"),
            }
            let seen = lines(&rig.shared.log);
            assert!(
                !seen
                    .iter()
                    .any(|l| l == "boss_stopped" || l.starts_with("child:")),
                "row {at} {fails:?}: a failed start has no callback, got {seen:?}"
            );
        });
    }
}

/// The children already started stop in reverse start order, each under its
/// own `Shutdown`: the one with `Shutdown::Kill` is killed with no hook, the
/// one that vetoes its stop is asked once and then killed at its window, the
/// plain one stops in order.
#[test]
fn started_children_stop_in_reverse_order_under_their_own_shutdown() {
    common::run(2, async {
        let mut rig = Rig::new();
        let last = Arc::new(Semaphore::new(0));
        let mut c1 = Spec::new("c1", Plan::ok());
        c1.shutdown = Shutdown::Kill;
        let mut c2 = Spec::new(
            "c2",
            Plan {
                veto: true,
                ..Plan::ok()
            },
        );
        c2.shutdown = Shutdown::Timeout(Duration::from_millis(100));
        // c3 fails once the other three have finished their start.
        let failing = Plan {
            fails: Fails::PreStartErr,
            ..Plan::gated(&last)
        };
        let specs = vec![
            Spec::new("c0", Plan::ok()),
            c1,
            c2,
            Spec::new("c3", failing),
        ];
        let start = rig.start(specs, strict(), None);
        for _ in 0..3 {
            rig.ready().await;
        }
        last.add_permits(1);
        let started = finished(start).await;
        assert!(
            matches!(started, Err(SpawnError::Init(_))),
            "got {started:?}"
        );

        let seen = lines(&rig.shared.log);
        let drops: Vec<&str> = seen
            .iter()
            .filter(|l| l.starts_with("dropped:") && !l.ends_with("c3"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            drops,
            ["dropped:c2", "dropped:c1", "dropped:c0"],
            "reverse start order, got {seen:?}"
        );
        assert!(
            seen.contains(&"stopped:c0".to_string()),
            "c0 stops in order"
        );
        assert!(
            !seen.contains(&"stopped:c1".to_string()),
            "Kill runs no hook"
        );
        assert!(
            !seen.contains(&"pre_stop:c1".to_string()),
            "Kill asks nothing"
        );
        assert!(
            seen.contains(&"pre_stop:c2".to_string()),
            "c2 is asked once"
        );
        assert!(
            !seen.contains(&"stopped:c2".to_string()),
            "c2 vetoed, then was killed"
        );
    });
}

/// Two children that wait for each other inside `pre_start` both start, so
/// the strict wait does not serialize their initialization; and the
/// supervisor's start does wait: it stays pending while a child is still in
/// `pre_start`, and completes once that child is done.
#[test]
fn children_initialize_concurrently() {
    common::run(2, async {
        let mut rig = Rig::new();
        let barrier = Arc::new(Barrier::new(2));
        let slow = Arc::new(Semaphore::new(0));
        let meet = Plan {
            meet: Some(barrier),
            ..Plan::ok()
        };
        let c1 = Plan {
            gate: Some(slow.clone()),
            ..meet.clone()
        };
        let specs = vec![Spec::new("c0", meet), Spec::new("c1", c1)];
        let mut start = rig.start(specs, strict(), None);
        // Both children are past the barrier, so c0 is up; c1 waits at its gate.
        assert_eq!(rig.ready().await, "c0");
        assert!(
            timeout(Duration::from_millis(200), &mut start)
                .await
                .is_err(),
            "the supervisor's start waits for the child still in pre_start"
        );
        slow.add_permits(1);
        assert_eq!(rig.ready().await, "c1");
        let boss = finished(start).await.expect("both children started");
        assert_eq!(boss.send(Cmd::Ping).await.unwrap(), Reply::Pong);
    });
}

/// Erlang/OTP's hazard, kept: a child that calls its supervisor during its
/// own initialization, with a message or through a `SupervisorRef`, waits for
/// a supervisor that waits for it, until the start bound. Without strict
/// start the same call is answered once the supervisor is up.
#[test]
fn a_child_that_calls_its_supervisor_in_init_waits_for_the_bound() {
    for (strict_start, via_ref) in [(true, false), (true, true), (false, false), (false, true)] {
        common::run(2, async move {
            let rig = Rig::new();
            let child = Plan {
                calls: Some(rig.me.clone()),
                via_ref,
                ..Plan::ok()
            };
            let specs = vec![Spec::new("c0", child)];
            if strict_start {
                let started = finished(rig.start(specs, strict(), Some(SHORT))).await;
                assert!(
                    matches!(started, Err(SpawnError::StartTimeout)),
                    "strict, ref {via_ref}: got {started:?}"
                );
                timeout(BOUND, rig.shared.dropped_start.notified())
                    .await
                    .expect("the cut supervisor takes its child down");
                assert!(
                    !lines(&rig.shared.log).iter().any(|l| l == "answered:true"),
                    "the call was never answered (ref {via_ref})"
                );
            } else {
                let boss = finished(rig.start(specs, SupervisionConfig::one_for_one(), None))
                    .await
                    .expect("the default start does not wait for the child");
                timeout(BOUND, async {
                    while !lines(&rig.shared.log).iter().any(|l| l == "answered:true") {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap_or_else(|_| {
                    panic!("the call is answered once the supervisor is up (ref {via_ref})")
                });
                drop(boss);
            }
        });
    }
}

/// A `Kill` that reaches the supervisor while it waits ends it at once, even
/// when its kill grace is far away: the wait reads the lane, it does not wait
/// for the abort.
#[test]
fn kill_during_the_wait_ends_the_supervisor() {
    common::run(2, async {
        let rig = Rig::new();
        let boss = Boss {
            specs: vec![Spec::new("c0", Plan::hanging())],
            after: Vec::new(),
            shared: rig.shared.clone(),
            me: rig.me.clone(),
        };
        let start = tokio::spawn(
            boss.spawn()
                .named("boss")
                .on_system(&rig.sys)
                .with_supervision(strict())
                .kill_grace(Duration::from_secs(3600))
                .into_future(),
        );
        // The child is inside its start, so the supervisor is in its wait.
        timeout(BOUND, async {
            while !lines(&rig.shared.log).iter().any(|l| l == "enter:c0") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the child enters its start");
        let report = timeout(BOUND, rig.sys.kill_by_id(&ActorId::from("boss")))
            .await
            .expect("the kill takes effect without waiting for the grace");
        assert!(report.is_some(), "the supervisor was found");
        let started = finished(start).await;
        assert!(
            matches!(started, Err(SpawnError::Killed)),
            "got {started:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// await_init
// ---------------------------------------------------------------------------

/// A boss that is up, to spawn awaited children into.
async fn running_boss(rig: &Rig, config: SupervisionConfig) -> ActorHandle<Boss> {
    finished(rig.start(Vec::new(), config, None))
        .await
        .expect("the empty boss starts")
}

/// `await_init` returns after the child's initialization, and returns its
/// error when it fails: success, an `Err`, a panic and a start bound that
/// passes. A failed child leaves no spec.
#[test]
fn await_init_returns_after_init_and_returns_the_error() {
    common::run(2, async {
        let rig = Rig::new();
        let boss = running_boss(&rig, SupervisionConfig::one_for_one()).await;

        // Success: the call stays pending while the child is in pre_start.
        let gate = Arc::new(Semaphore::new(0));
        let mut call = {
            let boss = boss.clone();
            let spec = Spec::new("ok", Plan::gated(&gate));
            tokio::spawn(async move { boss.send(Cmd::SpawnAwaited(spec, None)).await })
        };
        timeout(BOUND, async {
            while !lines(&rig.shared.log).iter().any(|l| l == "enter:ok") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the child enters its start");
        assert!(
            timeout(Duration::from_millis(200), &mut call)
                .await
                .is_err(),
            "the call waits for the child's initialization"
        );
        gate.add_permits(1);
        let reply = finished(call).await.unwrap();
        assert_eq!(reply, Reply::Spawned(Ok(())));
        assert!(lines(&rig.shared.log).contains(&"started:ok".to_string()));
        assert_eq!(
            boss.send(Cmd::Children).await.unwrap(),
            Reply::Children(vec!["ok".to_string()])
        );

        // Failures: the error comes back and no spec stays.
        let rows: [(&'static str, Plan, Option<Duration>); 4] = [
            ("err", Plan::failing(Fails::PreStartErr), None),
            ("late", Plan::failing(Fails::OnStartedErr), None),
            ("panic", Plan::failing(Fails::PreStartPanic), None),
            ("slow", Plan::hanging(), Some(SHORT)),
        ];
        for (name, plan, bound) in rows {
            let reply = boss
                .send(Cmd::SpawnAwaited(Spec::new(name, plan), bound))
                .await
                .unwrap();
            let Reply::Spawned(Err(err)) = reply else {
                panic!("{name}: expected an error, got {reply:?}");
            };
            match name {
                "err" | "late" => assert_eq!(
                    err,
                    SpawnError::Init(Box::new(ActorError::user("refused"))),
                    "{name}"
                ),
                "panic" => assert!(
                    matches!(&err, SpawnError::Init(inner) if matches!(**inner, ActorError::Panic(_))),
                    "{name}: got {err:?}"
                ),
                _ => assert_eq!(err, SpawnError::StartTimeout, "{name}"),
            }
            assert_eq!(
                boss.send(Cmd::Children).await.unwrap(),
                Reply::Children(vec!["ok".to_string()]),
                "{name}: the failed child keeps no spec"
            );
        }
        timeout(BOUND, rig.shared.dropped_start.notified())
            .await
            .expect("the child cut at the bound is taken down");
    });
}

/// A failed `await_init` keeps no spec, delivers no `on_child_stopped` and
/// charges no budget: the supervisor allows no restart at all, stays up, and
/// a spawn under the same id succeeds once the failed incarnation ended.
#[test]
fn a_failed_await_init_keeps_no_spec_no_callback_and_no_charge() {
    common::run(2, async {
        let mut rig = Rig::new();
        let config = SupervisionConfig::one_for_one().max_restarts(0, Duration::from_secs(60));
        let boss = running_boss(&rig, config).await;

        let reply = boss
            .send(Cmd::SpawnAwaited(
                Spec::new("w", Plan::failing(Fails::PreStartErr)),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(
            reply,
            Reply::Spawned(Err(SpawnError::Init(Box::new(ActorError::user("refused")))))
        );
        assert_eq!(
            boss.send(Cmd::Children).await.unwrap(),
            Reply::Children(vec![])
        );

        // The same id again: it waits for the failed incarnation's subtree
        // and then starts.
        let again = boss
            .send(Cmd::SpawnAwaited(Spec::new("w", Plan::ok()), None))
            .await
            .unwrap();
        assert_eq!(again, Reply::Spawned(Ok(())));

        // A temporary child's end is the first report the supervisor
        // delivers; the failed one's was dropped before it.
        let mut probe = Spec::new("probe", Plan::ok());
        probe.restart = RestartType::Temporary;
        assert_eq!(
            boss.send(Cmd::SpawnPlain(probe)).await.unwrap(),
            Reply::Spawned(Ok(()))
        );
        let probe_handle = rig
            .sys
            .get::<Child>("probe")
            .expect("the probe is registered");
        probe_handle.stop(StopReason::Graceful).await.unwrap();
        let event = rig.event().await;
        assert_eq!(
            event.child_id,
            ActorId::from("probe"),
            "no report of w came first"
        );
        assert_eq!(boss.send(Cmd::Ping).await.unwrap(), Reply::Pong);
        assert!(
            !lines(&rig.shared.log)
                .iter()
                .any(|l| l.starts_with("child:w")),
            "the failed child's end reached no callback"
        );
    });
}

/// Dropping the future of an `await_init` spawn once the child is registered
/// leaves it as a plain `spawn_child` leaves it: the start bound the dropped
/// wait carried no longer cuts the first start, and a later failure of the
/// initialization is supervised like any crash (reported, restarted, charged).
#[test]
fn a_dropped_await_init_leaves_a_plain_child() {
    common::run(2, async {
        let mut rig = Rig::new();
        let config = SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60));
        let boss = running_boss(&rig, config).await;
        let gate = Arc::new(Semaphore::new(0));
        let plan = Plan {
            fails: Fails::PreStartErr,
            ..Plan::gated(&gate)
        };
        let reply = boss
            .send(Cmd::SpawnAwaitedThenDrop(Spec::new("d", plan), SHORT))
            .await
            .unwrap();
        assert_eq!(reply, Reply::Pong);
        timeout(BOUND, async {
            while !lines(&rig.shared.log).iter().any(|l| l == "enter:d") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the child enters its start");

        // Well past the bound the dropped wait carried: the child is still in
        // its start, registered, and nothing cut it.
        tokio::time::sleep(SHORT * 4).await;
        assert!(
            timeout(
                Duration::from_millis(1),
                rig.shared.dropped_start.notified()
            )
            .await
            .is_err(),
            "the start was cut"
        );
        assert_eq!(
            boss.send(Cmd::Children).await.unwrap(),
            Reply::Children(vec!["d".to_string()])
        );

        // Its initialization now fails: the supervisor reports and restarts it.
        gate.add_permits(1);
        let event = rig.event().await;
        assert_eq!(event.child_id, ActorId::from("d"));
        assert!(matches!(event.reason, StopReason::Failure(_)), "{event:?}");
        assert_eq!(event.action, SupervisionAction::RestartInitiated);
        assert_eq!(boss.send(Cmd::Ping).await.unwrap(), Reply::Pong);
    });
}
