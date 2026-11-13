#![allow(dead_code)]
#![cfg(feature = "tracing")]
//! The crash, supervisor and progress reports, read from the log lines a
//! `tracing` subscriber receives: which exits are reported and at which
//! level, what the crash report holds, which child ends a supervisor
//! reports for each restart type, and that a sensitive actor's failures
//! reach no log line as text.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorSystem, PgScope, RestartType, StopReason,
    SupervisionConfig,
};

#[path = "common/capture.rs"]
mod capture;
mod common;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("reports-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

fn new_system() -> Arc<ActorSystem> {
    ActorSystem::create(uname("sys")).expect("system name must be unique")
}

// ---------------------------------------------------------------------------
// The actor every row uses
// ---------------------------------------------------------------------------

enum Msg {
    /// Ends the actor with an error carrying this text.
    Fail(&'static str),
    /// Ends the actor with a panic carrying this text.
    Boom(&'static str),
    /// Parks the turn until the gate opens, then ends the actor with an error.
    Hold(Arc<tokio::sync::Notify>),
    /// Mail whose drop panics with this text.
    Bomb(#[allow(dead_code)] Bomb),
}

/// Panics with its text when dropped.
struct Bomb(&'static str);

impl Drop for Bomb {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            panic!("{}", self.0);
        }
    }
}

/// Counts the calls of its `format_status`, which answers a fixed text or
/// panics.
struct Subject {
    status_calls: Arc<AtomicUsize>,
    status_panics: bool,
    /// Its drop panics with this text, when set.
    drop_panic: Option<&'static str>,
}

impl Subject {
    fn new(calls: &Arc<AtomicUsize>) -> Self {
        Self {
            status_calls: calls.clone(),
            status_panics: false,
            drop_panic: None,
        }
    }
}

impl Actor for Subject {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Fail(text) => Err(ActorError::user(text)),
            Msg::Boom(text) => panic!("{text}"),
            Msg::Hold(gate) => {
                gate.notified().await;
                Err(ActorError::user("held"))
            }
            Msg::Bomb(_) => Ok(()),
        }
    }

    fn format_status(&self) -> Option<String> {
        self.status_calls.fetch_add(1, Ordering::SeqCst);
        if self.status_panics {
            panic!("format_status refuses");
        }
        Some("the-state-text".to_string())
    }
}

impl Drop for Subject {
    fn drop(&mut self) {
        if let Some(text) = self.drop_panic {
            if !std::thread::panicking() {
                panic!("{text}");
            }
        }
    }
}

/// The lines that record a drop panic of actor `id` that a step discards.
fn discarded(id: &str) -> Vec<capture::Seen> {
    capture::events(|seen| {
        seen.field("actor_id") == Some(id)
            && seen
                .field("message")
                .is_some_and(|message| message.contains("this panic is discarded"))
    })
}

/// The crash reports written for the actor `id`.
fn crash_reports(id: &str) -> Vec<capture::Seen> {
    capture::events(|seen| {
        seen.target == "tokio_actors::crash" && seen.field("actor_id") == Some(id)
    })
}

/// How a row ends its actor.
#[derive(Clone, Copy)]
enum End {
    Fail,
    Boom,
    Stop(fn() -> StopReason),
}

fn graceful() -> StopReason {
    StopReason::Graceful
}
fn parent_request() -> StopReason {
    StopReason::ParentRequest
}
fn cancelled() -> StopReason {
    StopReason::Cancelled
}
fn parent_killed() -> StopReason {
    StopReason::ParentKilled
}
fn kill() -> StopReason {
    StopReason::Kill
}

/// Ends `handle`'s actor as `end` says.
async fn end_it(handle: &ActorHandle<Subject>, end: End, text: &'static str) {
    match end {
        End::Fail => {
            let _ = handle.notify(Msg::Fail(text)).await;
        }
        End::Boom => {
            let _ = handle.notify(Msg::Boom(text)).await;
        }
        End::Stop(reason) => {
            let _ = handle.stop(reason()).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Crash reports
// ---------------------------------------------------------------------------

/// One crash report, at error level, for a failure and for the end a
/// supervisor's death orders; none for an orderly end or a kill. The actor's
/// `format_status` runs once for a reported end and not at all otherwise,
/// and its text is in the report, or the placeholder when it panics.
#[test]
fn a_crash_is_reported_once_for_the_exits_that_are_crashes() {
    capture::install();
    // (end, state calls, state text in the report)
    let rows: &[(&str, End, bool, Option<&str>)] = &[
        ("error", End::Fail, true, Some("the-state-text")),
        ("panic", End::Boom, true, Some("the-state-text")),
        (
            "parent killed",
            End::Stop(parent_killed),
            true,
            Some("the-state-text"),
        ),
        ("graceful", End::Stop(graceful), false, None),
        ("parent request", End::Stop(parent_request), false, None),
        ("cancelled", End::Stop(cancelled), false, None),
        ("kill", End::Stop(kill), false, None),
    ];
    common::run(2, async {
        let sys = new_system();
        for (row, end, reported, state) in rows {
            let calls = Arc::new(AtomicUsize::new(0));
            let name = uname("crash");
            let handle = Subject::new(&calls)
                .spawn()
                .named(name.clone())
                .on_system(&sys)
                .await
                .unwrap();
            end_it(&handle, *end, "the-failure-text").await;
            capture::until("the actor's end line", || capture::ended(&name)).await;
            let seen = crash_reports(&name);
            assert_eq!(seen.len(), usize::from(*reported), "row {row}: {seen:?}");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                usize::from(*reported),
                "row {row}: format_status calls"
            );
            if let Some(report) = seen.first() {
                assert_eq!(report.level, "ERROR", "row {row}");
                assert_eq!(report.field("state"), *state, "row {row}");
                assert!(
                    report
                        .field("reason")
                        .is_some_and(|r| r.contains("the-failure-text"))
                        || matches!(end, End::Stop(_)),
                    "row {row}: {report:?}"
                );
            }
        }
        // A `format_status` that panics leaves the placeholder, and the
        // actor ends with the failure that ended it.
        let calls = Arc::new(AtomicUsize::new(0));
        let name = uname("crash-panicking-status");
        let mut subject = Subject::new(&calls);
        subject.status_panics = true;
        let handle = subject
            .spawn()
            .named(name.clone())
            .on_system(&sys)
            .await
            .unwrap();
        end_it(&handle, End::Fail, "the-failure-text").await;
        capture::until("the actor's end line", || capture::ended(&name)).await;
        let seen = crash_reports(&name);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].field("state"), Some("format_status panicked"));
        assert!(matches!(
            handle.wait_stopped().await,
            StopReason::Failure(ActorError::User(_))
        ));
    });
}

// ---------------------------------------------------------------------------
// Supervisor reports
// ---------------------------------------------------------------------------

/// A supervisor that starts the children it is given.
struct Boss {
    kids: Vec<(String, RestartType, Arc<AtomicUsize>)>,
    seen_deaths: Arc<AtomicUsize>,
}

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for (name, restart, calls) in &self.kids {
            let calls = calls.clone();
            ctx.spawn_child(move || Subject::new(&calls))
                .named(name.clone())
                .restart_type(*restart)
                .await?;
        }
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        _event: &tokio_actors::ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.seen_deaths.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn supervisor_reports(report: &str, child: &str) -> Vec<capture::Seen> {
    capture::events(|seen| {
        seen.target == "tokio_actors::supervisor"
            && seen.field("report") == Some(report)
            && seen.field("child") == Some(child)
    })
}

/// A supervisor reports a child's end as `child_terminated` by the child's
/// restart type: every exit of a permanent child, and every exit of a
/// transient or temporary one that `StopReason::is_normal` does not call
/// clean, the reading the restart decision makes.
#[test]
fn a_childs_end_is_reported_by_its_restart_type() {
    capture::install();
    use RestartType::{Permanent, Temporary, Transient};
    let rows: &[(&str, RestartType, End, bool)] = &[
        ("permanent graceful", Permanent, End::Stop(graceful), true),
        (
            "permanent request",
            Permanent,
            End::Stop(parent_request),
            true,
        ),
        ("permanent error", Permanent, End::Fail, true),
        ("permanent kill", Permanent, End::Stop(kill), true),
        ("transient graceful", Transient, End::Stop(graceful), false),
        (
            "transient request",
            Transient,
            End::Stop(parent_request),
            false,
        ),
        ("transient panic", Transient, End::Boom, true),
        (
            "transient parent killed",
            Transient,
            End::Stop(parent_killed),
            false,
        ),
        ("transient cancelled", Transient, End::Stop(cancelled), true),
        ("transient kill", Transient, End::Stop(kill), true),
        ("temporary graceful", Temporary, End::Stop(graceful), false),
        ("temporary cancelled", Temporary, End::Stop(cancelled), true),
        ("temporary kill", Temporary, End::Stop(kill), true),
    ];
    common::run(2, async {
        let sys = new_system();
        let kids: Vec<_> = rows
            .iter()
            .map(|(_, restart, _, _)| (uname("kid"), *restart, Arc::new(AtomicUsize::new(0))))
            .collect();
        let deaths = Arc::new(AtomicUsize::new(0));
        let _boss = Boss {
            kids: kids.clone(),
            seen_deaths: deaths.clone(),
        }
        .spawn()
        .named(uname("boss"))
        .on_system(&sys)
        .with_supervision(
            SupervisionConfig::one_for_one().max_restarts(100, Duration::from_secs(60)),
        )
        .await
        .unwrap();
        for (i, (row, _, end, reported)) in rows.iter().enumerate() {
            let name = &kids[i].0;
            let handle = loop {
                if let Some(h) = sys.get::<Subject>(name) {
                    if h.is_alive() {
                        break h;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            let before = deaths.load(Ordering::SeqCst);
            end_it(&handle, *end, "x").await;
            capture::until("the supervisor handled the death", || {
                deaths.load(Ordering::SeqCst) > before
            })
            .await;
            let seen = supervisor_reports("child_terminated", name);
            assert_eq!(seen.len(), usize::from(*reported), "row {row}: {seen:?}");
            if let Some(report) = seen.first() {
                assert_eq!(report.level, "ERROR", "row {row}");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Sensitive actors
// ---------------------------------------------------------------------------

/// No log line of any target carries the text of a sensitive actor's
/// failure, whether it is spawned sensitive, a child declared sensitive or a
/// type that says so, and the text of an ordinary actor's does appear. That
/// holds for the second of two drop panics of one step, which the actor's end
/// discards and logs.
#[test]
fn a_sensitive_actors_failures_reach_no_log_line_as_text() {
    capture::install();
    struct Vault(Arc<AtomicUsize>);
    impl Actor for Vault {
        const SENSITIVE: bool = true;
        type Message = Msg;
        type Response = ();
        async fn handle(&mut self, msg: Msg, _c: &mut ActorContext<Self>) -> ActorResult<()> {
            match msg {
                Msg::Fail(text) => Err(ActorError::user(text)),
                Msg::Boom(text) => panic!("{text}"),
                Msg::Hold(_) | Msg::Bomb(_) => Ok(()),
            }
        }
        fn format_status(&self) -> Option<String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Some("SECRET-state".to_string())
        }
    }
    common::run(2, async {
        let sys = new_system();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut ids = Vec::new();
        // Spawned sensitive: an error, a panic, and a value whose drop
        // panics after the failure.
        for (end, text, drop_panic) in [
            (End::Fail, "SECRET-error", None),
            (End::Boom, "SECRET-panic", None),
            (End::Boom, "SECRET-first", Some("SECRET-drop")),
        ] {
            let name = uname("secret");
            let mut subject = Subject::new(&calls);
            subject.drop_panic = drop_panic;
            let handle = subject
                .spawn()
                .named(name.clone())
                .sensitive()
                .on_system(&sys)
                .await
                .unwrap();
            end_it(&handle, end, text).await;
            ids.push(name);
        }
        // A type that declares itself sensitive.
        let name = uname("vault");
        let vault = Vault(calls.clone())
            .spawn()
            .named(name.clone())
            .on_system(&sys)
            .await
            .unwrap();
        let _ = vault.notify(Msg::Boom("SECRET-vault")).await;
        ids.push(name);
        // A sensitive child of a supervisor that restarts it.
        let child = uname("secret-child");
        let boss = Flaky {
            child: child.clone(),
        };
        let _boss = boss
            .spawn()
            .named(uname("boss"))
            .on_system(&sys)
            .with_supervision(
                SupervisionConfig::one_for_one().max_restarts(100, Duration::from_secs(60)),
            )
            .await
            .unwrap();
        let handle = loop {
            if let Some(h) = sys.get::<Subject>(&child) {
                if h.is_alive() {
                    break h;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let _ = handle.notify(Msg::Boom("SECRET-child")).await;
        ids.push(child.clone());
        // Two queued messages and the actor's own value whose drops panic
        // when the actor ends: the first panic is kept, and each later one
        // is discarded and logged, once from the queue's drain and once from
        // the actor's end. The row of an ordinary actor shows the lines
        // carry the text.
        for (sensitive, first, second, third) in [
            (
                true,
                "SECRET-queued-a",
                "SECRET-queued-b",
                "SECRET-queued-c",
            ),
            (false, "OPEN-queued-a", "OPEN-queued-b", "OPEN-queued-c"),
        ] {
            let name = uname("queued");
            let mut subject = Subject::new(&Arc::new(AtomicUsize::new(0)));
            subject.drop_panic = Some(third);
            let builder = subject.spawn().named(name.clone());
            let builder = if sensitive {
                builder.sensitive()
            } else {
                builder
            };
            let handle = builder.on_system(&sys).await.unwrap();
            let gate = Arc::new(tokio::sync::Notify::new());
            handle.notify(Msg::Hold(gate.clone())).await.unwrap();
            handle.notify(Msg::Bomb(Bomb(first))).await.unwrap();
            handle.notify(Msg::Bomb(Bomb(second))).await.unwrap();
            gate.notify_one();
            capture::until("the two discarded drop panics", || {
                discarded(&name).len() == 2
            })
            .await;
            for seen in discarded(&name) {
                let line = seen.line();
                assert_eq!(
                    line.contains(second) || line.contains(third),
                    !sensitive,
                    "row sensitive={sensitive}: the discarded panic's text: {line}"
                );
            }
            ids.push(name);
        }
        // An ordinary actor, whose text does appear.
        let open = uname("open");
        let handle = Subject::new(&calls)
            .spawn()
            .named(open.clone())
            .on_system(&sys)
            .await
            .unwrap();
        let _ = handle.notify(Msg::Boom("OPEN-text")).await;
        for id in ids.iter().chain([&open]) {
            capture::until("the actor's end line", || capture::ended(id)).await;
        }
        // The supervisor has handled the child's death by now.
        capture::until("the child's report", || {
            !supervisor_reports("child_terminated", &child).is_empty()
        })
        .await;
        let leaked: Vec<_> = capture::all()
            .into_iter()
            .filter(|seen| seen.line().contains("SECRET"))
            .collect();
        assert!(
            leaked.is_empty(),
            "text of a sensitive actor in the log: {leaked:?}"
        );
        assert!(
            crash_reports(&open)
                .iter()
                .any(|r| r.line().contains("OPEN-text")),
            "an ordinary actor's crash report names the text"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only the ordinary actor formats its state"
        );
    });
}

/// Does nothing; the watcher's targets and the request's callee.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Arms one map that panics with the text.
enum Arm {
    Monitor(ActorHandle<Idle>, &'static str),
    Trap(ActorHandle<Idle>, &'static str),
    Request(ActorHandle<Idle>, &'static str),
    Group(PgScope, &'static str),
}

struct Mapper;

impl Actor for Mapper {
    type Message = Arm;
    type Response = ();

    async fn handle(&mut self, msg: Arm, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Arm::Monitor(target, text) => {
                ctx.monitor(&target, move |_| panic!("{text}"))
                    .expect("running");
            }
            Arm::Trap(target, text) => {
                ctx.trap_exit(move |_| panic!("{text}"));
                ctx.link(&target).expect("running");
            }
            Arm::Request(target, text) => {
                ctx.request(&target, (), move |_| panic!("{text}"))
                    .expect("running");
            }
            Arm::Group(scope, text) => {
                ctx.monitor_group(&scope, "map-panic-group", move |_| panic!("{text}"))
                    .expect("running");
            }
        }
        Ok(())
    }
}

/// A map that panics (a watcher's, a link trap's, a request's and a group
/// monitor's) fails its actor, and the line that records the panic carries
/// the panic's text only for an actor that is not sensitive: no line of any
/// target holds the text of a sensitive actor's map panic.
#[test]
fn a_sensitive_actors_map_panic_reaches_no_log_line_as_text() {
    capture::install();
    // (the kind, the line's message, the secret text, the open text)
    const KINDS: [(&str, &str, &str, &str); 4] = [
        (
            "monitor",
            "a monitor's map function panicked",
            "MAPSECRET-monitor",
            "MAPOPEN-monitor",
        ),
        (
            "trap",
            "a trap map panicked",
            "MAPSECRET-trap",
            "MAPOPEN-trap",
        ),
        (
            "request",
            "a request's map function panicked",
            "MAPSECRET-request",
            "MAPOPEN-request",
        ),
        (
            "group",
            "a group monitor's map function panicked",
            "MAPSECRET-group",
            "MAPOPEN-group",
        ),
    ];
    common::run(2, async {
        let sys = new_system();
        let scope = sys.pg();
        let mut names = Vec::new();
        for (kind, line, secret, open) in KINDS {
            for sensitive in [true, false] {
                let text = if sensitive { secret } else { open };
                let name = uname(&format!("mapper-{kind}"));
                let builder = Mapper.spawn().named(name.clone());
                let builder = if sensitive {
                    builder.sensitive()
                } else {
                    builder
                };
                let watcher = builder.on_system(&sys).await.unwrap();
                let target = Idle
                    .spawn()
                    .named(uname("target"))
                    .on_system(&sys)
                    .await
                    .unwrap();
                let arm = match kind {
                    "monitor" => Arm::Monitor(target.clone(), text),
                    "trap" => Arm::Trap(target.clone(), text),
                    "request" => Arm::Request(target.clone(), text),
                    _ => Arm::Group(scope.clone(), text),
                };
                watcher.send(arm).await.unwrap();
                match kind {
                    "monitor" | "trap" => target.stop(StopReason::Graceful).await.unwrap(),
                    "request" => {}
                    _ => scope.join("map-panic-group", &target).unwrap(),
                }
                let lines = || {
                    capture::events(|seen| {
                        seen.field("actor_id") == Some(name.as_str())
                            && seen
                                .field("message")
                                .is_some_and(|message| message.contains(line))
                    })
                };
                capture::until(&format!("{kind} sensitive={sensitive}: the line"), || {
                    !lines().is_empty()
                })
                .await;
                assert_eq!(
                    lines().iter().any(|seen| seen.line().contains(text)),
                    !sensitive,
                    "{kind} sensitive={sensitive}: the text is in the map panic's line exactly for an ordinary actor: {:?}",
                    lines().iter().map(capture::Seen::line).collect::<Vec<_>>()
                );
                match watcher.wait_stopped().await {
                    StopReason::Failure(ActorError::Panic(message)) => {
                        assert!(message.contains(text), "{kind}: {message}");
                    }
                    other => panic!("{kind}: the map's panic was expected, got {other:?}"),
                }
                names.push(name);
            }
        }
        for name in &names {
            capture::until("the actor's end line", || capture::ended(name)).await;
        }
        let leaked: Vec<_> = capture::all()
            .into_iter()
            .filter(|seen| seen.line().contains("MAPSECRET"))
            .collect();
        assert!(
            leaked.is_empty(),
            "text of a sensitive actor's map panic in the log: {leaked:?}"
        );
    });
}

/// Supervises one sensitive child.
struct Flaky {
    child: String,
}

impl Actor for Flaky {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let calls = Arc::new(AtomicUsize::new(0));
        ctx.spawn_child(move || Subject::new(&calls))
            .named(self.child.clone())
            .restart_type(RestartType::Permanent)
            .sensitive()
            .await?;
        Ok(())
    }
}
