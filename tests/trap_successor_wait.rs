//! The restart of a killed supervisor waits for a trapping worker's whole
//! orderly stop. A worker that traps exit signals is stopped in order within
//! its own `Shutdown` window when its supervisor is killed, so the bound of
//! the wait on the old subtree covers that window; a worker that does not
//! trap is killed within its kill grace and adds nothing.
//!
//! The tests use real supervisors and workers on a real clock. A `Worker`
//! logs when it starts and when its `on_stopped` begins and ends; a `Mid`
//! supervisor and its restarts log their starts.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorResult, ActorSystem, RestartType, Shutdown, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(15);
/// The worker's `Shutdown` window.
const WINDOW: Duration = Duration::from_secs(4);
/// How long a trapping worker's `on_stopped` takes: most of its window, and
/// far more than its kill grace and the settle time together.
const HOLD: Duration = Duration::from_millis(3000);
const GRACE: Duration = Duration::from_millis(50);

#[derive(Clone, Default)]
struct Tape {
    log: Arc<Mutex<Vec<String>>>,
    /// The next worker to start crashes once, so its supervisor restarts it.
    crash_once: Arc<AtomicBool>,
}

impl Tape {
    fn put(&self, what: impl Into<String>) {
        self.log.lock().unwrap().push(what.into());
    }

    fn all(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn count(&self, what: &str) -> usize {
        self.all()
            .iter()
            .filter(|item| item.as_str() == what)
            .count()
    }

    /// The position of the `nth` (1-based) entry equal to `what`.
    fn nth(&self, what: &str, nth: usize) -> Option<usize> {
        self.all()
            .iter()
            .enumerate()
            .filter(|(_, item)| item.as_str() == what)
            .nth(nth - 1)
            .map(|(at, _)| at)
    }
}

async fn until(what: &str, tape: &Tape, mut ready: impl FnMut(&Tape) -> bool) {
    let deadline = Instant::now() + BOUND;
    while !ready(tape) {
        if Instant::now() >= deadline {
            panic!("timed out waiting for: {what}: {:?}", tape.all());
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

struct Worker {
    tape: Tape,
    traps: bool,
}

impl Actor for Worker {
    /// `true` makes the worker fail.
    type Message = bool;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.traps {
            ctx.trap_exit(|_| false);
        }
        self.tape.put("worker started");
        if self.tape.crash_once.swap(false, Ordering::SeqCst) {
            ctx.self_handle().notify(true).await.ok();
        }
        Ok(())
    }

    async fn handle(&mut self, crash: bool, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if crash {
            return Err(ActorError::user("crash"));
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.tape.put(format!("worker on_stopped {reason:?}"));
        if self.traps && matches!(reason, StopReason::ParentKilled) {
            sleep(HOLD).await;
        }
        self.tape.put("worker finished");
        Ok(())
    }
}

/// A supervisor with one worker child.
struct Mid {
    tape: Tape,
    traps: bool,
    worker: String,
    shutdown: Shutdown,
}

impl Actor for Mid {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.tape.put("mid started");
        let (tape, traps) = (self.tape.clone(), self.traps);
        ctx.spawn_child(move || Worker {
            tape: tape.clone(),
            traps,
        })
        .named(self.worker.clone())
        .shutdown(self.shutdown)
        .kill_grace(GRACE)
        .restart_type(RestartType::Permanent)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// The supervisor above `Mid`: it restarts `Mid` when it is killed.
struct Top {
    tape: Tape,
    traps: bool,
    mid: String,
    worker: String,
    shutdown: Shutdown,
}

impl Actor for Top {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (tape, traps, worker) = (self.tape.clone(), self.traps, self.worker.clone());
        let shutdown = self.shutdown;
        ctx.spawn_child(move || Mid {
            tape: tape.clone(),
            traps,
            worker: worker.clone(),
            shutdown,
        })
        .named(self.mid.clone())
        .restart_type(RestartType::Permanent)
        .with_config(ActorConfig::default().supervisor())
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

struct Rig {
    sys: Arc<ActorSystem>,
    tape: Tape,
    top: tokio_actors::ActorHandle<Top>,
    mid: String,
}

async fn rig_with(traps: bool, shutdown: Shutdown) -> Rig {
    let run = uuid::Uuid::new_v4();
    let sys = ActorSystem::create(format!("tsw-{run}")).unwrap();
    let tape = Tape::default();
    let mid = format!("mid-{run}");
    let top = Top {
        tape: tape.clone(),
        traps,
        mid: mid.clone(),
        worker: format!("worker-{run}"),
        shutdown,
    }
    .spawn()
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();
    until("the worker to start", &tape, |t| {
        t.count("worker started") == 1
    })
    .await;
    Rig {
        sys,
        tape,
        top,
        mid,
    }
}

/// A killed supervisor's restart starts only after the trapping worker's whole
/// stop ended, and the supervisor above never gives up on the child.
async fn restart_waits_for_the_worker(shutdown: Shutdown) {
    let rig = rig_with(true, shutdown).await;
    within("the kill", rig.sys.kill(&rig.mid))
        .await
        .expect("accepted");
    until("the restarted supervisor", &rig.tape, |t| {
        t.count("mid started") == 2
    })
    .await;
    let done = rig
        .tape
        .nth("worker finished", 1)
        .expect("the old worker finished its on_stopped");
    let restarted = rig.tape.nth("mid started", 2).unwrap();
    assert!(
        done < restarted,
        "the restart starts only after the old worker's stop ended: {:?}",
        rig.tape.all()
    );
    assert!(
        rig.tape
            .all()
            .iter()
            .any(|what| what == "worker on_stopped ParentKilled"),
        "the worker was stopped in order: {:?}",
        rig.tape.all()
    );
    assert!(rig.top.is_alive(), "the supervisor above never gave up");
}

/// A trapping worker whose `on_stopped` takes most of its window: the
/// restarted supervisor starts only after the old worker finished, and the
/// supervisor above it never gives up on the child.
#[test]
fn a_restart_waits_for_a_trapping_workers_whole_stop() {
    common::run(0, async {
        restart_waits_for_the_worker(Shutdown::Timeout(WINDOW)).await;
    });
}

/// A trapping worker under `Shutdown::Infinity` is waited for as a supervisor
/// child under `Infinity` is: without a bound. Its `on_stopped` takes far
/// longer than its kill grace and the settle time, and the restart still
/// starts only after it finished.
#[test]
fn a_restart_waits_without_a_bound_for_a_trapping_worker_under_infinity() {
    common::run(0, async {
        restart_waits_for_the_worker(Shutdown::Infinity).await;
    });
}

/// A trapping worker under `Shutdown::Infinity` that its supervisor restarted
/// after a crash is counted as the first incarnation was: the restart of the
/// supervisor above waits for the restarted worker's whole stop. The count
/// belongs to the restarted worker's own link, which a restart builds apart
/// from the first spawn.
#[test]
fn a_restarted_trapping_worker_under_infinity_holds_the_next_restart() {
    common::run(0, async {
        let run = uuid::Uuid::new_v4();
        let sys = ActorSystem::create(format!("tsw-{run}")).unwrap();
        let tape = Tape::default();
        tape.crash_once.store(true, Ordering::SeqCst);
        let mid = format!("mid-{run}");
        let _top = Top {
            tape: tape.clone(),
            traps: true,
            mid: mid.clone(),
            worker: format!("worker-{run}"),
            shutdown: Shutdown::Infinity,
        }
        .spawn()
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        // The first worker crashes at once and its supervisor restarts it.
        until("the restarted worker", &tape, |t| {
            t.count("worker started") == 2
        })
        .await;
        within("the kill", sys.kill(&mid)).await.expect("accepted");
        until("the restarted supervisor", &tape, |t| {
            t.count("mid started") == 2
        })
        .await;
        let done = tape
            .nth("worker finished", 2)
            .expect("the restarted worker finished its on_stopped");
        let restarted = tape.nth("mid started", 2).unwrap();
        assert!(
            done < restarted,
            "the restart starts only after the restarted worker's stop ended: {:?}",
            tape.all()
        );
        assert!(
            tape.all()
                .iter()
                .any(|what| what == "worker on_stopped ParentKilled"),
            "the restarted worker was stopped in order: {:?}",
            tape.all()
        );
    });
}
