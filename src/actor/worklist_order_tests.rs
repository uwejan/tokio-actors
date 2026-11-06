//! The worklist of child reports hands the reports out in order. A manual
//! stop whose caller dropped the call before it saw the fate is finished by
//! the run loop when the child's report arrives: the report goes on through
//! the worklist at its front, so the reports queued behind it wait. Such a
//! report is only ever handled with others behind it when a worklist holds
//! more than one report, so the test hands the worklist both reports itself,
//! from inside a supervisor's turn, the way a completing group restart hands
//! it the reports it released.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::timeout;

use super::bounded_run::{self, Flavor};
use super::{handle_worklist, ChildLifecycle};
use crate::actor::context::ActorContext;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult};
use crate::system::ActorSystem;
use crate::types::{ActorId, ChildEvent, ChildStoppedInternal, RestartType, Shutdown, StopReason};

/// How many deliveries the supervisor keeps. A worklist that handled one
/// report over and over would otherwise fill the memory of the host.
const DELIVERIES_KEPT: usize = 16;

/// A worker that only answers.
struct Leaf;

impl Actor for Leaf {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A worker that refuses every orderly stop, so a manual stop of it stays
/// pending.
struct Stubborn;

impl Actor for Stubborn {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        false
    }
}

/// A supervisor that starts two workers and hands its worklist their reports.
struct Sup {
    /// The workers' names, the one whose stop is left uncommitted first.
    names: (String, String),
    delivered: Arc<Mutex<Vec<String>>>,
}

impl Actor for Sup {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (stopped, crashed) = self.names.clone();
        // A `Permanent` worker keeps its spec through a manual stop; a
        // `Temporary` one is removed with its end.
        ctx.spawn_child(|| Stubborn)
            .named(stopped.clone())
            .restart_type(RestartType::Permanent)
            .shutdown(Shutdown::Timeout(Duration::from_secs(30)))
            .await?;
        ctx.spawn_child(|| Leaf)
            .named(crashed.clone())
            .restart_type(RestartType::Temporary)
            .await?;
        // A caller commits a manual stop of the first worker and drops the
        // call before it sees the fate.
        let _ = timeout(Duration::ZERO, ctx.terminate_child(stopped.clone())).await;
        let (stopped_life, crashed_life) = {
            let sup = ctx.supervision_ref().expect("a supervisor");
            let life = |name: &str| sup.registry.get(&ActorId::from(name)).map(|c| c.lifecycle);
            (life(&stopped), life(&crashed))
        };
        let Some(ChildLifecycle::Stopping {
            incarnation: stopped_incarnation,
            ..
        }) = stopped_life
        else {
            panic!("the first worker is stopping, not {stopped_life:?}");
        };
        let Some(ChildLifecycle::Running(crashed_incarnation)) = crashed_life else {
            panic!("the second worker is running, not {crashed_life:?}");
        };
        let first = ChildStoppedInternal {
            child_id: ActorId::from(stopped.as_str()),
            reason: StopReason::Graceful,
            incarnation: stopped_incarnation,
        };
        let behind = ChildStoppedInternal {
            child_id: ActorId::from(crashed.as_str()),
            reason: StopReason::Failure(ActorError::user("the second worker failed")),
            incarnation: crashed_incarnation,
        };
        let mut work = VecDeque::from([behind]);
        let flow = handle_worklist(self, ctx, Some(first), &mut work).await;
        assert!(flow.is_continue(), "the worklist ran to its end: {flow:?}");
        assert!(work.is_empty(), "the worklist is empty: {work:?}");
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let mut delivered = self.delivered.lock().unwrap();
        if delivered.len() < DELIVERIES_KEPT {
            delivered.push(event.child_id.as_str().to_string());
        }
        Ok(())
    }
}

async fn a_report_pushed_back_is_handled_before_the_reports_queued_behind_it() {
    let system = ActorSystem::create(format!("worklist-order-{}", uuid::Uuid::new_v4())).unwrap();
    let stopped = format!("worklist-stopped-{}", uuid::Uuid::new_v4());
    let crashed = format!("worklist-crashed-{}", uuid::Uuid::new_v4());
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let sup = Sup {
        names: (stopped.clone(), crashed.clone()),
        delivered: delivered.clone(),
    }
    .spawn()
    .named(format!("worklist-sup-{}", uuid::Uuid::new_v4()))
    .on_system(&system)
    .supervisor()
    .await
    .expect("the supervisor starts");
    sup.send(()).await.expect("the supervisor's turn ends");
    assert_eq!(
        *delivered.lock().unwrap(),
        vec![stopped, crashed],
        "the report of the uncommitted stop is delivered first, then the one behind it"
    );
}

#[test]
fn the_report_of_an_uncommitted_stop_is_delivered_before_the_reports_behind_it_current_thread() {
    bounded_run::run(Flavor::CurrentThread, || {
        a_report_pushed_back_is_handled_before_the_reports_queued_behind_it()
    });
}

#[test]
fn the_report_of_an_uncommitted_stop_is_delivered_before_the_reports_behind_it_multi_thread() {
    bounded_run::run(Flavor::MultiThread, || {
        a_report_pushed_back_is_handled_before_the_reports_queued_behind_it()
    });
}
