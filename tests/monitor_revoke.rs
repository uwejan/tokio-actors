#![allow(dead_code)]
//! Revoking a monitor: `demonitor` is the info form, `demonitor_flush` also
//! drops a notice that is queued, and the counters the snapshot reports rise
//! and fall with the monitors.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, Down, MonitorId, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(300);
/// Rounds a scenario may take to have its notice taken before the revoke.
const ROUNDS: usize = 20;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("mrevoke-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

struct Victim;

impl Actor for Victim {
    type Message = ();
    type Response = ();
    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Revoke {
    Demonitor,
    Flush,
}

/// A value whose drop panics once the flag is set.
struct Bomb(Arc<AtomicBool>);

impl Drop for Bomb {
    fn drop(&mut self) {
        if self.0.load(Ordering::SeqCst) {
            panic!("a flushed notice dropped");
        }
    }
}

enum Cmd {
    Watch(ActorHandle<Victim>),
    /// Watches with a mapped message that carries a [`Bomb`] on the flag.
    WatchWithBomb(ActorHandle<Victim>, Arc<AtomicBool>),
    /// Signals `entered`, waits for the gate, then arms the flag and flushes.
    ArmAndFlush(Arc<Semaphore>, Arc<Semaphore>, MonitorId, Arc<AtomicBool>),
    GoneWithBomb(Down, Bomb),
    Demonitor(MonitorId),
    Flush(MonitorId),
    /// Signals `entered`, waits for the gate, then revokes the monitor in the
    /// same turn.
    WaitThen(Arc<Semaphore>, Arc<Semaphore>, MonitorId, Revoke),
    Gone(Down),
}

#[derive(Debug, PartialEq)]
enum Out {
    Id(MonitorId),
    Flag(bool),
    Unit,
}

struct Watcher {
    log: mpsc::UnboundedSender<Down>,
}

impl Actor for Watcher {
    type Message = Cmd;
    type Response = Out;

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Out> {
        Ok(match msg {
            Cmd::Watch(target) => Out::Id(ctx.monitor(&target, Cmd::Gone).expect("running")),
            Cmd::WatchWithBomb(target, armed) => Out::Id(
                ctx.monitor(&target, move |down| {
                    Cmd::GoneWithBomb(down, Bomb(armed.clone()))
                })
                .expect("running"),
            ),
            Cmd::ArmAndFlush(entered, gate, id, armed) => {
                entered.add_permits(1);
                let _ = gate.acquire().await;
                armed.store(true, Ordering::SeqCst);
                Out::Flag(ctx.demonitor_flush(id))
            }
            Cmd::GoneWithBomb(..) => Out::Unit,
            Cmd::Demonitor(id) => Out::Flag(ctx.demonitor(id)),
            Cmd::Flush(id) => Out::Flag(ctx.demonitor_flush(id)),
            Cmd::WaitThen(entered, gate, id, revoke) => {
                entered.add_permits(1);
                let _ = gate.acquire().await;
                Out::Flag(match revoke {
                    Revoke::Demonitor => ctx.demonitor(id),
                    Revoke::Flush => ctx.demonitor_flush(id),
                })
            }
            Cmd::Gone(down) => {
                let _ = self.log.send(down);
                Out::Unit
            }
        })
    }
}

async fn setup() -> (
    ActorHandle<Watcher>,
    mpsc::UnboundedReceiver<Down>,
    ActorHandle<Victim>,
) {
    let (log, rx) = mpsc::unbounded_channel();
    let w = Watcher { log }
        .spawn()
        .named(uname("watcher"))
        .await
        .unwrap();
    let v = Victim.spawn().named(uname("victim")).await.unwrap();
    (w, rx, v)
}

async fn watch(w: &ActorHandle<Watcher>, v: &ActorHandle<Victim>) -> MonitorId {
    match w.send(Cmd::Watch(v.clone())).await.unwrap() {
        Out::Id(id) => id,
        other => panic!("{other:?}"),
    }
}

async fn flag(w: &ActorHandle<Watcher>, cmd: Cmd) -> bool {
    match w.send(cmd).await.unwrap() {
        Out::Flag(f) => f,
        other => panic!("{other:?}"),
    }
}

/// A watcher whose revoke came too late: the watcher is inside a turn that
/// revokes at its gate, the victim ends once that turn has begun, and the
/// gate opens once the notice has its slot in the watcher's mailbox. The slot
/// is taken before the notice is, so a revoke can still land in time; the
/// call then answers true, and the round is run again with a new pair.
async fn revoked_too_late(
    revoke: Revoke,
) -> (
    ActorHandle<Watcher>,
    mpsc::UnboundedReceiver<Down>,
    MonitorId,
) {
    for _ in 0..ROUNDS {
        let (w, log, v) = setup().await;
        let id = watch(&w, &v).await;
        let entered = Arc::new(Semaphore::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let parked = {
            let (w, entered, gate) = (w.clone(), entered.clone(), gate.clone());
            tokio::spawn(async move { flag(&w, Cmd::WaitThen(entered, gate, id, revoke)).await })
        };
        within("the revoking turn to begin", entered.acquire())
            .await
            .unwrap()
            .forget();
        v.stop(StopReason::Graceful).await.unwrap();
        v.wait_stopped().await;
        the_notice_has_its_slot(&w).await;
        gate.add_permits(1);
        if !parked.await.unwrap() {
            return (w, log, id);
        }
    }
    panic!("no round took the notice before the revoke");
}

/// Waits until one slot of the watcher's mailbox is taken: the notice's.
async fn the_notice_has_its_slot(w: &ActorHandle<Watcher>) {
    let give_up = std::time::Instant::now() + BOUND;
    while w.mailbox_len() != 1 && std::time::Instant::now() < give_up {
        tokio::task::yield_now().await;
    }
    assert_eq!(w.mailbox_len(), 1, "the notice has its slot");
}

/// `demonitor_flush` drops a queued notice: it never reaches `handle`, and the
/// call says it had to.
#[test]
fn flush_drops_a_queued_down() {
    common::run(4, async {
        // The flush read false: a notice was taken, and the flush dropped it.
        let (w, mut log, id) = revoked_too_late(Revoke::Flush).await;
        // The watcher answers a later message, so the dropped notice was
        // dispatched past.
        w.send(Cmd::Demonitor(id)).await.unwrap();
        sleep(QUIET).await;
        assert!(log.try_recv().is_err(), "a flushed notice was delivered");

        // In time: nothing queued, the flush reads true.
        let (w, _log, v) = setup().await;
        let id = watch(&w, &v).await;
        assert!(flag(&w, Cmd::Flush(id)).await);
    });
}

/// A flushed notice is dropped by the actor's own turn, so a drop that panics
/// fails the actor with `Failure(Panic)`, as a panic in any drop of the turn
/// does.
#[test]
fn a_flushed_notice_whose_drop_panics_fails_the_actor() {
    common::run(4, async {
        for _ in 0..ROUNDS {
            let (w, _log, v) = setup().await;
            let armed = Arc::new(AtomicBool::new(false));
            let id = match w
                .send(Cmd::WatchWithBomb(v.clone(), armed.clone()))
                .await
                .unwrap()
            {
                Out::Id(id) => id,
                other => panic!("{other:?}"),
            };
            let entered = Arc::new(Semaphore::new(0));
            let gate = Arc::new(Semaphore::new(0));
            let parked = {
                let (w, entered, gate, armed) = (w.clone(), entered.clone(), gate.clone(), armed);
                tokio::spawn(
                    async move { w.send(Cmd::ArmAndFlush(entered, gate, id, armed)).await },
                )
            };
            // The watcher is inside the turn that will flush; the victim ends
            // and its notice takes a slot behind that turn.
            within("the flushing turn to begin", entered.acquire())
                .await
                .unwrap()
                .forget();
            v.stop(StopReason::Graceful).await.unwrap();
            v.wait_stopped().await;
            the_notice_has_its_slot(&w).await;
            gate.add_permits(1);
            // A flush that reads true landed before the notice was taken:
            // nothing was queued for the turn to drop, and the round is run
            // again with a new pair.
            if matches!(parked.await.unwrap(), Ok(Out::Flag(true))) {
                continue;
            }
            let reason = within("the watcher to end", w.wait_stopped()).await;
            assert!(
                matches!(
                    reason,
                    StopReason::Failure(tokio_actors::ActorError::Panic(_))
                ),
                "{reason:?}"
            );
            return;
        }
        panic!("no round took the notice before the flush");
    });
}

/// The snapshot names the run and counts monitors on both sides, and every
/// path that ends a monitor gives its count back.
#[test]
fn snapshot_fields() {
    common::run(4, async {
        let (w, mut log, v) = setup().await;
        let before_v = v.get_status().await.unwrap();
        let before_w = w.get_status().await.unwrap();
        assert_eq!(before_v.incarnation, v.incarnation());
        assert_eq!(before_w.incarnation, w.incarnation());
        assert_ne!(before_v.incarnation, before_w.incarnation);
        assert_eq!((before_v.monitors, before_v.monitored_by), (0, 0));
        assert_eq!((before_w.monitors, before_w.monitored_by), (0, 0));

        let first = watch(&w, &v).await;
        let second = watch(&w, &v).await;
        assert_eq!(w.get_status().await.unwrap().monitors, 2);
        assert_eq!(v.get_status().await.unwrap().monitored_by, 2);

        assert!(flag(&w, Cmd::Demonitor(first)).await);
        assert_eq!(w.get_status().await.unwrap().monitors, 1);
        assert_eq!(v.get_status().await.unwrap().monitored_by, 1);

        // The watcher's end gives the target its count back.
        w.stop(StopReason::Graceful).await.unwrap();
        w.wait_stopped().await;
        assert_eq!(v.get_status().await.unwrap().monitored_by, 0);
        let _ = second;

        // A fired and dispatched monitor leaves no entry on the watcher.
        let (w2, mut log2, v2) = setup().await;
        watch(&w2, &v2).await;
        v2.stop(StopReason::Graceful).await.unwrap();
        within("the notice", log2.recv()).await.unwrap();
        assert_eq!(w2.get_status().await.unwrap().monitors, 0);
        assert!(log.try_recv().is_err());
    });
}
