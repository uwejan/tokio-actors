#![allow(dead_code)]
//! What a stopping watcher is owed: every notice of a monitor whose target
//! has ended is delivered once through `handle`, with the status reading
//! `Stopping`, the stop lane read before each, and none under a `Kill`.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorStatus, Down, DownReason, MonitorError,
    MonitorId, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(300);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("mstop-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
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

#[derive(Debug)]
enum Ev {
    Down(Down, ActorStatus),
    Data(u32),
    /// The result of a registration attempted in `handle` for a notice.
    Reg(Result<MonitorId, MonitorError>, ActorStatus),
    /// The result of a registration attempted in `on_stopped`.
    StoppedReg(Result<MonitorId, MonitorError>, ActorStatus),
    /// `on_stopped` ran.
    Stopped,
}

/// What the watcher does when a notice reaches `handle`.
#[derive(Default)]
struct GoneAction {
    kill_self: bool,
    revoke: Option<MonitorId>,
    try_monitor: Option<ActorHandle<Victim>>,
}

enum Cmd {
    Watch(ActorHandle<Victim>),
    WatchPanicMap(ActorHandle<Victim>),
    WatchSelf(ActorHandle<Watcher>),
    Wait(Arc<Semaphore>),
    /// Waits for the gate, then stops the actor from its own handler.
    WaitThenStop(Arc<Semaphore>),
    Data(u32),
    Gone(Down),
}

struct Watcher {
    log: mpsc::UnboundedSender<Ev>,
    action: Arc<Mutex<GoneAction>>,
    probe: Option<ActorHandle<Victim>>,
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
            Cmd::Watch(v) => Ok(Some(ctx.monitor(&v, Cmd::Gone).expect("running"))),
            Cmd::WatchPanicMap(v) => Ok(Some(
                ctx.monitor(&v, |_| -> Cmd { panic!("map boom") })
                    .expect("running"),
            )),
            Cmd::WatchSelf(me) => Ok(Some(ctx.monitor(&me, Cmd::Gone).expect("running"))),
            Cmd::Wait(gate) => {
                let _ = gate.acquire().await;
                Ok(None)
            }
            Cmd::WaitThenStop(gate) => {
                let _ = gate.acquire().await;
                ctx.stop(StopReason::Graceful).expect("the lane takes it");
                Ok(None)
            }
            Cmd::Data(n) => {
                let _ = self.log.send(Ev::Data(n));
                Ok(None)
            }
            Cmd::Gone(down) => {
                let status = ctx.status();
                let _ = self.log.send(Ev::Down(down, status));
                let (kill, revoke, try_monitor) = {
                    let mut action = self.action.lock().unwrap();
                    (
                        std::mem::take(&mut action.kill_self),
                        action.revoke.take(),
                        action.try_monitor.clone(),
                    )
                };
                if let Some(id) = revoke {
                    ctx.demonitor(id);
                }
                if let Some(v) = try_monitor {
                    let attempt = ctx.monitor(&v, Cmd::Gone);
                    let _ = self.log.send(Ev::Reg(attempt, ctx.status()));
                }
                if kill {
                    ctx.self_handle().stop(StopReason::Kill).await.ok();
                }
                Ok(None)
            }
        }
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.log.send(Ev::Stopped);
        if let Some(v) = &self.probe {
            let attempt = ctx.monitor(v, Cmd::Gone);
            let _ = self.log.send(Ev::StoppedReg(attempt, ctx.status()));
        }
        Ok(())
    }
}

struct Rig {
    w: ActorHandle<Watcher>,
    log: mpsc::UnboundedReceiver<Ev>,
    action: Arc<Mutex<GoneAction>>,
}

async fn rig(capacity: usize, supervisor: bool, probe: Option<ActorHandle<Victim>>) -> Rig {
    let (log, rx) = mpsc::unbounded_channel();
    let action = Arc::new(Mutex::new(GoneAction::default()));
    let mut builder = Watcher {
        log,
        action: action.clone(),
        probe,
    }
    .spawn()
    .named(uname("watcher"))
    .with_config(ActorConfig::default().with_mailbox_capacity(capacity));
    if supervisor {
        builder = builder.supervisor();
    }
    Rig {
        w: builder.await.unwrap(),
        log: rx,
        action,
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
    sleep(Duration::from_millis(100)).await;
    (gate, task)
}

fn drain(log: &mut mpsc::UnboundedReceiver<Ev>) -> Vec<Ev> {
    let mut out = Vec::new();
    while let Ok(ev) = log.try_recv() {
        out.push(ev);
    }
    out
}

fn downs(events: &[Ev]) -> Vec<&Down> {
    events
        .iter()
        .filter_map(|e| match e {
            Ev::Down(d, _) => Some(d),
            _ => None,
        })
        .collect()
}

/// Plain and supervisor watchers, a stop that is graceful or a `Kill`, with the
/// notices queued in the mailbox or still waiting for room: a graceful stop
/// delivers each once with `Stopping` visible, and a `Kill` delivers none.
#[test]
fn stage_by_watcher_kind_and_reason() {
    common::run_multi_thread_only(4, async {
        for supervisor in [false, true] {
            for queued in [true, false] {
                for kill in [false, true] {
                    let row = format!("supervisor {supervisor}, queued {queued}, kill {kill}");
                    // A full mailbox keeps the notices from queueing.
                    let capacity = if queued { 64 } else { 1 };
                    let mut rig = rig(capacity, supervisor, None).await;
                    let (a, b) = (victim().await, victim().await);
                    rig.w.send(Cmd::Watch(a.clone())).await.unwrap();
                    rig.w.send(Cmd::Watch(b.clone())).await.unwrap();
                    let (gate, parked) = park(&rig.w).await;
                    if !queued {
                        rig.w.notify(Cmd::Data(0)).await.unwrap();
                    }
                    end(&a).await;
                    end(&b).await;
                    sleep(Duration::from_millis(150)).await;
                    let reason = if kill {
                        StopReason::Kill
                    } else {
                        StopReason::Graceful
                    };
                    rig.w.stop(reason.clone()).await.unwrap();
                    gate.add_permits(1);
                    let _ = parked.await;
                    let ended = within("the watcher's end", rig.w.wait_stopped()).await;
                    assert_eq!(ended, reason, "{row}");
                    let events = drain(&mut rig.log);
                    if kill {
                        assert!(downs(&events).is_empty(), "{row}: a notice under Kill");
                    } else {
                        let got = downs(&events);
                        let ids: HashSet<_> = got.iter().map(|d| d.actor_id.clone()).collect();
                        assert_eq!(got.len(), 2, "{row}: {events:?}");
                        assert_eq!(ids.len(), 2, "{row}: a notice twice");
                        for ev in &events {
                            if let Ev::Down(d, status) = ev {
                                assert_eq!(*status, ActorStatus::Stopping, "{row}");
                                assert_eq!(d.reason, DownReason::Stopped(StopReason::Graceful));
                            }
                        }
                        assert!(
                            !events.iter().any(|e| matches!(e, Ev::Data(_))),
                            "{row}: a queued message was handled"
                        );
                    }
                }
            }
        }
    });
}

/// A notice already queued when the stop begins is delivered once, like an
/// owed one; the messages queued around it are dropped as at any stop.
#[test]
fn a_queued_notice_is_delivered_once_and_other_messages_are_dropped() {
    common::run_multi_thread_only(4, async {
        let mut rig = rig(64, false, None).await;
        let v = victim().await;
        rig.w.send(Cmd::Watch(v.clone())).await.unwrap();
        let (gate, parked) = park(&rig.w).await;
        rig.w.notify(Cmd::Data(1)).await.unwrap();
        end(&v).await;
        // The notice takes its slot behind the first message.
        within("the notice is queued", async {
            while rig.w.mailbox_len() < 2 {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        rig.w.notify(Cmd::Data(2)).await.unwrap();
        rig.w.stop(StopReason::Graceful).await.unwrap();
        gate.add_permits(1);
        let _ = parked.await;
        within("the end", rig.w.wait_stopped()).await;
        let events = drain(&mut rig.log);
        assert_eq!(downs(&events).len(), 1, "{events:?}");
        assert!(
            !events.iter().any(|e| matches!(e, Ev::Data(_))),
            "{events:?}"
        );
    });
}

/// A `Kill` raised by the first delivery stops the rest.
#[test]
fn lane_is_reread_before_each() {
    common::run_multi_thread_only(4, async {
        let mut rig = rig(1, false, None).await;
        let vs = [victim().await, victim().await, victim().await];
        for v in &vs {
            rig.w.send(Cmd::Watch(v.clone())).await.unwrap();
        }
        let (gate, parked) = park(&rig.w).await;
        rig.w.notify(Cmd::Data(0)).await.unwrap();
        for v in &vs {
            end(v).await;
        }
        sleep(Duration::from_millis(150)).await;
        rig.action.lock().unwrap().kill_self = true;
        rig.w.stop(StopReason::Graceful).await.unwrap();
        gate.add_permits(1);
        let _ = parked.await;
        let ended = within("the end", rig.w.wait_stopped()).await;
        assert_eq!(ended, StopReason::Kill);
        let events = drain(&mut rig.log);
        assert_eq!(downs(&events).len(), 1, "{events:?}");
    });
}

/// A map function that panics fails the watcher with that panic; another
/// watcher of the same target is unaffected.
#[test]
fn map_panic_fails_the_owner() {
    common::run(4, async {
        let bad = rig(64, false, None).await;
        let mut good = rig(64, false, None).await;
        let v = victim().await;
        bad.w.send(Cmd::WatchPanicMap(v.clone())).await.unwrap();
        good.w.send(Cmd::Watch(v.clone())).await.unwrap();
        end(&v).await;
        let ended = within("the failed owner", bad.w.wait_stopped()).await;
        assert_eq!(
            ended,
            StopReason::Failure(ActorError::Panic("map boom".into()))
        );
        match within("the other watcher's notice", good.log.recv())
            .await
            .unwrap()
        {
            Ev::Down(d, _) => assert_eq!(d.reason, DownReason::Stopped(StopReason::Graceful)),
            other => panic!("{other:?}"),
        }
        assert!(good.w.is_alive());
    });
}
