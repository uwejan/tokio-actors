//! The outcome a caller reads when an actor value's `Drop` outlasts the
//! caller's first window: the `Kill` step's raise is refused (the lane is
//! sealed) or never made (the entry is gone), and the actor still ends with
//! its own reason.
//!
//! Each window is long enough for the actor to reach its seal first, which
//! each test confirms by observation (the value's drop has begun, so the
//! lane is sealed) before the window ends, the window's start being read
//! inside the call's own task. The drop is then held until a clock margin
//! past the window's end, so the call's next step has begun. The value's
//! drop blocks its thread through `block_in_place`, which only a
//! multi_thread runtime provides, so every test runs on multi_thread only.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorSystem, ChildEvent, RestartType, SendError,
    ShutdownPolicy, StopOutcome, StopReason,
};

const BOUND: Duration = Duration::from_secs(10);
/// A caller's first window.
const WINDOW: Duration = Duration::from_millis(300);
/// How long past a window's end a held drop is kept, so the caller's next
/// step has begun.
const PAST: Duration = Duration::from_millis(200);

/// Fails if the actor reached its seal (its value's drop has begun) more
/// than `BOUND` after `start`, a hang check, then waits until `extra` past
/// the end of the window that began at `start`. Whether the seal came inside
/// the window shows in the outcome each test asserts.
async fn hold_past_the_window(start: Instant, extra: Duration) {
    assert!(start.elapsed() < BOUND, "the actor never reached its seal");
    tokio::time::sleep_until(start + WINDOW + extra).await;
}
static UNIQ: AtomicU64 = AtomicU64::new(0);
fn uname(base: &str) -> String {
    format!(
        "slow-drop-outcome-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

struct Held {
    entered: Option<oneshot::Sender<()>>,
    release: Option<std::sync::mpsc::Receiver<()>>,
}
impl Drop for Held {
    fn drop(&mut self) {
        if let Some(e) = self.entered.take() {
            let _ = e.send(());
        }
        if let Some(r) = self.release.take() {
            tokio::task::block_in_place(|| {
                let _ = r.recv();
            });
        }
    }
}
impl Actor for Held {
    type Message = ();
    type Response = ();
    async fn handle(&mut self, _m: (), _c: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn held() -> (Held, oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (etx, erx) = oneshot::channel();
    let (rtx, rrx) = std::sync::mpsc::channel();
    (
        Held {
            entered: Some(etx),
            release: Some(rrx),
        },
        erx,
        rtx,
    )
}

async fn root(sys: &Arc<ActorSystem>, v: Held, grace: Duration) -> ActorHandle<Held> {
    v.spawn()
        .named(uname("root"))
        .on_system(sys)
        .kill_grace(grace)
        .await
        .unwrap()
}

/// `stop_within(Graceful)`: the window ends inside the value's drop, the
/// `Kill` step's raise is refused (the lane is sealed), and the drop ends
/// inside the kill grace: the actor ended with its own reason, `Graceful`.
#[test]
fn a_refused_kill_step_leaves_stop_within_graceful() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let (v, entered, release) = held();
        let h = root(&sys, v, Duration::from_secs(5)).await;
        let (start_tx, start_rx) = oneshot::channel();
        let call = tokio::spawn({
            let h = h.clone();
            async move {
                let _ = start_tx.send(Instant::now());
                h.stop_within(StopReason::Graceful, WINDOW).await
            }
        });
        timeout(BOUND, entered).await.unwrap().unwrap();
        let start = start_rx.await.expect("the call began");
        assert!(matches!(
            h.stop(StopReason::Kill).await,
            Err(SendError::Closed)
        ));
        hold_past_the_window(start, PAST).await;
        let _ = release.send(());
        let out = timeout(BOUND, call).await.unwrap().unwrap().unwrap();
        assert_eq!(out, StopOutcome::Graceful, "refused Kill read as {out:?}");
    });
}

/// Shutdown: the first step's window ends inside the value's drop; the entry
/// is gone, so no `Kill` is raised, and the drop ends inside the grace:
/// `Graceful`.
#[test]
fn a_shutdown_whose_kill_step_raises_nothing_reads_graceful() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let (v, entered, release) = held();
        let h = root(&sys, v, Duration::from_secs(5)).await;
        let id = h.id().clone();
        let (start_tx, start_rx) = oneshot::channel();
        let call = tokio::spawn({
            let sys = sys.clone();
            async move {
                let _ = start_tx.send(Instant::now());
                sys.shutdown_with(ShutdownPolicy {
                    timeout: BOUND,
                    per_actor_timeout: WINDOW,
                })
                .await
            }
        });
        timeout(BOUND, entered).await.unwrap().unwrap();
        let start = start_rx.await.expect("the call began");
        hold_past_the_window(start, PAST).await;
        let _ = release.send(());
        let rep = timeout(BOUND, call).await.unwrap().unwrap();
        let out = rep
            .outcomes
            .iter()
            .find(|(a, _)| *a == id)
            .map(|(_, o)| o.outcome);
        assert_eq!(
            out,
            Some(StopOutcome::Graceful),
            "no Kill raised, read {out:?}"
        );
    });
}

/// Shutdown: the `Kill` step waits the root's own 5 s kill grace, read when
/// the shutdown began, so a drop held 600 ms past the first window ends
/// inside it.
#[test]
fn the_shutdown_kill_step_waits_the_root_s_own_grace() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let (v, entered, release) = held();
        let h = root(&sys, v, Duration::from_secs(5)).await;
        let id = h.id().clone();
        let (start_tx, start_rx) = oneshot::channel();
        let call = tokio::spawn({
            let sys = sys.clone();
            async move {
                let _ = start_tx.send(Instant::now());
                sys.shutdown_with(ShutdownPolicy {
                    timeout: BOUND,
                    per_actor_timeout: WINDOW,
                })
                .await
            }
        });
        timeout(BOUND, entered).await.unwrap().unwrap();
        let start = start_rx.await.expect("the call began");
        // Well past the default kill grace, well inside the root's own.
        hold_past_the_window(start, Duration::from_millis(600)).await;
        let _ = release.send(());
        let rep = timeout(BOUND, call).await.unwrap().unwrap();
        let out = rep
            .outcomes
            .iter()
            .find(|(a, _)| *a == id)
            .map(|(_, o)| o.outcome);
        assert_eq!(
            out,
            Some(StopOutcome::Graceful),
            "own 5 s grace not used, read {out:?}"
        );
    });
}

// Supervised: the caller and the supervisor read one end.
struct Parent {
    slot: Arc<std::sync::Mutex<Option<Held>>>,
    child: String,
    reports: mpsc::UnboundedSender<StopReason>,
}
impl Actor for Parent {
    type Message = ();
    type Response = ();
    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let slot = self.slot.clone();
        ctx.spawn_child(move || {
            slot.lock().unwrap().take().unwrap_or(Held {
                entered: None,
                release: None,
            })
        })
        .named(self.child.clone())
        .restart_type(RestartType::Temporary)
        .kill_grace(Duration::from_secs(5))
        .await
        .map_err(ActorError::from)?;
        Ok(())
    }
    async fn handle(&mut self, _m: (), _c: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
    async fn on_child_stopped(
        &mut self,
        e: &ChildEvent,
        _c: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.reports.send(e.reason.clone());
        Ok(())
    }
}

/// `stop_within(Graceful)` on a supervised child whose value's drop
/// outlasts the window: the caller's outcome and the supervisor's reason
/// name the same end.
#[test]
fn the_caller_and_the_supervisor_read_one_end() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let (v, entered, release) = held();
        let child = uname("child");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _p = Parent {
            slot: Arc::new(std::sync::Mutex::new(Some(v))),
            child: child.clone(),
            reports: tx,
        }
        .spawn()
        .named(uname("parent"))
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        let h = timeout(BOUND, async {
            loop {
                if let Some(h) = sys.get::<Held>(&child) {
                    break h;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let (start_tx, start_rx) = oneshot::channel();
        let call = tokio::spawn({
            let h = h.clone();
            async move {
                let _ = start_tx.send(Instant::now());
                h.stop_within(StopReason::Graceful, WINDOW).await
            }
        });
        timeout(BOUND, entered).await.unwrap().unwrap();
        let start = start_rx.await.expect("the call began");
        hold_past_the_window(start, PAST).await;
        let _ = release.send(());
        let out = timeout(BOUND, call).await.unwrap().unwrap().unwrap();
        let reason = timeout(BOUND, rx.recv()).await.unwrap().unwrap();
        // The reason the supervisor reads for each outcome the caller reads.
        let one_end = match out {
            StopOutcome::Graceful => matches!(reason, StopReason::Graceful),
            StopOutcome::Killed | StopOutcome::Aborted => matches!(reason, StopReason::Kill),
            StopOutcome::Unresponsive => false,
        };
        assert!(
            one_end,
            "two verdicts: caller {out:?}, supervisor {reason:?}"
        );
    });
}

/// Shutdown cut by its global deadline while a root's value drops: the
/// sweep never reports the root before its end. While the drop is held past
/// the root's kill grace and the short wait after it, the root is
/// `Unresponsive` and its status is not `Stopped`.
#[test]
fn the_sweep_reports_a_gone_root_only_after_its_end() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(uname("sys")).unwrap();
        let (v, entered, release) = held();
        let h = root(&sys, v, Duration::from_millis(50)).await;
        let id = h.id().clone();
        let (start_tx, start_rx) = oneshot::channel();
        let call = tokio::spawn({
            let sys = sys.clone();
            async move {
                let _ = start_tx.send(Instant::now());
                sys.shutdown_with(ShutdownPolicy {
                    timeout: WINDOW,
                    per_actor_timeout: Duration::from_secs(5),
                })
                .await
            }
        });
        timeout(BOUND, entered).await.unwrap().unwrap();
        let start = start_rx.await.expect("the call began");
        assert!(start.elapsed() < BOUND, "the actor never reached its seal");
        // The drop is held until the report is in.
        let rep = timeout(BOUND, call).await.unwrap().unwrap();
        let status = h.status();
        let out = rep
            .outcomes
            .iter()
            .find(|(a, _)| *a == id)
            .map(|(_, o)| o.outcome);
        assert_ne!(
            status,
            tokio_actors::ActorStatus::Stopped,
            "the drop is held"
        );
        assert_eq!(out, Some(StopOutcome::Unresponsive), "reported while held");
        let _ = release.send(());
        within_bound_stopped(&h).await;
    });
}

/// Waits until the actor publishes `Stopped`.
async fn within_bound_stopped(h: &ActorHandle<Held>) {
    timeout(BOUND, h.wait_stopped())
        .await
        .expect("the actor ends once its drop is released");
}
