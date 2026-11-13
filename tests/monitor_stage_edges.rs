#![allow(dead_code)]
//! Edges of the stop-time stage for monitors: the notices a `Kill` leaves
//! behind are dropped inside a catch, a map function of a death monitor or a
//! group monitor that panics after the message loop ended still fails the
//! watcher, and a notice queued when a plain `demonitor` returned `false` is
//! still delivered once at a stop.
//!
//! Every test drives real actors and holds them at gates; the sleeps only
//! give a task time to settle.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, Notify, Semaphore};
use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorId, ActorResult, ActorStatus, ChildEvent, Down,
    MonitorId, PgScope, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);
const SETTLE: Duration = Duration::from_millis(250);
/// Rounds a scenario may take to have its notice taken before the revoke.
const ROUNDS: usize = 20;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("mstage-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
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

/// A value whose drop panics unless the handler that received it defused it.
struct Bomb {
    armed: bool,
}

impl Bomb {
    fn new() -> Self {
        Bomb { armed: true }
    }

    fn defuse(&mut self) {
        self.armed = false;
    }
}

impl Drop for Bomb {
    fn drop(&mut self) {
        if self.armed && !std::thread::panicking() {
            panic!("bomb");
        }
    }
}

/// Signals when it is dropped.
struct DropSignal(Arc<Notify>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

enum Cmd {
    /// Monitors the victim; the notice carries an armed `Bomb`.
    Watch(ActorHandle<Victim>),
    /// Monitors the victim with a map function that panics.
    WatchPanicMap(ActorHandle<Victim>),
    /// Monitors a group with a map function that panics; the notify is
    /// signalled when the closure is dropped, which is after the panic was
    /// handled.
    WatchPanicGroup(PgScope, Arc<Notify>),
    /// Spawns a child and keeps its handle in the shared slot.
    SpawnKid,
    /// Waits for the gate and goes on.
    Wait(Arc<Semaphore>),
    /// Signals `entered`, waits for the gate, then revokes the monitor with a
    /// plain `demonitor` and records the answer.
    WaitThenDemonitor(Arc<Semaphore>, Arc<Semaphore>, MonitorId),
    Gone(Down, Bomb),
}

#[derive(Debug)]
enum Ev {
    Down(Down, ActorStatus),
    Demonitor(bool),
    Child,
    Stopped,
}

#[derive(Default)]
struct Plan {
    /// The first notice that reaches `handle` raises a `Kill` on its actor.
    kill_self: bool,
    kid: Option<ActorHandle<Victim>>,
    /// The first notice that reaches `handle` signals the notify, then waits
    /// for a permit.
    hold: Option<(Arc<Notify>, Arc<Semaphore>)>,
}

/// An actor that sends exit signals.
struct Signaler;

impl Actor for Signaler {
    type Message = (ActorId, StopReason);
    type Response = ();

    async fn handle(
        &mut self,
        (target, reason): (ActorId, StopReason),
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        ctx.exit(&target, reason)
            .expect("an exit signal is refused only at its target's bound");
        Ok(())
    }
}

struct Watcher {
    log: mpsc::UnboundedSender<Ev>,
    plan: Arc<Mutex<Plan>>,
    /// `on_child_stopped` signals `child_entered`, then waits here.
    child_gate: Arc<Semaphore>,
    child_entered: Arc<Notify>,
}

impl Actor for Watcher {
    type Message = Cmd;
    type Response = Option<MonitorId>;

    async fn handle(
        &mut self,
        msg: Cmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Option<MonitorId>> {
        match msg {
            Cmd::Watch(v) => Ok(Some(
                ctx.monitor(&v, |down| Cmd::Gone(down, Bomb::new()))
                    .expect("running"),
            )),
            Cmd::WatchPanicMap(v) => Ok(Some(
                ctx.monitor(&v, |_| -> Cmd { panic!("map boom") })
                    .expect("running"),
            )),
            Cmd::WatchPanicGroup(scope, dropped) => {
                let guard = DropSignal(dropped);
                Ok(Some(
                    ctx.monitor_group(&scope, "g", move |_| -> Cmd {
                        let _held = &guard;
                        panic!("map boom")
                    })
                    .expect("running")
                    .0,
                ))
            }
            Cmd::SpawnKid => {
                let kid = ctx.spawn_child(|| Victim).await?;
                self.plan.lock().unwrap().kid = Some(kid);
                Ok(None)
            }
            Cmd::Wait(gate) => {
                let _ = gate.acquire().await;
                Ok(None)
            }
            Cmd::WaitThenDemonitor(entered, gate, id) => {
                entered.add_permits(1);
                let _ = gate.acquire().await;
                let revoked = ctx.demonitor(id);
                let _ = self.log.send(Ev::Demonitor(revoked));
                Ok(None)
            }
            Cmd::Gone(down, mut bomb) => {
                bomb.defuse();
                let _ = self.log.send(Ev::Down(down, ctx.status()));
                let (kill, hold) = {
                    let mut plan = self.plan.lock().unwrap();
                    (std::mem::take(&mut plan.kill_self), plan.hold.take())
                };
                if let Some((entered, release)) = hold {
                    entered.notify_one();
                    let _ = release.acquire().await.map(|permit| permit.forget());
                }
                if kill {
                    ctx.self_handle().stop(StopReason::Kill).await.ok();
                }
                Ok(None)
            }
        }
    }

    async fn on_child_stopped(
        &mut self,
        _event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.log.send(Ev::Child);
        self.child_entered.notify_one();
        let _ = self.child_gate.acquire().await;
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.log.send(Ev::Stopped);
        Ok(())
    }
}

struct Rig {
    w: ActorHandle<Watcher>,
    log: mpsc::UnboundedReceiver<Ev>,
    plan: Arc<Mutex<Plan>>,
    child_gate: Arc<Semaphore>,
    child_entered: Arc<Notify>,
}

async fn rig(supervisor: bool) -> Rig {
    let (log, rx) = mpsc::unbounded_channel();
    let plan = Arc::new(Mutex::new(Plan::default()));
    let child_gate = Arc::new(Semaphore::new(0));
    let child_entered = Arc::new(Notify::new());
    let mut builder = Watcher {
        log,
        plan: plan.clone(),
        child_gate: child_gate.clone(),
        child_entered: child_entered.clone(),
    }
    .spawn()
    .named(uname("watcher"))
    .with_config(ActorConfig::default().with_mailbox_capacity(64));
    if supervisor {
        builder = builder.supervisor();
    }
    Rig {
        w: builder.await.unwrap(),
        log: rx,
        plan,
        child_gate,
        child_entered,
    }
}

async fn victim() -> ActorHandle<Victim> {
    Victim.spawn().named(uname("victim")).await.unwrap()
}

async fn end(v: &ActorHandle<Victim>) {
    v.stop(StopReason::Graceful).await.unwrap();
    v.wait_stopped().await;
}

/// Parks the watcher in a turn and returns the gate that releases it.
async fn park(w: &ActorHandle<Watcher>) -> (Arc<Semaphore>, tokio::task::JoinHandle<()>) {
    let gate = Arc::new(Semaphore::new(0));
    let task = {
        let w = w.clone();
        let gate = gate.clone();
        tokio::spawn(async move {
            let _ = w.send(Cmd::Wait(gate)).await;
        })
    };
    sleep(SETTLE).await;
    (gate, task)
}

fn drain(log: &mut mpsc::UnboundedReceiver<Ev>) -> Vec<Ev> {
    let mut out = Vec::new();
    while let Ok(ev) = log.try_recv() {
        out.push(ev);
    }
    out
}

fn downs(events: &[Ev]) -> usize {
    events.iter().filter(|e| matches!(e, Ev::Down(..))).count()
}

/// A `Kill` raised by the first delivery of the stage leaves the notices
/// behind it undelivered, and their messages are dropped inside a catch: the
/// actor ends as `Kill` even when the drop of a message panics.
#[test]
fn a_kill_in_the_stage_drops_the_leftover_notices_inside_a_catch() {
    common::run(4, async {
        let mut rig = rig(false).await;
        let vs = [victim().await, victim().await, victim().await];
        for v in &vs {
            rig.w.send(Cmd::Watch(v.clone())).await.unwrap();
        }
        rig.plan.lock().unwrap().kill_self = true;
        let (gate, parked) = park(&rig.w).await;
        for v in &vs {
            end(v).await;
        }
        sleep(SETTLE).await;
        rig.w.stop(StopReason::Graceful).await.unwrap();
        gate.add_permits(1);
        let _ = parked.await;
        let ended = within("the end", rig.w.wait_stopped()).await;
        assert_eq!(
            ended,
            StopReason::Kill,
            "a panic of a leftover message's drop does not change the Kill"
        );
        let events = drain(&mut rig.log);
        assert_eq!(downs(&events), 1, "only the first notice ran: {events:?}");
    });
}

/// An exit signal that reaches a watcher that does not trap while the stage
/// delivers its notices lands a `Kill` with its cause on the lane: the
/// deliveries behind the running one are skipped, and the watcher ends with
/// the signal's reason.
#[test]
fn a_cause_in_the_stage_stops_the_remaining_deliveries() {
    common::run(4, async {
        let mut rig = rig(false).await;
        let (a, b) = (victim().await, victim().await);
        rig.w.send(Cmd::Watch(a.clone())).await.unwrap();
        rig.w.send(Cmd::Watch(b.clone())).await.unwrap();
        let signaler = Signaler.spawn().named(uname("signaler")).await.unwrap();
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Semaphore::new(0)));
        rig.plan.lock().unwrap().hold = Some((entered.clone(), release.clone()));
        let (gate, parked) = park(&rig.w).await;
        end(&a).await;
        end(&b).await;
        sleep(SETTLE).await;
        rig.w.stop(StopReason::Graceful).await.unwrap();
        gate.add_permits(1);
        let _ = parked.await;
        within("the first delivery", entered.notified()).await;
        let cause = StopReason::Failure(ActorError::user("signalled"));
        signaler
            .send((rig.w.id().clone(), cause.clone()))
            .await
            .unwrap();
        // The signal's `Kill` is on the lane once the signaler has handled
        // it; the delivery in hand goes on.
        release.add_permits(1);
        let ended = within("the end", rig.w.wait_stopped()).await;
        assert_eq!(ended, cause);
        let events = drain(&mut rig.log);
        assert_eq!(
            downs(&events),
            1,
            "the second notice is skipped: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, Ev::Stopped)),
            "a Kill skips on_stopped: {events:?}"
        );
    });
}

/// A map function that panics after the watcher's message loop ended, while
/// the supervisor is still delivering its child reports, fails the watcher:
/// the stage reports the panic.
#[test]
fn a_map_panic_after_the_loop_ended_fails_the_watcher() {
    common::run(4, async {
        let mut rig = rig(true).await;
        let v = victim().await;
        rig.w.send(Cmd::WatchPanicMap(v.clone())).await.unwrap();
        rig.w.send(Cmd::SpawnKid).await.unwrap();
        let kid = rig
            .plan
            .lock()
            .unwrap()
            .kid
            .clone()
            .expect("the child is spawned");
        let (gate, parked) = park(&rig.w).await;
        // The child's death waits for the loop to end; the stop request is
        // read first.
        end(&kid).await;
        rig.w.stop(StopReason::Graceful).await.unwrap();
        gate.add_permits(1);
        let _ = parked.await;
        within("the child report", rig.child_entered.notified()).await;
        // The message loop is over and the supervisor is inside the final
        // delivery. The watched actor ends now, and its map panics.
        end(&v).await;
        sleep(SETTLE).await;
        rig.child_gate.add_permits(1);
        let ended = within("the end", rig.w.wait_stopped()).await;
        assert_eq!(
            ended,
            StopReason::Failure(ActorError::Panic("map boom".into()))
        );
        let events = drain(&mut rig.log);
        assert!(
            events.iter().any(|e| matches!(e, Ev::Stopped)),
            "on_stopped ran: {events:?}"
        );
    });
}

/// A plain `demonitor` that returns `false` because the notice is already
/// queued, followed by a stop, still delivers that notice once at the stop,
/// before `on_stopped`.
///
/// The watcher is inside a turn that revokes at its gate, the watched actor
/// ends once that turn has begun, and the gate opens once the notice has its
/// slot in the watcher's mailbox. The slot is taken before the notice is, so
/// the `demonitor` can still land in time; it then answers true, nothing is
/// delivered, and the round is run again with new actors.
#[test]
fn a_late_plain_demonitor_leaves_the_queued_notice_to_the_stop() {
    common::run(4, async {
        for _ in 0..ROUNDS {
            let mut rig = rig(false).await;
            let v = victim().await;
            let id = rig
                .w
                .send(Cmd::Watch(v.clone()))
                .await
                .unwrap()
                .expect("an id");
            let entered = Arc::new(Semaphore::new(0));
            let gate = Arc::new(Semaphore::new(0));
            let turn = {
                let (w, entered, gate) = (rig.w.clone(), entered.clone(), gate.clone());
                tokio::spawn(async move { w.send(Cmd::WaitThenDemonitor(entered, gate, id)).await })
            };
            within("the revoking turn to begin", entered.acquire())
                .await
                .unwrap()
                .forget();
            end(&v).await;
            // The notice takes its slot while the turn holds the actor.
            within("the notice has its slot", async {
                while rig.w.mailbox_len() < 1 {
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            rig.w.stop(StopReason::Graceful).await.unwrap();
            gate.add_permits(1);
            let _ = within("the turn", turn).await;
            let ended = within("the end", rig.w.wait_stopped()).await;
            let events = drain(&mut rig.log);
            // The turn's answer is the first event: no notice reaches
            // `handle` while the turn holds the actor.
            if matches!(events.first(), Some(Ev::Demonitor(true))) {
                // The revoke took the monitor before the notice was taken:
                // nothing arrives.
                assert_eq!(
                    downs(&events),
                    0,
                    "a demonitor that answered true is followed by its notice: {events:?}"
                );
                continue;
            }
            assert_eq!(ended, StopReason::Graceful);
            let shape: Vec<&str> = events
                .iter()
                .map(|e| match e {
                    Ev::Demonitor(false) => "demonitor false",
                    Ev::Demonitor(true) => "demonitor true",
                    Ev::Down(_, status) => {
                        assert_eq!(*status, ActorStatus::Stopping, "{events:?}");
                        "down"
                    }
                    Ev::Stopped => "stopped",
                    Ev::Child => "child",
                })
                .collect();
            assert_eq!(
                shape,
                ["demonitor false", "down", "stopped"],
                "the notice is delivered once at the stop: {events:?}"
            );
            return;
        }
        panic!("no round took the notice before the demonitor");
    });
}
