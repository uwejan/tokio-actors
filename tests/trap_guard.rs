#![allow(dead_code)]
//! A worker under a supervisor that is killed: with its trap flag set it is
//! stopped in order, bounded by its own `Shutdown`, and runs `on_stopped`;
//! without it, it is killed at once. The flag is read when the
//! supervisor's end reaches the worker, so a toggle just before it counts.
//!
//! The tests use real supervisors and workers. A `Worker` records the reason
//! its `on_stopped` received; a slow one stays in `on_stopped` until it is cut.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorHandle, ActorResult, ActorSystem, RestartType, Shutdown, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);
const WINDOW: Duration = Duration::from_millis(200);
const GRACE: Duration = Duration::from_millis(120);
/// How far past its exact instant an outcome may land on the paused clock.
const PAUSED_SLACK: Duration = Duration::from_millis(40);

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

fn system() -> Arc<ActorSystem> {
    ActorSystem::create(format!("tg-{}", uuid::Uuid::new_v4())).unwrap()
}

enum Msg {
    Trap,
    Untrap,
    Ping,
}

/// What a node's `on_stopped` saw.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<StopReason>>>);

impl Seen {
    fn reasons(&self) -> Vec<StopReason> {
        self.0.lock().unwrap().clone()
    }
}

/// A worker, or a supervisor child that has a worker of its own.
struct Node {
    seen: Seen,
    /// Stays in `on_stopped` until the task is cut.
    slow: bool,
    /// Spawns a worker below it, making this node a supervisor.
    child: Option<&'static str>,
}

impl Actor for Node {
    type Message = Msg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(name) = self.child {
            let seen = self.seen.clone();
            ctx.spawn_child(move || Node {
                seen: seen.clone(),
                slow: false,
                child: None,
            })
            .named(name)
            .restart_type(RestartType::Permanent)
            .await?;
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Trap => ctx.trap_exit(|_| Msg::Ping),
            Msg::Untrap => ctx.untrap_exit(),
            Msg::Ping => {}
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.seen.0.lock().unwrap().push(reason.clone());
        if self.slow {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        Ok(())
    }
}

/// The supervisor of one child `name`, whose `Shutdown` is `WINDOW`.
struct Boss {
    seen: Seen,
    slow: bool,
    supervisor_child: bool,
}

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (seen, slow) = (self.seen.clone(), self.slow);
        let supervisor_child = self.supervisor_child;
        let mut config = ActorConfig::default();
        if supervisor_child {
            config = config.supervisor();
        }
        ctx.spawn_child(move || Node {
            seen: seen.clone(),
            slow,
            child: supervisor_child.then_some("grandchild"),
        })
        .named("worker")
        .shutdown(Shutdown::Timeout(WINDOW))
        .kill_grace(GRACE)
        .restart_type(RestartType::Permanent)
        .with_config(config)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

struct Rig {
    sys: Arc<ActorSystem>,
    boss: ActorHandle<Boss>,
    worker: ActorHandle<Node>,
    seen: Seen,
}

async fn rig(slow: bool, supervisor_child: bool) -> Rig {
    let sys = system();
    let seen = Seen::default();
    let boss = Boss {
        seen: seen.clone(),
        slow,
        supervisor_child,
    }
    .spawn()
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();
    let worker = sys.get::<Node>("worker").expect("the worker is registered");
    Rig {
        sys,
        boss,
        worker,
        seen,
    }
}

impl Rig {
    /// Kills the supervisor and returns the worker's reason and how long it
    /// took to end.
    async fn kill_the_supervisor(&self) -> (StopReason, Duration) {
        let started = Instant::now();
        self.boss.stop(StopReason::Kill).await.unwrap();
        let reason = within("the worker", self.worker.wait_stopped()).await;
        (reason, started.elapsed())
    }
}

#[test]
fn non_trapping_worker_still_gets_kill() {
    common::run_paused(async {
        let rig = rig(true, false).await;
        let (reason, elapsed) = rig.kill_the_supervisor().await;
        assert_eq!(reason, StopReason::Kill);
        assert!(
            rig.seen.reasons().is_empty(),
            "a worker that does not trap runs no on_stopped"
        );
        assert!(
            elapsed < GRACE + PAUSED_SLACK,
            "and ends at once, not at its window: {elapsed:?}"
        );
    });
}

#[test]
fn toggle_just_before_the_supervisors_death() {
    common::run(2, async {
        // Rows: the toggles sent to the worker, and what the supervisor's end
        // does to it.
        let rows: [(&[bool], StopReason, bool); 4] = [
            (&[true], StopReason::ParentKilled, true),
            (&[true, false], StopReason::Kill, false),
            (&[false], StopReason::Kill, false),
            (&[false, true], StopReason::ParentKilled, true),
        ];
        for (toggles, want, on_stopped_runs) in rows {
            let rig = rig(false, false).await;
            for on in toggles {
                let msg = if *on { Msg::Trap } else { Msg::Untrap };
                rig.worker.send(msg).await.unwrap();
            }
            let (reason, _) = rig.kill_the_supervisor().await;
            assert_eq!(reason, want, "toggles {toggles:?}");
            assert_eq!(
                !rig.seen.reasons().is_empty(),
                on_stopped_runs,
                "toggles {toggles:?}"
            );
        }
    });
}
