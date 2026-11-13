#![allow(dead_code)]
//! The exit signal an actor sends with `ctx.exit`: what a target that does not
//! trap ends with, what the stop lane does with the reason, and how a target
//! that never reaches a turn boundary is still ended.
//!
//! Every test drives real actors. A `Peer` can be hung inside a handler,
//! trap exit signals, and send an exit signal to a handle, an id, a name or
//! itself; it logs every `on_stopped` it runs. Each test runs on the flavor
//! `TOKIO_ACTORS_TEST_FLAVOR` selects.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{oneshot, Notify};
use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorId, ActorResult, ActorSystem, Alias, AliasOptions,
    AskError, LinkExit, RestartType, Shutdown, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);
const GRACE: Duration = Duration::from_millis(100);
/// What a peer answers when the exit signal it was asked to send was refused.
const REFUSED: u32 = 0;

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

fn uname(base: &str) -> String {
    format!("xs-{base}-{}", uuid::Uuid::new_v4())
}

fn system() -> Arc<ActorSystem> {
    ActorSystem::create(uname("sys")).unwrap()
}

// ---------------------------------------------------------------------------
// The actor under test
// ---------------------------------------------------------------------------

enum Msg {
    Ping,
    /// Signals that the handler started, then never yields.
    Hang(Arc<Notify>),
    Trap,
    Untrap,
    /// Tells `started`, then holds the turn until `release` (or the bound).
    Park(Arc<Notify>, Arc<Notify>),
    /// Makes an alias with these options and hands it to the caller.
    MintAlias(AliasOptions, oneshot::Sender<Alias<Peer>>),
    /// A trapped exit signal arrived.
    Trapped(#[allow(dead_code)] LinkExit),
    ExitHandle(ActorHandle<Peer>, StopReason),
    ExitId(ActorId, StopReason),
    ExitName(String, StopReason),
    ExitSelf(StopReason),
    /// Sends through an alias with `exit_priority`; answers `REFUSED` when the
    /// signal is refused.
    ExitAlias(Alias<Peer>, StopReason),
    /// As `ExitId`, answering `REFUSED` when the signal is refused.
    ExitIdTry(ActorId, StopReason),
    /// Sets the flag and signals itself in the same turn.
    TrapThenExitSelf(StopReason),
    /// Clears the flag and signals itself in the same turn.
    UntrapThenExitSelf(StopReason),
}

#[derive(Clone, Default)]
struct Probe {
    /// The reason of every `on_stopped` that ran.
    stopped: Arc<Mutex<Vec<StopReason>>>,
    /// Trapped signals delivered to `handle`.
    trapped: Arc<AtomicUsize>,
    /// Told once per trapped signal delivered to `handle`.
    trapped_note: Arc<Notify>,
    /// Incarnations started by the factory.
    starts: Arc<AtomicUsize>,
    /// When set, `on_stopped` tells `entered` and waits for `release`.
    stop_gate: Option<(Arc<Notify>, Arc<Notify>)>,
}

struct Peer {
    probe: Probe,
}

impl Actor for Peer {
    type Message = Msg;
    type Response = u32;

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        match msg {
            Msg::Ping => {}
            Msg::Hang(started) => {
                started.notify_one();
                std::future::pending::<()>().await;
            }
            Msg::Trap => ctx.trap_exit(Msg::Trapped),
            Msg::Untrap => ctx.untrap_exit(),
            Msg::Park(started, release) => {
                started.notify_one();
                let _ = timeout(BOUND, release.notified()).await;
            }
            Msg::MintAlias(options, reply) => {
                let _ = reply.send(ctx.alias(options));
            }
            Msg::ExitAlias(alias, reason) => {
                if ctx.exit_priority(&alias, reason).is_err() {
                    return Ok(REFUSED);
                }
            }
            Msg::ExitIdTry(id, reason) => {
                if ctx.exit(&id, reason).is_err() {
                    return Ok(REFUSED);
                }
            }
            Msg::Trapped(_) => {
                self.probe.trapped.fetch_add(1, Ordering::SeqCst);
                self.probe.trapped_note.notify_one();
            }
            Msg::ExitHandle(target, reason) => {
                if ctx.exit(&target, reason).is_err() {
                    return Ok(REFUSED);
                }
            }
            Msg::ExitId(id, reason) => ctx
                .exit(&id, reason)
                .expect("an exit signal is refused only at its target's bound"),
            Msg::ExitName(name, reason) => ctx
                .exit(name.as_str(), reason)
                .expect("an exit signal is refused only at its target's bound"),
            Msg::ExitSelf(reason) => {
                let me = ctx.actor_id().clone();
                ctx.exit(&me, reason)
                    .expect("an exit signal is refused only at its target's bound");
            }
            Msg::TrapThenExitSelf(reason) => {
                ctx.trap_exit(Msg::Trapped);
                let me = ctx.actor_id().clone();
                ctx.exit(&me, reason)
                    .expect("an exit signal is refused only at its target's bound");
            }
            Msg::UntrapThenExitSelf(reason) => {
                ctx.untrap_exit();
                let me = ctx.actor_id().clone();
                ctx.exit(&me, reason)
                    .expect("an exit signal is refused only at its target's bound");
            }
        }
        Ok(7)
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.probe.stopped.lock().unwrap().push(reason.clone());
        if let Some((entered, release)) = &self.probe.stop_gate {
            entered.notify_one();
            release.notified().await;
        }
        Ok(())
    }
}

async fn spawn_peer(sys: &Arc<ActorSystem>, grace: Duration) -> (ActorHandle<Peer>, Probe) {
    let probe = Probe::default();
    let handle = Peer {
        probe: probe.clone(),
    }
    .spawn()
    .on_system(sys)
    .kill_grace(grace)
    .await
    .unwrap();
    (handle, probe)
}

/// A peer whose mailbox holds `capacity` messages.
async fn spawn_sized(sys: &Arc<ActorSystem>, capacity: usize) -> (ActorHandle<Peer>, Probe) {
    let probe = Probe::default();
    let handle = Peer {
        probe: probe.clone(),
    }
    .spawn()
    .on_system(sys)
    .with_config(ActorConfig::default().with_mailbox_capacity(capacity))
    .await
    .unwrap();
    (handle, probe)
}

/// Waits until `n` trapped signals have reached `handle`.
async fn delivered(probe: &Probe, n: usize) {
    within("the trapped exits to be handled", async {
        while probe.trapped.load(Ordering::SeqCst) < n {
            probe.trapped_note.notified().await;
        }
    })
    .await;
}

/// A peer stuck inside a handler: it started the `Hang` and never yields, and
/// the call that carries it is still pending.
async fn spawn_hung(
    sys: &Arc<ActorSystem>,
    grace: Duration,
) -> (
    ActorHandle<Peer>,
    Probe,
    tokio::task::JoinHandle<Result<u32, AskError>>,
) {
    let (handle, probe) = spawn_peer(sys, grace).await;
    let started = Arc::new(Notify::new());
    let call = {
        let (handle, started) = (handle.clone(), started.clone());
        tokio::spawn(async move { handle.send(Msg::Hang(started)).await })
    };
    within("the handler to start", started.notified()).await;
    (handle, probe, call)
}

fn failure(text: &str) -> StopReason {
    StopReason::Failure(ActorError::user(text))
}

/// The reasons an exit signal can carry that end a target that does not trap.
fn signal_reasons() -> Vec<(&'static str, StopReason)> {
    vec![
        ("failure by Err", failure("signalled")),
        ("parent request", StopReason::ParentRequest),
        ("shutdown", StopReason::Shutdown("bye".into())),
        ("cancelled", StopReason::Cancelled),
        ("parent killed", StopReason::ParentKilled),
    ]
}

// ---------------------------------------------------------------------------
// A target that does not trap
// ---------------------------------------------------------------------------

#[test]
fn non_trapping_target_ends_with_the_cause() {
    common::run(2, async {
        for (label, reason) in signal_reasons() {
            let sys = system();
            let (sender, _) = spawn_peer(&sys, GRACE).await;
            let (target, probe) = spawn_peer(&sys, GRACE).await;
            sender
                .send(Msg::ExitHandle(target.clone(), reason.clone()))
                .await
                .unwrap();
            let ended = within(label, target.wait_stopped()).await;
            assert_eq!(
                ended, reason,
                "{label}: the target ends with the signal's reason"
            );
            assert!(!target.is_alive(), "{label}");
            assert!(
                probe.stopped.lock().unwrap().is_empty(),
                "{label}: a non-trapping target runs no on_stopped"
            );
            let reply = sender.send(Msg::Ping).await;
            assert!(reply.is_ok(), "{label}: the sender goes on");
        }
    });
}

#[test]
fn abort_path_reads_the_cause() {
    common::run(2, async {
        for (label, reason) in signal_reasons() {
            let sys = system();
            let (sender, _) = spawn_peer(&sys, GRACE).await;
            let (target, probe, call) = spawn_hung(&sys, GRACE).await;
            sender
                .send(Msg::ExitHandle(target.clone(), reason.clone()))
                .await
                .unwrap();
            let ended = within(label, target.wait_stopped()).await;
            assert_eq!(
                ended, reason,
                "{label}: the aborted target's plane reads the cause, not Kill"
            );
            match within(label, call).await.unwrap() {
                Err(AskError::ResponseDropped { reason: read }) => {
                    assert_eq!(read, reason, "{label}: the pending send carries the cause");
                }
                other => panic!("{label}: the pending send read {other:?}"),
            }
            assert!(probe.stopped.lock().unwrap().is_empty(), "{label}");
        }
    });
}

#[test]
fn kill_is_causeless_and_untrappable() {
    common::run(2, async {
        for trapping in [false, true] {
            let sys = system();
            let (sender, _) = spawn_peer(&sys, GRACE).await;
            let (target, probe) = spawn_peer(&sys, GRACE).await;
            if trapping {
                target.send(Msg::Trap).await.unwrap();
            }
            sender
                .send(Msg::ExitHandle(target.clone(), StopReason::Kill))
                .await
                .unwrap();
            let ended = within("the target", target.wait_stopped()).await;
            assert_eq!(ended, StopReason::Kill, "trapping {trapping}");
            assert!(
                probe.stopped.lock().unwrap().is_empty(),
                "trapping {trapping}"
            );
        }
    });
}

#[test]
fn kill_to_a_stuck_target_is_backstopped() {
    common::run(2, async {
        let sys = system();
        let (sender, _) = spawn_peer(&sys, GRACE).await;
        let (target, _probe, call) = spawn_hung(&sys, GRACE).await;
        sender
            .send(Msg::ExitHandle(target.clone(), StopReason::Kill))
            .await
            .unwrap();
        assert_eq!(
            within("the stuck target", target.wait_stopped()).await,
            StopReason::Kill
        );
        assert!(matches!(
            within("the pending send", call).await.unwrap(),
            Err(AskError::ResponseDropped {
                reason: StopReason::Kill
            })
        ));
    });
}

#[test]
fn graceful_to_another_is_ignored_and_to_oneself_kills() {
    common::run(2, async {
        let sys = system();
        let (sender, _) = spawn_peer(&sys, GRACE).await;
        let (other, _) = spawn_peer(&sys, GRACE).await;
        sender
            .send(Msg::ExitHandle(other.clone(), StopReason::Graceful))
            .await
            .unwrap();
        assert!(
            other.send(Msg::Ping).await.is_ok(),
            "a graceful signal to another actor does nothing"
        );
        assert!(other.is_alive());

        let (me, probe) = spawn_peer(&sys, GRACE).await;
        let _ = me.send(Msg::ExitSelf(StopReason::Graceful)).await;
        let ended = within("the actor that signalled itself", me.wait_stopped()).await;
        assert_eq!(
            ended,
            StopReason::Graceful,
            "a graceful signal to oneself ends a non-trapping actor with it"
        );
        assert!(
            probe.stopped.lock().unwrap().is_empty(),
            "and runs no on_stopped"
        );
    });
}

#[test]
fn explicit_kill_clears_the_cause_and_wins() {
    common::run(2, async {
        let grace = Duration::from_millis(200);
        for cause_first in [true, false] {
            let sys = system();
            let (sender, _) = spawn_peer(&sys, grace).await;
            let (target, _probe, _call) = spawn_hung(&sys, grace).await;
            if cause_first {
                sender
                    .send(Msg::ExitHandle(target.clone(), failure("cause")))
                    .await
                    .unwrap();
                target.stop(StopReason::Kill).await.unwrap();
            } else {
                target.stop(StopReason::Kill).await.unwrap();
                sender
                    .send(Msg::ExitHandle(target.clone(), failure("cause")))
                    .await
                    .unwrap();
            }
            let ended = within("the target", target.wait_stopped()).await;
            assert_eq!(
                ended,
                StopReason::Kill,
                "cause first {cause_first}: an explicit Kill takes no cause"
            );
        }
    });
}

#[test]
fn second_cause_does_not_replace_the_first() {
    common::run(2, async {
        let sys = system();
        let (sender, _) = spawn_peer(&sys, Duration::from_millis(200)).await;
        let (target, _probe, _call) = spawn_hung(&sys, Duration::from_millis(200)).await;
        for reason in [failure("first"), StopReason::Cancelled] {
            sender
                .send(Msg::ExitHandle(target.clone(), reason))
                .await
                .unwrap();
        }
        let ended = within("the target", target.wait_stopped()).await;
        assert_eq!(ended, failure("first"));
    });
}

// ---------------------------------------------------------------------------
// A target that traps, and the flag read when the signal is sent
// ---------------------------------------------------------------------------

#[test]
fn trapping_target_is_not_ended_by_a_signal() {
    common::run(2, async {
        let reasons = [
            StopReason::Graceful,
            failure("trapped"),
            StopReason::Shutdown("x".into()),
            StopReason::ParentRequest,
        ];
        for reason in reasons {
            let sys = system();
            let (sender, _) = spawn_peer(&sys, GRACE).await;
            let (target, probe) = spawn_peer(&sys, GRACE).await;
            target.send(Msg::Trap).await.unwrap();
            sender
                .send(Msg::ExitHandle(target.clone(), reason.clone()))
                .await
                .unwrap();
            assert!(
                target.send(Msg::Ping).await.is_ok(),
                "{reason:?}: a trapping target goes on"
            );
            assert!(target.is_alive(), "{reason:?}");
            assert!(probe.stopped.lock().unwrap().is_empty(), "{reason:?}");
        }
        // Exit signals delivered one at a time never reach the bound of the
        // signals waiting for a trapping actor (its mailbox capacity, 64):
        // each one's count is given back once its delivery takes a mailbox
        // slot.
        let sys = system();
        let (sender, _) = spawn_peer(&sys, GRACE).await;
        let (target, probe) = spawn_peer(&sys, GRACE).await;
        target.send(Msg::Trap).await.unwrap();
        for n in 1..=129 {
            let answer = sender
                .send(Msg::ExitHandle(target.clone(), failure("one at a time")))
                .await
                .unwrap();
            assert_ne!(answer, REFUSED, "exit {n} was refused");
            within("the trapped exit", probe.trapped_note.notified()).await;
        }
        assert_eq!(probe.trapped.load(Ordering::SeqCst), 129);

        // Signals that wait while the target's loop is parked are bounded by
        // its mailbox capacity: the next call is refused and sends nothing,
        // the target is handed exactly the accepted ones, and the count is
        // given back, so a second spell accepts the full bound again. The
        // bound is shared by `exit` and `exit_priority`.
        const CAPACITY: usize = 3;
        let rows = [
            ("handle", None),
            ("alias", Some(AliasOptions::default())),
            (
                "priority alias",
                Some(AliasOptions {
                    priority: true,
                    ..AliasOptions::default()
                }),
            ),
        ];
        for (row, options) in rows {
            let sys = system();
            let (sender, _) = spawn_peer(&sys, GRACE).await;
            let (target, probe) = spawn_sized(&sys, CAPACITY).await;
            target.send(Msg::Trap).await.unwrap();
            let alias = match options {
                None => None,
                Some(options) => {
                    let (tx, rx) = oneshot::channel();
                    target.send(Msg::MintAlias(options, tx)).await.unwrap();
                    Some(within("the alias", rx).await.unwrap())
                }
            };
            let signal = || match &alias {
                None => Msg::ExitHandle(target.clone(), failure("queued")),
                Some(alias) => Msg::ExitAlias(alias.clone(), failure("queued")),
            };
            for spell in 1..=2 {
                let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
                target
                    .notify(Msg::Park(started.clone(), release.clone()))
                    .await
                    .unwrap();
                within("the target to park", started.notified()).await;
                for n in 1..=CAPACITY {
                    assert_ne!(
                        sender.send(signal()).await.unwrap(),
                        REFUSED,
                        "{row}, spell {spell}: signal {n} is within the bound"
                    );
                }
                assert_eq!(
                    sender.send(signal()).await.unwrap(),
                    REFUSED,
                    "{row}, spell {spell}: the signal over the bound is refused"
                );
                release.notify_one();
                delivered(&probe, CAPACITY * spell).await;
                target.send(Msg::Ping).await.unwrap();
                assert_eq!(
                    probe.trapped.load(Ordering::SeqCst),
                    CAPACITY * spell,
                    "{row}, spell {spell}: the target is handed the accepted signals and not the refused one"
                );
                assert!(target.is_alive(), "{row}: a refused signal ends nothing");
            }
        }

        // A supervisor traps from its creation without having called
        // `trap_exit`: its bound is 64.
        let sys = system();
        let (sender, _) = spawn_peer(&sys, GRACE).await;
        let boss = Warden {
            asked: Arc::new(Notify::new()),
            exits: Arc::default(),
        }
        .spawn()
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        for spell in 1..=2 {
            let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
            boss.notify(WardenMsg::Park(started.clone(), release.clone()))
                .await
                .unwrap();
            within("the supervisor to park", started.notified()).await;
            for n in 1..=64 {
                let answer = sender
                    .send(Msg::ExitIdTry(boss.id().clone(), failure("queued")))
                    .await
                    .unwrap();
                assert_ne!(answer, REFUSED, "spell {spell}: signal {n} is within 64");
            }
            let answer = sender
                .send(Msg::ExitIdTry(boss.id().clone(), failure("queued")))
                .await
                .unwrap();
            assert_eq!(answer, REFUSED, "spell {spell}: signal 65 is refused");
            release.notify_one();
            within("the supervisor to go on", boss.send(WardenMsg::Ping))
                .await
                .unwrap();
        }
        assert!(boss.is_alive(), "signals to a supervisor end nothing");
    });
}

/// A child that vetoes every orderly stop.
struct Stubborn {
    asked: Arc<Notify>,
}

impl Actor for Stubborn {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.asked.notify_one();
        false
    }
}

enum WardenMsg {
    /// Links to the peer.
    Link(ActorHandle<Peer>),
    /// Traps exit signals, mapping each to `Trapped`.
    Map,
    Trapped(LinkExit),
    /// Tells `started`, then holds the turn until `release` (or the bound).
    Park(Arc<Notify>, Arc<Notify>),
    Ping,
}

/// A supervisor of one `Stubborn` child whose `Shutdown` is an hour. It
/// traps exit signals from its creation, as every supervisor does, and
/// records the exits its map is handed.
struct Warden {
    asked: Arc<Notify>,
    exits: Arc<Mutex<Vec<LinkExit>>>,
}

impl Actor for Warden {
    type Message = WardenMsg;
    type Response = ();

    async fn handle(&mut self, msg: WardenMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            WardenMsg::Link(peer) => ctx.link(&peer).expect("the supervisor is running"),
            WardenMsg::Map => ctx.trap_exit(WardenMsg::Trapped),
            WardenMsg::Trapped(exit) => self.exits.lock().unwrap().push(exit),
            WardenMsg::Park(started, release) => {
                started.notify_one();
                let _ = timeout(BOUND, release.notified()).await;
            }
            WardenMsg::Ping => {}
        }
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let asked = self.asked.clone();
        ctx.spawn_child(move || Stubborn {
            asked: asked.clone(),
        })
        .shutdown(Shutdown::Timeout(Duration::from_secs(3600)))
        .await?;
        Ok(())
    }
}

/// A supervisor traps from its creation, so an exit signal never lands a
/// cause on its stop lane: a linked peer's end reaches it as a `LinkExit`, an
/// `exit` with a failure to it is queued as a message and ends nothing, and
/// `exit(sup, Kill)` ends it as `Kill` with the teardown of a plain `Kill`:
/// the wait for a child that vetoes its stop ends at the signal, with no abort
/// to backstop it.
#[test]
fn a_supervisor_gets_exits_as_messages_and_a_kill_ends_it_like_a_plain_kill() {
    common::run(2, async {
        // The kill grace is an hour, so no abort backstops the signal: the
        // supervisor's wait for its child ends only because it reads the
        // `Kill` on its stop lane.
        let sys = system();
        let (sender, _) = spawn_peer(&sys, GRACE).await;
        let (other, _) = spawn_peer(&sys, GRACE).await;
        let asked = Arc::new(Notify::new());
        let exits = Arc::new(Mutex::new(Vec::new()));
        let boss = Warden {
            asked: asked.clone(),
            exits: exits.clone(),
        }
        .spawn()
        .on_system(&sys)
        .kill_grace(Duration::from_secs(3600))
        .supervisor()
        .await
        .unwrap();
        boss.send(WardenMsg::Map).await.unwrap();
        boss.send(WardenMsg::Link(sender.clone())).await.unwrap();

        // A linked peer's end reaches the supervisor as a `LinkExit`.
        let _ = sender.send(Msg::ExitSelf(failure("peer"))).await;
        within("the peer's end", sender.wait_stopped()).await;
        let deadline = Instant::now() + BOUND;
        while exits.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "no exit reached the supervisor");
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(exits.lock().unwrap().len(), 1);
        assert!(boss.is_alive(), "the supervisor goes on");

        // The teardown begins and the child vetoes its stop.
        boss.stop(StopReason::Graceful).await.unwrap();
        within("the child to be asked to stop", asked.notified()).await;

        // A failure signal to the supervisor is a message: the teardown goes
        // on waiting.
        other
            .send(Msg::ExitId(boss.id().clone(), failure("teardown")))
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(300), boss.wait_stopped())
                .await
                .is_err(),
            "a failure signal does not end a supervisor"
        );

        // A `Kill` signal is a plain `Kill`.
        let started = std::time::Instant::now();
        other
            .send(Msg::ExitId(boss.id().clone(), StopReason::Kill))
            .await
            .unwrap();
        assert_eq!(
            within("the supervisor", boss.wait_stopped()).await,
            StopReason::Kill
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait for the child ended at the signal"
        );
    });
}

// ---------------------------------------------------------------------------
// A supervised child that receives a signal
// ---------------------------------------------------------------------------

struct Boss {
    probe: Probe,
    name: String,
}

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let probe = self.probe.clone();
        ctx.spawn_child(move || {
            probe.starts.fetch_add(1, Ordering::SeqCst);
            Peer {
                probe: probe.clone(),
            }
        })
        .named(self.name.clone())
        .restart_type(RestartType::Transient)
        .await?;
        Ok(())
    }
}

#[test]
fn transient_child_killed_by_a_shutdown_cause_is_not_restarted() {
    common::run(2, async {
        let sys = system();
        let probe = Probe::default();
        let name = uname("kid");
        let _boss = Boss {
            probe: probe.clone(),
            name: name.clone(),
        }
        .spawn()
        .on_system(&sys)
        .with_config(ActorConfig::default().supervisor())
        .await
        .unwrap();
        let (sender, _) = spawn_peer(&sys, GRACE).await;
        let kid = sys.get::<Peer>(&name).expect("the child is registered");
        assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
        sender
            .send(Msg::ExitName(
                name.clone(),
                StopReason::Shutdown("done".into()),
            ))
            .await
            .unwrap();
        assert_eq!(
            within("the child", kid.wait_stopped()).await,
            StopReason::Shutdown("done".into())
        );
        sleep(Duration::from_millis(400)).await;
        assert_eq!(
            probe.starts.load(Ordering::SeqCst),
            1,
            "a Transient child that ended with shutdown is not restarted"
        );
    });
}
