//! A `Kill` raised for an exit signal carries the signal's reason and is
//! declared, raised and backstopped like any registry `Kill`: the deadline is
//! on the entry before the lane holds the `Kill`, the lane stores the cause
//! with the raise and not after it, and the abort the raise arms ends the
//! actor with the cause.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::timeout;

use super::{raise_kill_declared, ActorSystem, KillKey};
use crate::actor::context::ActorContext;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult};
use crate::types::{RaiseResult, StopReason};

/// The longest a call may take. It only limits how long a real hang takes to
/// report.
const BOUND: Duration = Duration::from_secs(60);

/// An actor that only answers.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// What the hook read at the moment the declaration returned.
#[derive(Debug)]
struct AtDeclaration {
    /// The deadline the entry held, `0` when none.
    due: u64,
    /// Whether the actor's stop lane already held a `Kill`.
    kill_raised: bool,
    /// The cause the lane held.
    cause: Option<StopReason>,
}

fn signalled() -> StopReason {
    StopReason::Failure(ActorError::user("signalled"))
}

#[tokio::test]
async fn cause_raiser_declares_before_it_raises() {
    let sys = ActorSystem::create(format!("cause-declare-{}", uuid::Uuid::new_v4())).unwrap();
    let handle = Idle
        .spawn()
        .on_system(&sys)
        .await
        .expect("the actor starts");
    let seen: Arc<Mutex<Option<AtDeclaration>>> = Arc::new(Mutex::new(None));
    {
        let (hook_sys, lane, id, hook_seen) = (
            sys.clone(),
            handle.stop_lane(),
            handle.id().clone(),
            seen.clone(),
        );
        sys.set_after_kill_declaration_hook(move || {
            let due = hook_sys.by_id.get(&id).map_or(0, |entry| entry.kill_due);
            *hook_seen.lock().unwrap() = Some(AtDeclaration {
                due,
                kill_raised: lane.reason() == Some(StopReason::Kill),
                cause: lane.abort_record().kill_cause(),
            });
        });
    }
    let raised = raise_kill_declared(
        Some(&sys),
        KillKey::Id(handle.id()),
        &handle.status_rx(),
        &handle.stop_lane(),
        None,
        None,
        || {},
        Some(signalled()),
    );
    assert_eq!(raised.result, RaiseResult::Recorded);
    let seen = seen
        .lock()
        .unwrap()
        .take()
        .expect("the caused Kill declared no deadline");
    assert_ne!(
        seen.due, 0,
        "the entry holds the deadline once the declaration returns"
    );
    assert!(
        !seen.kill_raised,
        "the deadline is declared before the Kill is raised"
    );
    assert_eq!(
        seen.cause, None,
        "the cause is stored with the raise, not before it"
    );
    let ended = timeout(BOUND, handle.wait_stopped())
        .await
        .expect("the actor ends");
    assert_eq!(ended, signalled());
}
