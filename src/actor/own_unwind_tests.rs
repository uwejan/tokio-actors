//! An actor task whose own poll unwinds outside every catch. Every call of
//! user code and every log line is caught, so the unwind is forced from the
//! crate's own code through a test hook: the registry guard's release of the
//! name, at the end of the task, panics, or a supervisor's teardown walk
//! panics while the supervisor still holds a live child. The actor's status
//! plane, its supervisor's report of its end, the restart attempt that joins
//! it and an awaited spawn whose initialization the unwind cut all read one
//! reason, `Failure(Panic(..))`, the reason of an uncaught panic. With the
//! `tracing` feature, the task that joins the unwound one logs the unwind's
//! payload once. A live child of an unwound supervisor is killed, as the
//! worker of a killed parent is.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::actor::context::ActorContext;
use crate::actor::runtime::{end_hooks, ActorConfig, UNCAUGHT_PANIC};
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult, SpawnError};
use crate::system::ActorSystem;
use crate::types::{ActorId, ChildEvent, RestartType, StopReason};

const BOUND: Duration = Duration::from_secs(10);

/// The text of the panic the hook raises in the task's own poll.
const HOOK_PANIC: &str = "the crate's own code panics in the task's poll";

fn uncaught() -> StopReason {
    StopReason::Failure(ActorError::Panic(UNCAUGHT_PANIC.to_string()))
}

fn unique(base: &str) -> String {
    format!("own-unwind-{base}-{}", uuid::Uuid::new_v4())
}

/// Makes the release of `name` panic at the end of its task.
fn panic_at_the_end_of(name: &str) {
    end_hooks::attach(end_hooks::NAME, ActorId::from(name), || {
        panic!("{HOOK_PANIC}")
    });
}

/// What a [`Mid`] is asked.
enum MidMsg {
    /// Answers once its initialization finished.
    Ping,
    /// Fails the turn.
    Fail,
    /// Starts a [`Leaf`] child of that name.
    Leaf(String),
}

/// A worker that only answers.
struct Leaf;

impl Actor for Leaf {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Takes one from `left` when it is above zero, and says whether it did.
fn take_one(left: &AtomicUsize) -> bool {
    let mut seen = left.load(Ordering::SeqCst);
    while seen > 0 {
        match left.compare_exchange_weak(seen, seen - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => seen = now,
        }
    }
    false
}

/// Fails its initialization while `fail_init` counts down.
struct Mid {
    fail_init: Arc<AtomicUsize>,
}

impl Actor for Mid {
    type Message = MidMsg;
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let fail = take_one(&self.fail_init);
        if fail {
            return Err(ActorError::user("mid fails its initialization"));
        }
        Ok(())
    }

    async fn handle(&mut self, msg: MidMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            MidMsg::Ping => Ok(()),
            MidMsg::Fail => Err(ActorError::user("mid fails its turn")),
            MidMsg::Leaf(name) => {
                ctx.spawn_child(|| Leaf).named(name).await?;
                Ok(())
            }
        }
    }
}

/// Supervises one [`Mid`] and reports every end of it.
struct Top {
    mid: String,
    restart: RestartType,
    /// Whether [`Mid`] is itself a supervisor.
    mid_supervises: bool,
    fail_init: Arc<AtomicUsize>,
    events: mpsc::UnboundedSender<ChildEvent>,
}

impl Actor for Top {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let fail_init = self.fail_init.clone();
        let spawn = ctx
            .spawn_child(move || Mid {
                fail_init: fail_init.clone(),
            })
            .named(self.mid.clone())
            .restart_type(self.restart);
        if self.mid_supervises {
            spawn
                .with_config(ActorConfig::default().supervisor())
                .await?;
        } else {
            spawn.await?;
        }
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.events.send(event.clone());
        Ok(())
    }
}

/// A supervisor and its [`Mid`] child.
struct Rig {
    system: Arc<ActorSystem>,
    mid: String,
    fail_init: Arc<AtomicUsize>,
    events: mpsc::UnboundedReceiver<ChildEvent>,
}

async fn rig(restart: RestartType) -> Rig {
    rig_of(restart, false).await
}

/// [`rig`], with [`Mid`] a supervisor when `mid_supervises`.
async fn rig_of(restart: RestartType, mid_supervises: bool) -> Rig {
    let system = ActorSystem::create(unique("system")).unwrap();
    let mid = unique("mid");
    let fail_init = Arc::new(AtomicUsize::new(0));
    let (tx, events) = mpsc::unbounded_channel();
    let top = Top {
        mid: mid.clone(),
        restart,
        mid_supervises,
        fail_init: fail_init.clone(),
        events: tx,
    }
    .spawn()
    .named(unique("top"))
    .on_system(&system)
    .supervisor()
    .await
    .expect("top starts");
    top.send(()).await.expect("mid starts");
    Rig {
        system,
        mid,
        fail_init,
        events,
    }
}

/// A child whose own poll unwinds at the end of its stop: its plane and its
/// supervisor's report of its end read one panic. Returns the child's name.
async fn a_child_unwinding_from_its_own_poll_reads_one_panic() -> String {
    let mut rig = rig(RestartType::Temporary).await;
    let mid = rig.system.get::<Mid>(&rig.mid).expect("mid is registered");
    mid.send(MidMsg::Ping).await.expect("mid answers");
    panic_at_the_end_of(&rig.mid);
    mid.stop(StopReason::Graceful).await.expect("accepted");
    let plane = timeout(BOUND, mid.wait_stopped()).await.expect("mid ends");
    let event = timeout(BOUND, rig.events.recv())
        .await
        .expect("top's report of mid")
        .expect("a report");
    assert_eq!(plane, uncaught(), "the plane reads the unwind");
    assert_eq!(event.reason, plane, "the report reads the plane's reason");
    rig.mid
}

#[tokio::test(flavor = "current_thread")]
async fn a_child_unwinding_from_its_own_poll_reads_one_panic_current_thread() {
    a_child_unwinding_from_its_own_poll_reads_one_panic().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_unwinding_from_its_own_poll_reads_one_panic_multi_thread() {
    a_child_unwinding_from_its_own_poll_reads_one_panic().await;
}

/// An actor whose initialization failed unwinds at the end of its task,
/// before it answers its awaited spawn: the spawn reads the panic its plane
/// carries, as `SpawnError::Init`.
async fn an_unwind_during_init_answers_the_awaited_spawn_with_the_planes_panic() {
    let system = ActorSystem::create(unique("system")).unwrap();
    let name = unique("mid");
    panic_at_the_end_of(&name);
    let spawned = Mid {
        fail_init: Arc::new(AtomicUsize::new(1)),
    }
    .spawn()
    .named(name)
    .on_system(&system)
    .await;
    match spawned {
        Err(SpawnError::Init(err)) => {
            assert_eq!(*err, ActorError::Panic(UNCAUGHT_PANIC.to_string()));
        }
        Err(other) => panic!("expected Init with the plane's panic, got {other:?}"),
        Ok(_) => panic!("expected the spawn to fail"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_unwind_during_init_answers_the_awaited_spawn_with_the_planes_panic_current_thread() {
    an_unwind_during_init_answers_the_awaited_spawn_with_the_planes_panic().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unwind_during_init_answers_the_awaited_spawn_with_the_planes_panic_multi_thread() {
    an_unwind_during_init_answers_the_awaited_spawn_with_the_planes_panic().await;
}

/// A restarted incarnation fails its initialization and unwinds at the end
/// of its task, before it answers its restart attempt: the attempt reads
/// the reason the incarnation's plane carries, and the supervisor's report
/// of the failed restart carries it. Returns the child's name.
async fn a_restarted_incarnation_unwinding_before_its_ack_reports_the_planes_reason() -> String {
    let mut rig = rig(RestartType::Permanent).await;
    let mid = rig.system.get::<Mid>(&rig.mid).expect("mid is registered");
    mid.send(MidMsg::Ping).await.expect("mid answers");
    rig.fail_init.store(1, Ordering::SeqCst);
    // The first incarnation's end passes the hook on to the next one, whose
    // end then panics.
    let name = rig.mid.clone();
    end_hooks::attach(
        end_hooks::NAME,
        ActorId::from(rig.mid.as_str()),
        move || {
            panic_at_the_end_of(&name);
        },
    );
    let _ = mid.send(MidMsg::Fail).await;
    let crash = timeout(BOUND, rig.events.recv())
        .await
        .expect("top's report of mid's crash")
        .expect("a report");
    assert_eq!(
        crash.reason,
        StopReason::Failure(ActorError::user("mid fails its turn"))
    );
    let restart = timeout(BOUND, rig.events.recv())
        .await
        .expect("top's report of the failed restart")
        .expect("a report");
    assert_eq!(
        restart.reason,
        uncaught(),
        "the attempt reads the incarnation's plane"
    );
    rig.mid
}

#[tokio::test(flavor = "current_thread")]
async fn a_restarted_incarnation_unwinding_before_its_ack_reports_the_planes_reason_current_thread()
{
    a_restarted_incarnation_unwinding_before_its_ack_reports_the_planes_reason().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_incarnation_unwinding_before_its_ack_reports_the_planes_reason_multi_thread() {
    a_restarted_incarnation_unwinding_before_its_ack_reports_the_planes_reason().await;
}

/// A supervisor's own poll unwinds in its teardown walk, while it still
/// holds a live child: its plane and its supervisor's report of its end read
/// the panic, and the child, a worker, is killed when the unwound
/// supervisor's state drops, as the worker of a killed parent is.
async fn a_supervisor_unwinding_in_its_teardown_walk_kills_its_live_child() {
    let mut rig = rig_of(RestartType::Temporary, true).await;
    let mid = rig.system.get::<Mid>(&rig.mid).expect("mid is registered");
    let leaf_name = unique("leaf");
    mid.send(MidMsg::Leaf(leaf_name.clone()))
        .await
        .expect("the leaf starts");
    let leaf = rig
        .system
        .get::<Leaf>(&leaf_name)
        .expect("the leaf is registered");
    end_hooks::attach(end_hooks::WALK, ActorId::from(rig.mid.as_str()), || {
        panic!("{HOOK_PANIC}")
    });
    mid.stop(StopReason::Graceful).await.expect("accepted");
    let plane = timeout(BOUND, mid.wait_stopped()).await.expect("mid ends");
    let event = timeout(BOUND, rig.events.recv())
        .await
        .expect("top's report of mid")
        .expect("a report");
    let fate = timeout(BOUND, leaf.wait_stopped())
        .await
        .expect("the leaf ends");
    assert_eq!(plane, uncaught(), "the plane reads the unwind");
    assert_eq!(event.reason, plane, "the report reads the plane's reason");
    assert_eq!(fate, StopReason::Kill, "the live child is killed");
    assert!(
        rig.system.get::<Leaf>(&leaf_name).is_none(),
        "the child's name is free once it ended"
    );
    assert!(
        rig.system.get::<Mid>(&rig.mid).is_none(),
        "the supervisor's name is free once it ended"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_supervisor_unwinding_in_its_teardown_walk_kills_its_live_child_current_thread() {
    a_supervisor_unwinding_in_its_teardown_walk_kills_its_live_child().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supervisor_unwinding_in_its_teardown_walk_kills_its_live_child_multi_thread() {
    a_supervisor_unwinding_in_its_teardown_walk_kills_its_live_child().await;
}

/// The warnings that name `child` and carry the hook's panic as an unwind's
/// payload, read from the process-wide capture other tests share: every
/// test names its own child, so a sibling test's lines never count here.
#[cfg(feature = "tracing")]
fn unwind_lines_of(lines: &std::sync::Mutex<Vec<String>>, child: &str) -> Vec<String> {
    let token = format!("child={child} ");
    lines
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(&token) && line.contains(HOOK_PANIC))
        .cloned()
        .collect()
}

/// A child's own unwind is logged once, with its payload, by the runner that
/// joins its task.
#[cfg(feature = "tracing")]
#[tokio::test(flavor = "current_thread")]
async fn the_joiner_logs_a_child_s_own_unwind_once() {
    let lines = crate::actor::supervision::lost_runner::capture::install();
    let child = a_child_unwinding_from_its_own_poll_reads_one_panic().await;
    let logged = unwind_lines_of(&lines, &child);
    assert_eq!(logged.len(), 1, "one line carries the payload: {logged:?}");
    assert!(
        logged[0].contains("panicked outside every catch"),
        "{logged:?}"
    );
}

/// A restarted incarnation's own unwind is logged once, with its payload,
/// by the restart attempt that joins its task.
#[cfg(feature = "tracing")]
#[tokio::test(flavor = "current_thread")]
async fn the_restart_attempt_logs_an_incarnation_s_own_unwind_once() {
    let lines = crate::actor::supervision::lost_runner::capture::install();
    let child = a_restarted_incarnation_unwinding_before_its_ack_reports_the_planes_reason().await;
    let logged = unwind_lines_of(&lines, &child);
    assert_eq!(logged.len(), 1, "one line carries the payload: {logged:?}");
    assert!(
        logged[0].contains("panicked outside every catch"),
        "{logged:?}"
    );
}
