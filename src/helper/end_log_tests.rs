//! What the end of a helper task writes to the log (with the `tracing`
//! feature): a task cancelled while it held late values is logged once with
//! its owner and the number of values dropped; a cancellation that dropped
//! nothing writes nothing.

use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::actor::runtime;
use crate::actor::supervision::lost_runner::capture;
use crate::helper::{HelperEnd, HelperKind};
use crate::system::ActorSystem;
use crate::types::ActorId;

const HANG_BOUND: Duration = Duration::from_secs(60);

fn fresh_system(tag: &str) -> Arc<ActorSystem> {
    ActorSystem::create(format!("helper-end-log-{tag}-{}", uuid::Uuid::new_v4())).unwrap()
}

fn fresh_owner(tag: &str) -> ActorId {
    ActorId::from(format!("helper-end-log-{tag}-{}", uuid::Uuid::new_v4()))
}

/// The captured warnings that name `owner`.
fn lines_for(owner: &ActorId) -> Vec<String> {
    let token = format!("actor_id={owner} ");
    capture::install()
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(&token))
        .cloned()
        .collect()
}

/// Ends a late-value drain task on a runtime that is dropped before the task
/// is polled once, with `values` late values waiting in its receiver, and
/// returns the end the system recorded.
fn drain_cancelled_by_its_runtime(
    system: &Arc<ActorSystem>,
    owner: &ActorId,
    values: usize,
) -> Vec<(HelperKind, HelperEnd)> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (tx, rx) = mpsc::channel::<oneshot::Sender<()>>(2);
    let mut replies = Vec::new();
    rt.block_on(async {
        let permit = tx.clone().try_reserve_owned().unwrap();
        assert!(runtime::end_queue_late(rx, system, owner).is_none());
        // The task has not been polled: what is sent waits in the receiver.
        if values > 0 {
            let (reply_tx, reply_rx) = oneshot::channel();
            replies.push(reply_rx);
            let _sender = permit.send(reply_tx);
        } else {
            drop(permit);
        }
        assert_eq!(system.live_helpers(), 1);
    });
    let handle = rt.handle().clone();
    drop(rt);
    assert_eq!(system.live_helpers(), 0, "the cancelled task counted down");
    drop(tx);
    system.helper_ends(&handle)
}

/// A drain cancelled while it held a value logs one warning naming the
/// actor, with the number of values it dropped.
#[test]
fn a_cancelled_helper_that_dropped_a_value_logs_once_with_its_owner() {
    let _ = capture::install();
    let system = fresh_system("loud");
    let owner = fresh_owner("loud");
    let ends = drain_cancelled_by_its_runtime(&system, &owner, 1);
    assert_eq!(
        ends,
        vec![(HelperKind::LateDrain, HelperEnd::Cancelled { dropped: 1 })]
    );
    let lines = lines_for(&owner);
    assert_eq!(lines.len(), 1, "one line for one cancellation: {lines:?}");
    assert!(
        lines[0].contains("dropped=1 ") && lines[0].contains("late values it held were dropped"),
        "{}",
        lines[0]
    );
}

/// A drain cancelled with nothing to drop, and a task aborted while it waits,
/// are recorded and write nothing to the log.
#[test]
fn a_cancelled_helper_that_dropped_nothing_logs_nothing() {
    let _ = capture::install();
    let system = fresh_system("quiet");

    let drained = fresh_owner("quiet-drain");
    let ends = drain_cancelled_by_its_runtime(&system, &drained, 0);
    assert_eq!(
        ends,
        vec![(HelperKind::LateDrain, HelperEnd::Cancelled { dropped: 0 })]
    );
    assert!(
        lines_for(&drained).is_empty(),
        "a cancellation that dropped nothing was logged: {:?}",
        lines_for(&drained)
    );

    let aborted = fresh_owner("quiet-abort");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.block_on(async {
        let rt_handle = Handle::current();
        let parked = system.spawn_helper(
            &rt_handle,
            HelperKind::Reaper,
            Some(aborted.clone()),
            |end| end.observe(std::future::pending::<()>()),
        );
        parked.abort();
        let _ = parked.await;
        timeout(HANG_BOUND, async {
            while system.live_helpers() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the aborted helper is counted down");
        rt_handle
    });
    assert_eq!(
        system.helper_ends(&handle),
        vec![(HelperKind::Reaper, HelperEnd::Cancelled { dropped: 0 })]
    );
    assert!(
        lines_for(&aborted).is_empty(),
        "an abort that dropped nothing was logged: {:?}",
        lines_for(&aborted)
    );
}
