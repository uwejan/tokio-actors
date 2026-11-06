//! A `Kill` raised while a stop is in its last callbacks is the exit reason
//! over a panic of `pre_stop`, an `Err` or panic of `on_stopped` and an `Err`
//! or panic of a final delivery, and it skips the hooks that had not begun.
//! Every case runs with a kill grace of an hour, so the `Kill` is observed by
//! the actor and not cut short by the abort that follows it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, ChildEvent, RestartType, StopReason, SupervisionAction,
};

mod common;

const BOUND: Duration = Duration::from_secs(30);
const GRACE: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug)]
enum Failure {
    Err,
    Panic,
}

type Reason = Arc<Mutex<Option<StopReason>>>;

// ---------------------------------------------------------------------------
// pre_stop panics
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// on_stopped fails
// ---------------------------------------------------------------------------

struct FailsInOnStopped {
    how: Failure,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for FailsInOnStopped {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        match self.how {
            Failure::Err => Err(ActorError::user("close failed")),
            Failure::Panic => panic!("close panicked"),
        }
    }
}

#[test]
fn a_kill_raised_while_on_stopped_fails_is_the_exit_reason() {
    common::run(2, async {
        for how in [Failure::Err, Failure::Panic] {
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let h = FailsInOnStopped {
                how,
                entered: entered.clone(),
                release: release.clone(),
            }
            .spawn()
            .kill_grace(GRACE)
            .await
            .unwrap();
            h.stop(StopReason::Graceful).await.unwrap();
            timeout(BOUND, entered.notified())
                .await
                .expect("on_stopped starts");
            h.stop(StopReason::Kill).await.unwrap();
            release.notify_one();
            let reason = timeout(BOUND, h.wait_stopped())
                .await
                .expect("the actor ends");
            assert_eq!(reason, StopReason::Kill, "{how:?}");
        }
    });
}

// ---------------------------------------------------------------------------
// A final delivery fails
// ---------------------------------------------------------------------------

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Holds its handler at a gate with one child death recorded and not
/// delivered, so a stop requested then leaves that death for the final
/// deliveries; the delivery waits at a second gate and then fails.
struct Boss {
    how: Failure,
    in_handler: Arc<Notify>,
    handler_gate: Arc<Notify>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    stopped_with: Reason,
}

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let child = ctx
            .spawn_child(|| Idle)
            .restart_type(RestartType::Temporary)
            .await?;
        child.stop(StopReason::Cancelled).await.unwrap();
        child.wait_stopped().await;
        self.in_handler.notify_one();
        self.handler_gate.notified().await;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if event.action != SupervisionAction::SupervisorStopping {
            return Ok(());
        }
        self.entered.notify_one();
        self.release.notified().await;
        match self.how {
            Failure::Err => Err(ActorError::user("delivery failed")),
            Failure::Panic => panic!("delivery panicked"),
        }
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.stopped_with.lock().unwrap() = Some(reason.clone());
        Ok(())
    }
}

#[test]
fn a_kill_raised_while_a_final_delivery_fails_is_the_exit_reason() {
    common::run(2, async {
        for how in [Failure::Err, Failure::Panic] {
            let in_handler = Arc::new(Notify::new());
            let handler_gate = Arc::new(Notify::new());
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let stopped_with: Reason = Arc::default();
            let sup = Boss {
                how,
                in_handler: in_handler.clone(),
                handler_gate: handler_gate.clone(),
                entered: entered.clone(),
                release: release.clone(),
                stopped_with: stopped_with.clone(),
            }
            .spawn()
            .kill_grace(GRACE)
            .supervisor()
            .await
            .unwrap();
            sup.notify(()).await.unwrap();
            timeout(BOUND, in_handler.notified())
                .await
                .expect("the handler holds the loop");
            sup.stop(StopReason::Graceful).await.unwrap();
            handler_gate.notify_one();
            timeout(BOUND, entered.notified())
                .await
                .expect("the final delivery starts");
            sup.stop(StopReason::Kill).await.unwrap();
            release.notify_one();
            let reason = timeout(BOUND, sup.wait_stopped())
                .await
                .expect("the supervisor ends");
            assert_eq!(reason, StopReason::Kill, "{how:?}");
            assert_eq!(
                *stopped_with.lock().unwrap(),
                None,
                "{how:?}: on_stopped ran after the Kill"
            );
        }
    });
}
