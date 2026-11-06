//! `kill_by_id_with_observer` takes the observer and the kill from one
//! lookup. A registry that changes hands between the lookup and the raise (a
//! supervised child that restarted by itself) must not split them: the
//! observer names the incarnation the kill was raised on.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::time::timeout;

use super::ActorSystem;
use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::{Actor, ActorExt};
use crate::error::ActorResult;
use crate::types::{
    ActorStatus, StatusTx, StatusValue, StopLane, StopOutcome, SubtreeEnd, TurnClock,
};

/// The longest a call may take. It only limits how long a real hang takes to
/// report.
const BOUND: Duration = Duration::from_secs(10);

/// An actor that only answers.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_observer_names_the_incarnation_the_lookup_found() {
    let sys = ActorSystem::create(format!("pairing-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("pairing-target-{}", uuid::Uuid::new_v4());
    let first = Idle
        .spawn()
        .named(name.clone())
        .on_system(&sys)
        .await
        .expect("the actor starts");
    let id = first.id().clone();

    // Between the lookup and the raise the id changes hands: the entry the
    // lookup found is removed and another incarnation is registered under the
    // same id, whose plane the test keeps open.
    let (second_tx, second_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (second_lane, _second_stop_rx) = StopLane::new();
    let (mail_tx, _mail_rx) = crate::actor::mailbox::test_channel(1);
    let second = ActorHandle::<Idle>::new(
        id.clone(),
        mail_tx,
        second_lane,
        1,
        second_rx,
        sys.name().into(),
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    {
        let (hook_sys, hook_id) = (sys.clone(), id.clone());
        let second = second.clone();
        *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
            hook_sys.by_id.remove(&hook_id);
            hook_sys
                .register_actor::<Idle>(
                    &hook_id,
                    None,
                    &second,
                    true,
                    TurnClock::default(),
                    Duration::from_millis(120),
                    Handle::current(),
                )
                .expect("the id is free");
        }));
    }

    let (report, observer) = timeout(BOUND, sys.kill_by_id_with_observer(&id))
        .await
        .expect("the call returns")
        .expect("the id was registered");
    assert!(
        matches!(report.outcome, StopOutcome::Killed | StopOutcome::Aborted),
        "the first incarnation took the kill: {report:?}"
    );
    assert_eq!(
        timeout(BOUND, observer.wait_ended())
            .await
            .expect("the observer's actor ended"),
        SubtreeEnd::Ended,
        "the observer names the incarnation the kill was raised on"
    );

    // The incarnation registered in between is not the one the observer names.
    let other = sys.observe_subtree(&id).expect("the second is registered");
    assert!(
        timeout(Duration::from_millis(100), other.wait_ended())
            .await
            .is_err(),
        "an observer of the incarnation that replaced it waits for it"
    );
    drop(second_tx);
    assert_eq!(
        timeout(BOUND, other.wait_ended())
            .await
            .expect("the second incarnation's plane closed"),
        SubtreeEnd::Ended
    );
}
