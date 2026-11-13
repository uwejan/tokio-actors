//! A panic of a signal map function that lands after the watcher's message
//! loop ended.
//!
//! Every window between the loop's end and the choice of the final reason is
//! held open in turn, a map function of each kind is made to panic inside it,
//! and the exit reason is checked: the panic fails the actor unless a `Kill`
//! or an earlier `Failure` stands. The windows are the instants the stop
//! passes through after its loop, held either by a user callback (the final
//! child report, a delivery of the stage, `on_stopped`, a child's stop) or by
//! a gate the stop itself waits at (between the steps, where no callback
//! runs).

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, timeout};

use crate::actor::context::ActorContext;
use crate::actor::runtime::end_hooks::{self, Gate};
use crate::actor::{Actor, ActorExt};
use crate::error::ActorResult;
use crate::types::{Down, GroupChange, LinkExit, RestartType, StopReason};
use crate::{ActorError, ActorHandle, ActorSystem, ChildEvent, PgScope};

const BOUND: Duration = Duration::from_secs(10);
const SETTLE: Duration = Duration::from_millis(100);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("late-map-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    within(what, async {
        while !ready() {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Peer,
    Group,
    Link,
}

/// The instants after the loop's end, in the order the stop passes them.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
enum Window {
    /// After the loop, before the final child reports (a gate).
    LoopEnded,
    /// Inside the delivery of the final child report (a callback).
    FinalReport,
    /// After the final reports, before the monitor stage (a gate).
    FinalDone,
    /// Inside a delivery of the monitor stage (a callback).
    Stage,
    /// After the stage, before `on_stopped` (a gate).
    StageDone,
    /// Inside `on_stopped` (a callback).
    OnStopped,
    /// Inside a child's stop (its `on_stopped`).
    ChildStop,
    /// After the children are stopped, before the forwarders end (a gate).
    TeardownDone,
}

const WINDOWS: [Window; 8] = [
    Window::LoopEnded,
    Window::FinalReport,
    Window::FinalDone,
    Window::Stage,
    Window::StageDone,
    Window::OnStopped,
    Window::ChildStop,
    Window::TeardownDone,
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Variant {
    /// The loop ends `Graceful`.
    Plain,
    /// The loop ends with a `Failure` that was there first.
    FailureStands,
    /// A `Kill` is accepted after the panic landed.
    KillAfter,
}

struct Victim;

impl Actor for Victim {
    type Message = ();
    type Response = ();
    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A child whose stop waits at its gate.
struct SlowKid(Option<Arc<Gate>>);

impl Actor for SlowKid {
    type Message = ();
    type Response = ();
    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(&mut self, _: &StopReason, _: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(gate) = &self.0 {
            gate.pass().await;
        }
        Ok(())
    }
}

/// The user callbacks of the watcher that hold a window open.
#[derive(Default, Clone)]
struct Holds {
    final_report: Option<Arc<Gate>>,
    stage: Option<Arc<Gate>>,
    on_stopped: Option<Arc<Gate>>,
    child_stop: Option<Arc<Gate>>,
}

enum Cmd {
    /// Sets the watcher up for one kind of signal map; the answer is the
    /// child whose death is the final report.
    Setup {
        kind: Kind,
        v1: ActorHandle<Victim>,
        v2: ActorHandle<Victim>,
        scope: PgScope,
    },
    /// Waits at the gate.
    Wait(Arc<Gate>),
    /// The notice of the second monitored actor.
    Note(Down),
    /// The message the panicking maps would make.
    Never,
}

struct Watcher {
    holds: Holds,
}

impl Actor for Watcher {
    type Message = Cmd;
    type Response = Option<ActorHandle<Victim>>;

    async fn handle(
        &mut self,
        msg: Cmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Option<ActorHandle<Victim>>> {
        match msg {
            Cmd::Setup {
                kind,
                v1,
                v2,
                scope,
            } => {
                match kind {
                    Kind::Peer => {
                        ctx.monitor(&v1, |_: Down| -> Cmd { panic!("map boom") })
                            .expect("running");
                    }
                    Kind::Group => {
                        ctx.monitor_group(&scope, "g", |_: GroupChange| -> Cmd {
                            panic!("map boom")
                        })
                        .expect("running");
                    }
                    Kind::Link => {
                        ctx.trap_exit(|_: LinkExit| -> Cmd { panic!("map boom") });
                        ctx.link(&v1).expect("running");
                    }
                }
                ctx.monitor(&v2, Cmd::Note).expect("running");
                let gate = self.holds.child_stop.clone();
                ctx.spawn_child(move || SlowKid(gate.clone()))
                    .restart_type(RestartType::Temporary)
                    .await?;
                let kid = ctx
                    .spawn_child(|| Victim)
                    .restart_type(RestartType::Temporary)
                    .await?;
                Ok(Some(kid))
            }
            Cmd::Wait(gate) => {
                gate.pass().await;
                Ok(None)
            }
            Cmd::Note(_) => {
                if let Some(gate) = &self.holds.stage {
                    gate.pass().await;
                }
                Ok(None)
            }
            Cmd::Never => Ok(None),
        }
    }

    async fn on_child_stopped(
        &mut self,
        _event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if let Some(gate) = &self.holds.final_report {
            gate.pass().await;
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if let Some(gate) = &self.holds.on_stopped {
            gate.pass().await;
        }
        Ok(())
    }
}

/// Whether a map function of `kind` can run in `window`. A monitor stage
/// closes every peer monitor and every link when it starts, so a watched
/// actor or a linked peer that ends later is owed nothing and no map runs. A
/// group monitor's task runs until the actor's mailbox closes, which is right
/// after `on_stopped` returns: the task then ends without running its map.
fn can_land(kind: Kind, window: Window) -> bool {
    match kind {
        Kind::Group => window <= Window::OnStopped,
        Kind::Peer | Kind::Link => window <= Window::FinalDone,
    }
}

/// The exit reason a row must end with.
fn expected(kind: Kind, window: Window, variant: Variant) -> StopReason {
    let first = StopReason::Failure(ActorError::user("first"));
    match (variant, can_land(kind, window)) {
        (Variant::KillAfter, _) => StopReason::Kill,
        (Variant::FailureStands, _) => first,
        (Variant::Plain, true) => StopReason::Failure(ActorError::Panic("map boom".into())),
        (Variant::Plain, false) => StopReason::Graceful,
    }
}

async fn run_row(kind: Kind, window: Window, variant: Variant) -> Result<(), String> {
    let system = ActorSystem::create(uname("system")).expect("a fresh system name");
    let scope = system.pg();
    let user_gate = Gate::new();
    let mut holds = Holds::default();
    match window {
        Window::FinalReport => holds.final_report = Some(user_gate.clone()),
        Window::Stage => holds.stage = Some(user_gate.clone()),
        Window::OnStopped => holds.on_stopped = Some(user_gate.clone()),
        Window::ChildStop => holds.child_stop = Some(user_gate.clone()),
        _ => {}
    }
    let w = Watcher { holds }
        .spawn()
        .named(uname("watcher"))
        .on_system(&system)
        .supervisor()
        .await
        .unwrap();
    let id = w.id().clone();
    let gate = match window {
        Window::LoopEnded => end_hooks::attach_gate(end_hooks::LOOP_ENDED, id),
        Window::FinalDone => end_hooks::attach_gate(end_hooks::FINAL_DONE, id),
        Window::StageDone => end_hooks::attach_gate(end_hooks::STAGE_DONE, id),
        Window::TeardownDone => end_hooks::attach_gate(end_hooks::TEARDOWN_DONE, id),
        _ => user_gate,
    };
    let v1 = Victim
        .spawn()
        .named(uname("v1"))
        .on_system(&system)
        .await
        .unwrap();
    let v2 = Victim
        .spawn()
        .named(uname("v2"))
        .on_system(&system)
        .await
        .unwrap();
    let member = Victim
        .spawn()
        .named(uname("m"))
        .on_system(&system)
        .await
        .unwrap();
    let kid = within(
        "the setup",
        w.send(Cmd::Setup {
            kind,
            v1: v1.clone(),
            v2: v2.clone(),
            scope: scope.clone(),
        }),
    )
    .await
    .unwrap()
    .expect("the child");

    // The watcher is parked in a turn, so the child's death report and the
    // second notice wait for the loop, which reads the stop request first.
    let parked = Gate::new();
    let turn = {
        let (w, parked) = (w.clone(), parked.clone());
        tokio::spawn(async move {
            let _ = w.send(Cmd::Wait(parked)).await;
        })
    };
    within("the park", parked.reached.notified()).await;
    kid.stop(StopReason::Graceful).await.unwrap();
    kid.wait_stopped().await;
    v2.stop(StopReason::Graceful).await.unwrap();
    v2.wait_stopped().await;
    until("the second notice to queue", || w.mailbox_len() != 0).await;
    let base = match variant {
        Variant::FailureStands => StopReason::Failure(ActorError::user("first")),
        _ => StopReason::Graceful,
    };
    w.stop(base).await.unwrap();
    parked.release.add_permits(1);
    let _ = turn.await;

    within("the window", gate.reached.notified()).await;
    land(kind, window, &w, &v1, &member, &scope).await;
    if variant == Variant::KillAfter {
        w.stop(StopReason::Kill).await.unwrap();
    }
    gate.release.add_permits(1);
    let ended = within("the end", w.wait_stopped()).await;
    let want = expected(kind, window, variant);
    if ended == want {
        Ok(())
    } else {
        Err(format!(
            "{kind:?} in {window:?} ({variant:?}): ended {ended:?}, wanted {want:?}"
        ))
    }
}

/// Makes a map function of `kind` panic now, and waits until it did.
async fn land(
    kind: Kind,
    window: Window,
    w: &ActorHandle<Watcher>,
    v1: &ActorHandle<Victim>,
    member: &ActorHandle<Victim>,
    scope: &PgScope,
) {
    let lane = w.stop_lane();
    match kind {
        Kind::Peer => {
            v1.stop(StopReason::Graceful).await.unwrap();
            v1.wait_stopped().await;
        }
        Kind::Group => scope.join("g", member).unwrap(),
        Kind::Link => {
            v1.stop(StopReason::Failure(ActorError::user("boom")))
                .await
                .unwrap();
            v1.wait_stopped().await;
        }
    }
    if !can_land(kind, window) {
        sleep(SETTLE).await;
        return;
    }
    until(
        &format!("the map's panic to be kept ({kind:?} in {window:?})"),
        || lane.abort_record().map_panic().is_some(),
    )
    .await;
}

async fn the_table() {
    let failures = Arc::new(Mutex::new(Vec::new()));
    let mut rows = 0;
    for kind in [Kind::Peer, Kind::Group, Kind::Link] {
        for window in WINDOWS {
            let variants: &[Variant] = match kind {
                _ if can_land(kind, window) => {
                    &[Variant::Plain, Variant::FailureStands, Variant::KillAfter]
                }
                _ => &[Variant::Plain],
            };
            for &variant in variants {
                rows += 1;
                if let Err(row) = run_row(kind, window, variant).await {
                    failures.lock().unwrap().push(row);
                }
            }
        }
    }
    let failures = failures.lock().unwrap();
    assert!(
        failures.is_empty(),
        "{} of {rows} rows failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A map function of each kind that panics in each window after the loop
/// ended makes the exit reason a `Failure` unless one already stands, and a
/// `Kill` stays a `Kill`. Run on a current-thread runtime.
#[test]
fn a_late_map_panic_in_every_window_on_a_current_thread_runtime() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(the_table());
}
