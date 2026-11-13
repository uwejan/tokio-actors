//! The monitor of a run its watcher has just spawned, on bare mailboxes and
//! planes: a run that ended first keeps its own reason.

use std::sync::{Arc, Mutex};

use super::monitor::{monitor_forward, new_entry, new_spawned_entry};
use crate::actor::handle::ActorHandle;
use crate::actor::mailbox::test_channel;
use crate::actor::Actor;
use crate::error::ActorResult;
use crate::system::ResolvedTarget;
use crate::types::{
    ActorId, ActorStatus, Down, DownReason, MonitorId, StatusTx, StatusValue, StopLane, StopReason,
};

struct Sink;

impl Actor for Sink {
    type Message = u32;
    type Response = ();

    async fn handle(
        &mut self,
        _: u32,
        _ctx: &mut super::context::ActorContext<Self>,
    ) -> ActorResult<()> {
        Ok(())
    }
}

fn handle_of() -> (ActorHandle<Sink>, impl Sized) {
    let (tx, rx) = test_channel(4);
    let (_status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, _) = StopLane::new();
    let handle = ActorHandle::new(
        ActorId::from("spawned-monitor-sink"),
        tx,
        stop_lane,
        4,
        status_rx,
        None,
        None,
    );
    (handle, rx)
}

/// What the entry task's map sees for a watched run that ended before the
/// monitor was made, from the plain constructor and from the spawned one.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_run_that_ended_first_keeps_its_reason_only_for_a_spawned_monitor() {
    for spawned in [false, true] {
        let (tx, plane) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        StatusValue::write_reason(&tx, StopReason::Graceful, true);
        let target = ResolvedTarget {
            actor_id: ActorId::from("target"),
            incarnation: Some(plane.record().incarnation()),
            status_rx: Some(plane.clone()),
            lane: None,
        };
        let seen: Arc<Mutex<Option<Down>>> = Arc::new(Mutex::new(None));
        let sink = seen.clone();
        let map = Arc::new(move |down: Down| {
            *sink.lock().unwrap() = Some(down);
            0
        });
        let (_entry, task, token) = if spawned {
            new_spawned_entry::<Sink>(
                MonitorId::next(),
                target,
                map,
                crate::actor::runtime::Owner::new(ActorId::from("watcher"), false),
            )
        } else {
            new_entry::<Sink>(
                MonitorId::next(),
                target,
                false,
                map,
                crate::actor::runtime::Owner::new(ActorId::from("watcher"), false),
            )
        };
        let (handle, _rx) = handle_of();
        let _ = monitor_forward(handle, task, token).await;
        let down = seen.lock().unwrap().take();
        let reason = down.map(|d| d.reason);
        let expected = if spawned {
            DownReason::Stopped(StopReason::Graceful)
        } else {
            DownReason::NoProc
        };
        assert_eq!(reason, Some(expected), "spawned: {spawned}");
    }
}
