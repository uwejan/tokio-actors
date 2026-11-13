//! A registry `Kill` declares its deadline on the actor's entry before the
//! `Kill` is raised: whichever of the raiser and the attach of the task's
//! abort handle comes second sees the other. A declaration that lands after
//! the raise leaves the attach to arm the later of two deadlines.
//!
//! The stop task of a bounded call made from a task is deferred: it sleeps
//! until its window ends or its caller is gone, and the last test below keeps
//! its call, without polling it again, to see it end with the window.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use super::ActorSystem;
use crate::actor::context::ActorContext;
use crate::actor::{Actor, ActorExt};
use crate::error::ActorResult;
use crate::types::StopReason;

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

/// How the `Kill` reaches the registry.
#[derive(Clone, Copy, Debug)]
enum Raiser {
    KillById,
    KillByName,
    StopWithinKill,
}

/// What the hook read at the moment the declaration returned.
#[derive(Debug)]
struct AtDeclaration {
    /// The deadline the entry held, `0` when none.
    due: u64,
    /// Whether the actor's stop lane already held a `Kill`.
    kill_raised: bool,
}

async fn the_deadline_is_declared_before_the_kill_is_raised(raiser: Raiser) {
    let sys = ActorSystem::create(format!("declare-first-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("declare-first-target-{}", uuid::Uuid::new_v4());
    let handle = Idle
        .spawn()
        .named(name.clone())
        .on_system(&sys)
        .await
        .expect("the actor starts");
    let seen: Arc<Mutex<Option<AtDeclaration>>> = Arc::new(Mutex::new(None));
    {
        let (hook_sys, lane, id, hook_name, hook_seen) = (
            sys.clone(),
            handle.stop_lane(),
            handle.id().clone(),
            name.clone(),
            seen.clone(),
        );
        sys.set_after_kill_declaration_hook(move || {
            let due = match raiser {
                Raiser::KillByName => hook_sys
                    .by_name
                    .get(hook_name.as_str())
                    .map_or(0, |entry| entry.kill_due),
                Raiser::KillById | Raiser::StopWithinKill => {
                    hook_sys.by_id.get(&id).map_or(0, |entry| entry.kill_due)
                }
            };
            *hook_seen.lock().unwrap() = Some(AtDeclaration {
                due,
                kill_raised: lane.reason() == Some(StopReason::Kill),
            });
        });
    }
    match raiser {
        Raiser::KillById => {
            timeout(BOUND, sys.kill_by_id(handle.id()))
                .await
                .expect("the kill returns");
        }
        Raiser::KillByName => {
            timeout(BOUND, sys.kill(&name))
                .await
                .expect("the kill returns")
                .expect("the kill is accepted");
        }
        Raiser::StopWithinKill => {
            timeout(
                BOUND,
                handle.stop_within(StopReason::Kill, Duration::from_secs(5)),
            )
            .await
            .expect("the call returns")
            .expect("the request is accepted");
        }
    }
    let seen = seen
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| panic!("{raiser:?}: the Kill declared no deadline"));
    assert_ne!(
        seen.due, 0,
        "{raiser:?}: the entry holds the deadline once the declaration returns"
    );
    assert!(
        !seen.kill_raised,
        "{raiser:?}: the deadline is declared before the Kill is raised"
    );
}

/// A bounded call made from a task, and kept there without being polled
/// again, releases its stop task when its window ends, whether or not the
/// caller is still there. The clock is paused, so the window passes at once
/// and no host delay can end it early.
#[tokio::test(start_paused = true)]
async fn a_kept_call_made_from_a_task_releases_its_stop_task_when_its_window_ends() {
    let sys = ActorSystem::create(format!("kept-from-a-task-{}", uuid::Uuid::new_v4())).unwrap();
    let handle = Idle.spawn().on_system(&sys).await.unwrap();
    let window = Duration::from_millis(200);
    let (polled_tx, polled_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let caller = {
        let handle = handle.clone();
        tokio::spawn(async move {
            let mut call = Box::pin(handle.stop_within(StopReason::Graceful, window));
            {
                let mut cx = Context::from_waker(Waker::noop());
                let _ = call.as_mut().poll(&mut cx);
            }
            let _ = polled_tx.send(());
            // The call is kept, not polled again, until the test lets go.
            let _ = release_rx.await;
            drop(call);
        })
    };
    timeout(BOUND, polled_rx).await.unwrap().unwrap();
    timeout(BOUND, handle.wait_stopped()).await.unwrap();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(
        sys.live_helpers() >= 1,
        "the stop task is held until its window ends"
    );
    tokio::time::advance(window + Duration::from_millis(50)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        sys.live_helpers(),
        0,
        "the stop task ended with the window, while its call is still kept"
    );
    let _ = release_tx.send(());
    timeout(BOUND, caller).await.unwrap().unwrap();
}
