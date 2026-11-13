#![allow(dead_code)]
//! What a group or scope monitor tells its watcher: the members at the start
//! and the changes after them, in order; one `Resync` for each spell of
//! overflow, carrying the table as it is; what a revoke keeps and drops;
//! what the watcher's end drops; and the watcher that a panicking map fails.
//!
//! Every test drives real actors in a system of its own. The copy of a
//! `Resync` outside the scope's lock and the containment of the wake are
//! pinned by the tests in `src/actor/pg_tests.rs`, which can see the lock and
//! the wake.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorHandle, ActorResult, ActorSystem, Down, GroupChange, MonitorId, PeerRef,
    PgScope,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(300);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn system(base: &str) -> Arc<ActorSystem> {
    ActorSystem::create(format!(
        "pgmon-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ))
    .expect("a fresh system name")
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

// ---------------------------------------------------------------------------
// The actors
// ---------------------------------------------------------------------------

/// A plain member.
struct Member;

impl Actor for Member {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Holds the map function of a group monitor inside a change until the test
/// lets it go: the entry task has taken the change and not yet counted it.
struct MapGate {
    entered: Mutex<std::sync::mpsc::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

enum W {
    /// Monitors a group with a map that waits on the gate.
    GatedGroup(PgScope, &'static str, Arc<MapGate>),
    /// Monitors a group.
    Group(PgScope, &'static str),
    /// Monitors a scope.
    Scope(PgScope),
    /// Monitors a group with a map that panics.
    PanickyGroup(PgScope, &'static str),
    /// Monitors an actor's death.
    Death(ActorHandle<Member>),
    Demonitor(MonitorId),
    DemonitorFlush(MonitorId),
    /// Holds the turn until the gate has a permit.
    Hold(Arc<Semaphore>),
    /// Holds the turn the same way, then revokes the monitor in the same
    /// turn, so every change that reached the mailbox meanwhile is queued
    /// when it does; answers what the revoke returned.
    HoldThenRevoke(Arc<Semaphore>, MonitorId, bool),
    Change(GroupChange),
    Down(Down),
}

#[derive(Debug)]
enum Reply {
    Unit,
    Group(MonitorId, Vec<PeerRef>),
    Scope(MonitorId, Vec<(Arc<str>, Vec<PeerRef>)>),
    Id(MonitorId),
    Flag(bool),
}

#[derive(Debug)]
enum Ev {
    Change(GroupChange),
    Down(Down),
}

struct Watcher {
    log: mpsc::UnboundedSender<Ev>,
}

impl Actor for Watcher {
    type Message = W;
    type Response = Reply;

    async fn handle(&mut self, msg: W, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        Ok(match msg {
            W::Group(scope, group) => {
                let (id, members) = ctx
                    .monitor_group(&scope, group, W::Change)
                    .expect("running");
                Reply::Group(id, members)
            }
            W::Scope(scope) => {
                let (id, groups) = ctx.monitor_scope(&scope, W::Change).expect("running");
                Reply::Scope(id, groups)
            }
            W::GatedGroup(scope, group, gate) => {
                let (id, _) = ctx
                    .monitor_group(&scope, group, move |change| {
                        let _ = gate.entered.lock().unwrap().send(());
                        let _ = gate.release.lock().unwrap().recv();
                        W::Change(change)
                    })
                    .expect("running");
                Reply::Id(id)
            }
            W::PanickyGroup(scope, group) => {
                let (id, _) = ctx
                    .monitor_group(&scope, group, |_| panic!("the map function failed"))
                    .expect("running");
                Reply::Id(id)
            }
            W::Death(target) => Reply::Id(ctx.monitor(&target, W::Down).expect("running")),
            W::Demonitor(id) => Reply::Flag(ctx.demonitor(id)),
            W::DemonitorFlush(id) => Reply::Flag(ctx.demonitor_flush(id)),
            W::Hold(gate) => {
                let _ = gate.acquire().await;
                Reply::Unit
            }
            W::HoldThenRevoke(gate, id, flush) => {
                let _ = gate.acquire().await;
                Reply::Flag(if flush {
                    ctx.demonitor_flush(id)
                } else {
                    ctx.demonitor(id)
                })
            }
            W::Change(change) => {
                let _ = self.log.send(Ev::Change(change));
                Reply::Unit
            }
            W::Down(down) => {
                let _ = self.log.send(Ev::Down(down));
                Reply::Unit
            }
        })
    }
}

type Log = mpsc::UnboundedReceiver<Ev>;

async fn watcher(system: &Arc<ActorSystem>, capacity: usize) -> (ActorHandle<Watcher>, Log) {
    let (log, rx) = mpsc::unbounded_channel();
    let handle = Watcher { log }
        .spawn()
        .on_system(system)
        .with_config(ActorConfig::default().with_mailbox_capacity(capacity))
        .await
        .expect("the watcher spawns");
    (handle, rx)
}

async fn member(system: &Arc<ActorSystem>) -> ActorHandle<Member> {
    Member.spawn().on_system(system).await.expect("spawns")
}

fn peer(handle: &ActorHandle<Member>) -> PeerRef {
    PeerRef {
        actor_id: handle.id().clone(),
        incarnation: handle.incarnation(),
    }
}

fn sorted(mut peers: Vec<PeerRef>) -> Vec<PeerRef> {
    peers.sort_by(|a, b| {
        (a.actor_id.as_str(), a.incarnation).cmp(&(b.actor_id.as_str(), b.incarnation))
    });
    peers
}

async fn next_change(log: &mut Log) -> GroupChange {
    loop {
        match within("a change", log.recv()).await {
            Some(Ev::Change(change)) => return change,
            Some(Ev::Down(_)) => {}
            None => panic!("the watcher's log closed"),
        }
    }
}

async fn assert_quiet(log: &mut Log) {
    sleep(QUIET).await;
    if let Ok(event) = log.try_recv() {
        panic!("unexpected event after the quiet window: {event:?}");
    }
}

/// Reads what arrives until nothing has for a quiet window.
async fn drain(log: &mut Log) -> Vec<Ev> {
    let mut out = Vec::new();
    while let Ok(Some(event)) = timeout(QUIET, log.recv()).await {
        out.push(event);
    }
    out
}

/// Reads events until `done` holds for what has arrived (under the hang
/// bound), then until nothing has arrived for a quiet window, so an extra
/// event after the expected ones still shows.
async fn collect_until(log: &mut Log, what: &str, done: impl Fn(&[Ev]) -> bool) -> Vec<Ev> {
    let mut out = Vec::new();
    within(what, async {
        while !done(&out) {
            match log.recv().await {
                Some(event) => out.push(event),
                None => panic!("the watcher's log closed"),
            }
        }
    })
    .await;
    out.extend(drain(log).await);
    out
}

async fn group_monitor(
    watcher: &ActorHandle<Watcher>,
    scope: &PgScope,
    group: &'static str,
) -> (MonitorId, Vec<PeerRef>) {
    match watcher.send(W::Group(scope.clone(), group)).await.unwrap() {
        Reply::Group(id, members) => (id, members),
        other => panic!("a group monitor was expected, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// What a watcher knows of a group after the changes it was told: replaying
/// them over the members it started with.
fn replay(start: Vec<PeerRef>, changes: &[GroupChange]) -> Vec<PeerRef> {
    let mut state = start;
    for change in changes {
        match change {
            GroupChange::Join { members, .. } => state.extend(members.iter().cloned()),
            GroupChange::Leave { members, .. } => {
                for gone in members {
                    let at = state
                        .iter()
                        .position(|have| have == gone)
                        .expect("a leave names a member the watcher knows");
                    state.swap_remove(at);
                }
            }
            GroupChange::Resync { groups, .. } => state = groups[0].1.clone(),
        }
    }
    state
}

/// A watcher whose mailbox is held full while the group changes a thousand
/// times loses no change silently: the changes that did not fit are covered by
/// one `Resync` for the spell, replaying what the watcher was told gives the
/// table, and a second spell gets its own `Resync`.
#[test]
fn overflow_gives_one_resync_per_spell() {
    common::run(4, async {
        let system = system("overflow");
        let scope = system.pg();
        let (w, mut log) = watcher(&system, 4).await;
        let members: Vec<_> = {
            let mut out = Vec::new();
            for _ in 0..20 {
                out.push(member(&system).await);
            }
            out
        };
        let (_id, start) = group_monitor(&w, &scope, "g").await;
        assert!(start.is_empty());
        let mut told = Vec::new();
        let mut resyncs = 0;
        for spell in 0..2 {
            let gate = Arc::new(Semaphore::new(0));
            let held = {
                let (w, gate) = (w.clone(), gate.clone());
                tokio::spawn(async move { w.send(W::Hold(gate)).await })
            };
            sleep(Duration::from_millis(40)).await;
            for member in &members {
                for _ in 0..50 {
                    scope.join("g", member).unwrap();
                }
            }
            if spell == 1 {
                // The second spell also shrinks the group.
                for member in members.iter().take(10) {
                    for _ in 0..50 {
                        scope.leave("g", member).unwrap();
                    }
                }
            }
            gate.add_permits(1);
            held.await.unwrap().unwrap();
            let mut count_here = 0;
            let told_before = told.clone();
            let events = collect_until(&mut log, "the spell's resync and its changes", |seen| {
                let mut all = told_before.clone();
                let mut resynced = false;
                for event in seen {
                    if let Ev::Change(change) = event {
                        resynced |= matches!(change, GroupChange::Resync { .. });
                        all.push(change.clone());
                    }
                }
                resynced && sorted(replay(Vec::new(), &all)) == sorted(scope.members_any("g"))
            })
            .await;
            for event in events {
                if let Ev::Change(change) = event {
                    if matches!(change, GroupChange::Resync { .. }) {
                        resyncs += 1;
                        count_here += 1;
                    }
                    told.push(change);
                }
            }
            assert_eq!(count_here, 1, "spell {spell}: one Resync for the spell");
            let table = scope.members_any("g");
            assert_eq!(
                sorted(replay(Vec::new(), &told)),
                sorted(table),
                "spell {spell}: what the watcher was told is not the table"
            );
        }
        assert_eq!(resyncs, 2);
    });
}

/// A revoke with `demonitor_flush` drops the changes already in the watcher's
/// mailbox and says so; `demonitor` keeps them; with nothing queued either says
/// `true`; neither lets a later change through.
#[test]
fn demonitor_flush_drops_queued_changes() {
    common::run(4, async {
        revoke_with_changes_queued(true).await;
    });
}

/// See [`demonitor_flush_drops_queued_changes`].
#[test]
fn demonitor_keeps_them() {
    common::run(4, async {
        revoke_with_changes_queued(false).await;
    });
}

async fn revoke_with_changes_queued(flush: bool) {
    let system = system(if flush { "flush" } else { "keep" });
    let scope = system.pg();
    let (w, mut log) = watcher(&system, 64).await;
    let (a, b, c, later) = (
        member(&system).await,
        member(&system).await,
        member(&system).await,
        member(&system).await,
    );

    // Nothing queued: the revoke is in time, and says so.
    let (idle, _) = group_monitor(&w, &scope, "g").await;
    let reply = if flush {
        w.send(W::DemonitorFlush(idle)).await.unwrap()
    } else {
        w.send(W::Demonitor(idle)).await.unwrap()
    };
    assert!(matches!(reply, Reply::Flag(true)), "{reply:?}");
    // A revoked id, or one the watcher never made, is not a monitor.
    assert!(matches!(
        w.send(W::Demonitor(idle)).await.unwrap(),
        Reply::Flag(false)
    ));
    scope.join("g", &later).unwrap();
    assert_quiet(&mut log).await;
    scope.leave("g", &later).unwrap();

    // Three changes queued in the watcher's mailbox when it revokes.
    let (id, _) = group_monitor(&w, &scope, "g").await;
    let gate = Arc::new(Semaphore::new(0));
    let revoke = {
        let (w, gate) = (w.clone(), gate.clone());
        tokio::spawn(async move { w.send(W::HoldThenRevoke(gate, id, flush)).await })
    };
    sleep(Duration::from_millis(40)).await;
    for member in [&a, &b, &c] {
        scope.join("g", member).unwrap();
    }
    within("the changes reach the mailbox", async {
        while w.mailbox_len() < 3 {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    gate.add_permits(1);
    let reply = revoke.await.unwrap().unwrap();
    assert!(
        matches!(reply, Reply::Flag(false)),
        "changes were queued: {reply:?}"
    );
    let told = if flush {
        drain(&mut log).await
    } else {
        collect_until(&mut log, "the three kept changes", |seen| seen.len() >= 3).await
    };
    if flush {
        assert!(told.is_empty(), "flushed changes were delivered: {told:?}");
    } else {
        assert_eq!(told.len(), 3, "the queued changes were kept: {told:?}");
    }

    // Nothing after the revoke, in either case.
    scope.join("g", &later).unwrap();
    scope.leave("g", &a).unwrap();
    assert_quiet(&mut log).await;

    // A change the entry task holds in its map function when the revoke comes
    // is none: the answer is `true` and the change is never delivered. The map
    // function blocks its thread while it holds the change, so the step needs
    // a worker thread besides the one it holds.
    if tokio::runtime::Handle::current().runtime_flavor()
        != tokio::runtime::RuntimeFlavor::MultiThread
    {
        return;
    }
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let gate = Arc::new(MapGate {
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
    });
    let held = match w
        .send(W::GatedGroup(scope.clone(), "held", gate))
        .await
        .unwrap()
    {
        Reply::Id(id) => id,
        other => panic!("a monitor id was expected, got {other:?}"),
    };
    scope.join("held", &b).unwrap();
    within(
        "the map function is entered",
        tokio::task::spawn_blocking(move || entered_rx.recv()),
    )
    .await
    .unwrap()
    .unwrap();
    let reply = if flush {
        w.send(W::DemonitorFlush(held)).await.unwrap()
    } else {
        w.send(W::Demonitor(held)).await.unwrap()
    };
    assert!(
        matches!(reply, Reply::Flag(true)),
        "a change in the map's hands is none: {reply:?}"
    );
    release_tx.send(()).unwrap();
    assert_quiet(&mut log).await;
}
