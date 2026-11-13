//! The monitor cell, the entry task and the counts they keep, on bare
//! mailboxes and planes. Every test runs on a paused clock: a wait ends only
//! once every task is idle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::monitor::{new_entry, put_notice};
use crate::actor::handle::ActorHandle;
use crate::actor::mailbox::{test_channel, MailRx, QueueRx};
use crate::actor::{Actor, ActorEnvelope};
use crate::error::ActorResult;
use crate::system::ResolvedTarget;
use crate::types::{
    ActorId, ActorStatus, Down, DownReason, MonitorId, SignalTag, StatusRx, StatusTx, StatusValue,
    StopLane, StopReason,
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

/// A handle, its mailbox's receiver and its priority lane's.
type Bare = (
    ActorHandle<Sink>,
    MailRx<ActorEnvelope<Sink>>,
    Option<tokio::sync::mpsc::Receiver<ActorEnvelope<Sink>>>,
);

/// A handle on a bare mailbox of `capacity` units, with a priority lane of
/// four when `lane` is set, and the mailbox's receiver.
fn handle_of(capacity: usize, lane: bool) -> Bare {
    let (tx, rx) = test_channel(capacity);
    let (_status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, _) = StopLane::new();
    let (priority_tx, lane_rx) = if lane {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let handle = ActorHandle::new(
        ActorId::from("monitor-sink"),
        tx,
        stop_lane,
        capacity,
        status_rx,
        priority_tx,
        Some(Arc::new(AtomicBool::new(false))),
    );
    (handle, rx, lane_rx)
}

/// A running target plane and its sender, which ends it.
fn target() -> (StatusTx, StatusRx) {
    StatusTx::channel(StatusValue::new(ActorStatus::Running))
}

fn end_target(tx: &StatusTx) {
    StatusValue::write_reason(tx, StopReason::Graceful, true);
}

fn resolved(rx: &StatusRx) -> ResolvedTarget {
    ResolvedTarget {
        actor_id: ActorId::from("target"),
        incarnation: Some(rx.record().incarnation()),
        status_rx: Some(rx.clone()),
        lane: None,
    }
}

fn map_to_monitor_number(down: Down) -> u32 {
    u32::try_from(format!("{:?}", down.monitor).len()).unwrap_or(0)
}

async fn until_idle() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

/// A notice whose push finds the queue closed is not spent: the cell reads
/// pending again, so the stop-time stage still owes it. The queue closes
/// between the slot's reservation and the send, on the normal mailbox and on
/// the priority lane.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn closed_mailbox_restores_pending() {
    for lane in [false, true] {
        let (handle, mut rx, mut lane_rx) = handle_of(2, lane);
        let (_tx, plane) = target();
        let (_entry, task, _token) = new_entry::<Sink>(
            MonitorId::next(),
            resolved(&plane),
            lane,
            Arc::new(map_to_monitor_number),
            crate::actor::runtime::Owner::new(ActorId::from("watcher"), false),
        );
        let shared = task.shared.clone();
        let permit = handle.reserve_signal(lane).await.expect("a slot");
        assert!(shared.take());
        match lane_rx.as_mut() {
            Some(lane_rx) => lane_rx.close(),
            None => QueueRx::close(&mut rx),
        }
        put_notice(permit, &shared, 7);
        assert_eq!(shared.state_for_test(), 0, "lane {lane}: owed again");

        // A queue that takes the message leaves the cell taken.
        let (handle, _rx, _lane_rx) = handle_of(2, lane);
        let permit = handle.reserve_signal(lane).await.expect("a slot");
        assert!(shared.take());
        put_notice(permit, &shared, 7);
        assert_eq!(shared.state_for_test(), 1, "lane {lane}: landed");
    }
}

/// The cell's transitions from every state: one winner among the entry task,
/// the revoke and the stage.
#[test]
fn cell_transitions() {
    use super::monitor::MonitorShared;
    // States: 0 pending, 1 taken, 2 revoked, 3 flushed, 4 claimed by the
    // stage, 5 spent.
    type Op = fn(&MonitorShared) -> bool;
    let ops: [(&str, Op); 7] = [
        ("take", MonitorShared::take),
        ("take_closed", MonitorShared::take_closed),
        ("claim_for_stage", MonitorShared::claim_for_stage),
        ("revoke", MonitorShared::revoke),
        ("flush", MonitorShared::flush),
        ("restore", |s| {
            s.restore();
            true
        }),
        ("spend", |s| {
            s.spend();
            true
        }),
    ];
    // For each starting state: (result, state after) per operation, in the
    // order above.
    let table: [(u8, [(bool, u8); 7]); 6] = [
        (
            0,
            [
                (true, 1),
                (false, 0),
                (true, 4),
                (true, 2),
                (true, 2),
                (true, 0),
                (true, 0),
            ],
        ),
        (
            1,
            [
                (false, 1),
                (false, 1),
                (false, 1),
                (false, 1),
                (false, 3),
                (true, 0),
                (true, 5),
            ],
        ),
        (
            2,
            [
                (false, 2),
                (false, 2),
                (false, 2),
                (false, 2),
                (false, 2),
                (true, 2),
                (true, 2),
            ],
        ),
        (
            3,
            [
                (false, 3),
                (false, 3),
                (false, 3),
                (false, 3),
                (false, 3),
                (true, 3),
                (true, 3),
            ],
        ),
        (
            4,
            [
                (false, 4),
                (true, 1),
                (false, 4),
                (true, 2),
                (true, 2),
                (true, 4),
                (true, 4),
            ],
        ),
        (
            5,
            [
                (false, 5),
                (false, 5),
                (false, 5),
                (false, 5),
                (false, 5),
                (true, 5),
                (true, 5),
            ],
        ),
    ];
    for (start, rows) in table {
        for ((name, op), (want, after)) in ops.iter().zip(rows) {
            let (_tx, plane) = target();
            let (_entry, task, _token) = new_entry::<Sink>(
                MonitorId::next(),
                resolved(&plane),
                false,
                Arc::new(map_to_monitor_number),
                crate::actor::runtime::Owner::new(ActorId::from("watcher"), false),
            );
            let shared = task.shared.clone();
            drive(&shared, start);
            assert_eq!(op(&shared), want, "{name} from {start}");
            assert_eq!(shared.state_for_test(), after, "{name} from {start}");
        }
    }
}

/// Moves a fresh cell to `state` by the operations that reach it.
fn drive(shared: &super::monitor::MonitorShared, state: u8) {
    match state {
        0 => {}
        1 => assert!(shared.take()),
        2 => assert!(shared.revoke()),
        3 => {
            assert!(shared.take());
            assert!(!shared.flush());
        }
        4 => assert!(shared.claim_for_stage()),
        _ => {
            assert!(shared.take());
            shared.spend();
        }
    }
}

/// Monitoring and revoking in a loop against a live target leaves no task.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn monitor_demonitor_loop_leaves_no_task() {
    use super::context::ActorContext;

    struct Loopy(ActorHandle<Sink>);

    impl Actor for Loopy {
        type Message = bool;
        type Response = (usize, usize);

        async fn handle(
            &mut self,
            count: bool,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<(usize, usize)> {
            if !count {
                for _ in 0..1000 {
                    let id = ctx.monitor(&self.0, |_| false).expect("running");
                    assert!(ctx.demonitor(id));
                }
            }
            Ok((ctx.forwarder_tasks(), ctx.monitor_count()))
        }
    }

    use crate::actor::ActorExt;
    let live = Sink.spawn().await.unwrap();
    let watcher = Loopy(live.clone()).spawn().await.unwrap();
    watcher.send(false).await.unwrap();
    // The run loop reaps the ended tasks before it takes the next message.
    for _ in 0..20 {
        until_idle().await;
    }
    assert_eq!(watcher.send(true).await.unwrap(), (0, 0));
    assert_eq!(live.get_status().await.unwrap().monitored_by, 0);
}

/// Reports the envelope's size, which the idle-memory gate compares: the tag
/// is one pointer, so the notice variant does not outgrow the message one.
#[test]
fn size_of_envelope_is_reported() {
    println!(
        "size_of ActorEnvelope<Sink> = {}, SignalTag = {}",
        std::mem::size_of::<ActorEnvelope<Sink>>(),
        std::mem::size_of::<SignalTag>()
    );
    assert_eq!(
        std::mem::size_of::<SignalTag>(),
        std::mem::size_of::<usize>()
    );
    let _ = DownReason::Unknown;
}

/// A captured value that panics when it is dropped while armed.
struct LoudDrop(Arc<AtomicBool>);

impl Drop for LoudDrop {
    fn drop(&mut self) {
        if self.0.load(Ordering::SeqCst) {
            panic!("a captured value of the map panicked when dropped");
        }
    }
}
