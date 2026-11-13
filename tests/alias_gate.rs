//! Aliases: a send through an alias checks its flag and enqueues under one
//! guard that `unalias` takes exclusively, so no message enters after
//! `unalias` returns true; a reply alias takes one message; the alias is the
//! only way to the priority lane when the gate is on; `exit_priority` rides
//! the lane only for an active priority alias, a trapping receiver and a lane.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorSystem, Alias, AliasError,
    AliasOptions, LinkExit, StopReason, SystemConfig,
};

const PROMPT: Duration = Duration::from_secs(20);

enum Msg {
    /// Makes an alias and hands it to the caller; the owner keeps it too.
    Mint(AliasOptions, oneshot::Sender<Alias<Owner>>),
    /// Ends the alias the owner minted `n`th.
    Unalias(usize, oneshot::Sender<bool>),
    /// Asks to end an alias another actor made.
    UnaliasForeign(Alias<Owner>, oneshot::Sender<bool>),
    Data(u32),
    /// Holds a turn until released.
    Block(oneshot::Sender<()>, oneshot::Receiver<()>),
    Exit(LinkExit),
    /// Links to the peer, with the priority flag when the bool is set.
    Link(ActorHandle<Peer>, bool),
    Sync,
}

#[derive(Default)]
struct Seen {
    handled: AtomicUsize,
    order: Mutex<Vec<String>>,
}

/// A trap map the test holds: it says it began and blocks its thread until the
/// test opens it (at most `PROMPT`).
#[derive(Default)]
struct MapHold {
    began: AtomicBool,
    open: AtomicBool,
}

impl MapHold {
    fn block(&self) {
        self.began.store(true, Ordering::SeqCst);
        let give_up = std::time::Instant::now() + PROMPT;
        while !self.open.load(Ordering::SeqCst) && std::time::Instant::now() < give_up {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

struct Owner {
    seen: Arc<Seen>,
    aliases: Vec<Alias<Owner>>,
    trap: bool,
    /// When set, the trap map blocks on this hold.
    slow_map: Option<Arc<MapHold>>,
}

impl Owner {
    fn new(seen: &Arc<Seen>) -> Self {
        Self {
            seen: seen.clone(),
            aliases: Vec::new(),
            trap: false,
            slow_map: None,
        }
    }
}

impl Actor for Owner {
    type Message = Msg;
    type Response = usize;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.trap {
            match self.slow_map.clone() {
                Some(hold) => ctx.trap_exit(move |exit| {
                    hold.block();
                    Msg::Exit(exit)
                }),
                None => ctx.trap_exit(Msg::Exit),
            }
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<usize> {
        match msg {
            Msg::Mint(options, reply) => {
                let alias = ctx.alias(options);
                self.aliases.push(alias.clone());
                let _ = reply.send(alias);
            }
            Msg::Unalias(n, reply) => {
                let ended = ctx.unalias(&self.aliases[n]);
                let _ = reply.send(ended);
            }
            Msg::UnaliasForeign(alias, reply) => {
                let _ = reply.send(ctx.unalias(&alias));
            }
            Msg::Data(n) => {
                self.seen.handled.fetch_add(1, Ordering::SeqCst);
                self.seen.order.lock().unwrap().push(n.to_string());
            }
            Msg::Block(entered, release) => {
                let _ = entered.send(());
                let _ = release.await;
            }
            Msg::Exit(exit) => {
                assert!(matches!(exit.reason, tokio_actors::DownReason::Stopped(_)));
                self.seen.order.lock().unwrap().push("exit".into());
            }
            Msg::Link(peer, priority) => {
                if priority {
                    ctx.link_priority(&peer).expect("the owner is running");
                } else {
                    ctx.link(&peer).expect("the owner is running");
                }
            }
            Msg::Sync => {}
        }
        Ok(self.seen.handled.load(Ordering::SeqCst))
    }
}

/// An actor the owner links to.
struct Peer;

impl Actor for Peer {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

struct Exiter;

impl Actor for Exiter {
    type Message = (Alias<Owner>, StopReason);
    type Response = ();

    async fn handle(
        &mut self,
        (alias, reason): (Alias<Owner>, StopReason),
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        ctx.exit_priority(&alias, reason)
            .expect("an exit signal is refused only at its target's bound");
        Ok(())
    }
}

async fn mint(owner: &ActorHandle<Owner>, options: AliasOptions) -> Alias<Owner> {
    let (tx, rx) = oneshot::channel();
    owner.notify(Msg::Mint(options, tx)).await.unwrap();
    timeout(PROMPT, rx).await.unwrap().unwrap()
}

async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

fn order(seen: &Seen) -> Vec<String> {
    seen.order.lock().unwrap().clone()
}

/// Senders race `unalias`: every message the owner handled was sent before
/// `unalias` returned, so the count it handled is the count of sends that were
/// accepted, and not one more.
#[test]
fn no_message_enters_after_unalias_returns_true() {
    common::run(4, async {
        for _ in 0..200 {
            let seen = Arc::new(Seen::default());
            let owner = Owner::new(&seen)
                .spawn()
                .with_config(ActorConfig::default().with_mailbox_capacity(4096))
                .await
                .unwrap();
            let alias = mint(&owner, AliasOptions::default()).await;
            let accepted = Arc::new(AtomicUsize::new(0));
            let senders: Vec<_> = (0..3)
                .map(|_| {
                    let (alias, accepted) = (alias.clone(), accepted.clone());
                    tokio::spawn(async move {
                        for n in 0..100_000 {
                            match alias.notify(Msg::Data(n)).await {
                                Ok(()) => {
                                    accepted.fetch_add(1, Ordering::SeqCst);
                                }
                                Err(AliasError::Inactive) => return,
                                Err(other) => panic!("{other:?}"),
                            }
                        }
                        panic!("the alias was never ended");
                    })
                })
                .collect();
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            let (tx, rx) = oneshot::channel();
            owner.notify(Msg::Unalias(0, tx)).await.unwrap();
            assert!(timeout(PROMPT, rx).await.unwrap().unwrap());
            for sender in senders {
                timeout(PROMPT, sender).await.unwrap().unwrap();
            }
            owner.send(Msg::Sync).await.unwrap();
            assert_eq!(
                seen.handled.load(Ordering::SeqCst),
                accepted.load(Ordering::SeqCst),
                "every handled message was an accepted send"
            );
        }
    });
}

/// `try_notify` answers `Full` when the mailbox has no room, and an actor's
/// `unalias` of an alias another actor made answers `false` and leaves the
/// alias active.
#[test]
fn a_full_mailbox_and_a_foreign_unalias_are_refused() {
    common::run(2, async {
        let seen = Arc::new(Seen::default());
        let owner = Owner::new(&seen)
            .spawn()
            .with_config(ActorConfig::default().with_mailbox_capacity(2))
            .await
            .unwrap();
        let other = Owner::new(&seen).spawn().await.unwrap();
        let alias = mint(&owner, AliasOptions::default()).await;
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        owner.notify(Msg::Block(entered, release_rx)).await.unwrap();
        entered_rx.await.unwrap();
        alias.try_notify(Msg::Data(0)).unwrap();
        alias.try_notify(Msg::Data(1)).unwrap();
        assert_eq!(alias.try_notify(Msg::Data(2)), Err(AliasError::Full));
        release.send(()).unwrap();
        owner.send(Msg::Sync).await.unwrap();
        assert_eq!(
            order(&seen),
            ["0", "1"],
            "the refused message was not queued"
        );

        let (tx, rx) = oneshot::channel();
        other
            .notify(Msg::UnaliasForeign(alias.clone(), tx))
            .await
            .unwrap();
        assert!(!timeout(PROMPT, rx).await.unwrap().unwrap());
        assert!(alias.is_active());
        alias.notify(Msg::Data(3)).await.unwrap();
    });
}

/// Messages queued before `unalias` stay and are handled; the next send is
/// refused.
#[test]
fn queued_messages_stay() {
    common::run(2, async {
        let seen = Arc::new(Seen::default());
        let owner = Owner::new(&seen).spawn().await.unwrap();
        let alias = mint(&owner, AliasOptions::default()).await;
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        owner.notify(Msg::Block(entered, release_rx)).await.unwrap();
        entered_rx.await.unwrap();
        for n in 0..3 {
            alias.try_notify(Msg::Data(n)).unwrap();
        }
        let (tx, rx) = oneshot::channel();
        owner.notify(Msg::Unalias(0, tx)).await.unwrap();
        release.send(()).unwrap();
        assert!(timeout(PROMPT, rx).await.unwrap().unwrap());
        assert_eq!(alias.notify(Msg::Data(9)).await, Err(AliasError::Inactive));
        assert_eq!(alias.try_notify(Msg::Data(9)), Err(AliasError::Inactive));
        owner.send(Msg::Sync).await.unwrap();
        assert_eq!(order(&seen), ["0", "1", "2"]);
    });
}

/// A reply alias takes exactly one message from racing senders and is not
/// active afterwards.
#[test]
fn a_reply_alias_accepts_one() {
    common::run(4, async {
        let seen = Arc::new(Seen::default());
        let owner = Owner::new(&seen).spawn().await.unwrap();
        let alias = mint(
            &owner,
            AliasOptions {
                reply: true,
                ..AliasOptions::default()
            },
        )
        .await;
        assert!(alias.is_active());
        let sends: Vec<_> = (0..8)
            .map(|n| {
                let alias = alias.clone();
                tokio::spawn(async move { alias.notify(Msg::Data(n)).await })
            })
            .collect();
        let mut accepted = 0;
        for send in sends {
            match timeout(PROMPT, send).await.unwrap().unwrap() {
                Ok(()) => accepted += 1,
                Err(AliasError::Inactive) => {}
                Err(other) => panic!("{other:?}"),
            }
        }
        assert_eq!(accepted, 1);
        owner.send(Msg::Sync).await.unwrap();
        assert_eq!(seen.handled.load(Ordering::SeqCst), 1);
        assert!(!alias.is_active());
        assert_eq!(owner.get_status().await.unwrap().aliases, 0);
    });
}

/// Spawns an owner with `config`.
async fn owner_with(config: ActorConfig) -> (Arc<Seen>, ActorHandle<Owner>) {
    let seen = Arc::new(Seen::default());
    let owner = Owner::new(&seen).spawn().with_config(config).await.unwrap();
    (seen, owner)
}

/// Holds the owner in a turn; the order it then handles shows the lane each
/// send used.
async fn hold(owner: &ActorHandle<Owner>) -> oneshot::Sender<()> {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    owner.notify(Msg::Block(entered, release_rx)).await.unwrap();
    entered_rx.await.unwrap();
    release
}

/// An alias's priority send rides the lane only when the alias was made with
/// the option; any other alias's priority send takes the ordinary mailbox.
#[test]
fn priority_rides_the_lane_only_for_a_priority_alias() {
    for gate in [false, true] {
        for try_form in [false, true] {
            common::run(2, async move {
                let mut config = ActorConfig::default().with_priority_lane(8);
                if gate {
                    config = config.with_priority_by_alias();
                }
                let (seen, owner) = owner_with(config).await;
                let plain = mint(&owner, AliasOptions::default()).await;
                let lane = mint(
                    &owner,
                    AliasOptions {
                        priority: true,
                        ..AliasOptions::default()
                    },
                )
                .await;
                let release = hold(&owner).await;
                owner.notify(Msg::Data(1)).await.unwrap();
                if try_form {
                    plain.try_notify_priority(Msg::Data(2)).unwrap();
                    lane.try_notify_priority(Msg::Data(3)).unwrap();
                } else {
                    plain.notify_priority(Msg::Data(2)).await.unwrap();
                    lane.notify_priority(Msg::Data(3)).await.unwrap();
                }
                assert_eq!(owner.priority_len(), 1, "one message on the lane");
                release.send(()).unwrap();
                owner.send(Msg::Sync).await.unwrap();
                assert_eq!(order(&seen), ["3", "1", "2"], "gate {gate}, try {try_form}");
            });
        }
    }
}

/// On an actor spawned without a priority lane, a priority send through an
/// alias made with the option takes the ordinary mailbox, in send order, and
/// reaches the owner.
#[test]
fn a_priority_alias_of_an_actor_with_no_lane_sends_on_the_ordinary_mailbox() {
    for try_form in [false, true] {
        common::run(2, async move {
            let (seen, owner) = owner_with(ActorConfig::default()).await;
            let lane = mint(
                &owner,
                AliasOptions {
                    priority: true,
                    ..AliasOptions::default()
                },
            )
            .await;
            let release = hold(&owner).await;
            owner.notify(Msg::Data(1)).await.unwrap();
            if try_form {
                lane.try_notify_priority(Msg::Data(2)).unwrap();
            } else {
                lane.notify_priority(Msg::Data(2)).await.unwrap();
            }
            assert_eq!(owner.priority_len(), 0, "there is no lane");
            release.send(()).unwrap();
            owner.send(Msg::Sync).await.unwrap();
            assert_eq!(order(&seen), ["1", "2"], "try {try_form}");
        });
    }
}

/// The gate is off unless asked for, so a plain handle's priority send rides
/// the lane; with it on, that send takes the ordinary mailbox, in order, and
/// the lane is the alias's.
#[test]
fn the_gate_is_off_by_default_and_degrades_plain_priority_when_on() {
    assert!(!ActorConfig::default().mailbox.priority_by_alias);
    for gate in [false, true] {
        common::run(2, async move {
            let mut config = ActorConfig::default().with_priority_lane(8);
            if gate {
                config = config.with_priority_by_alias();
            }
            let (seen, owner) = owner_with(config).await;
            let release = hold(&owner).await;
            owner.notify(Msg::Data(1)).await.unwrap();
            owner.notify_priority(Msg::Data(2)).await.unwrap();
            owner.try_notify_priority(Msg::Data(3)).unwrap();
            assert_eq!(owner.priority_len(), if gate { 0 } else { 2 });
            release.send(()).unwrap();
            owner.send(Msg::Sync).await.unwrap();
            let want = if gate {
                ["1", "2", "3"]
            } else {
                ["2", "3", "1"]
            };
            assert_eq!(order(&seen), want, "gate {gate}");
            // A request on the priority path degrades the same way.
            let config = if gate {
                ActorConfig::default()
                    .with_priority_lane(8)
                    .with_priority_by_alias()
            } else {
                ActorConfig::default().with_priority_lane(8)
            };
            let (seen, owner) = owner_with(config).await;
            let release = hold(&owner).await;
            owner.notify(Msg::Data(1)).await.unwrap();
            let asker = {
                let owner = owner.clone();
                tokio::spawn(async move { owner.send_priority(Msg::Data(2)).await })
            };
            let give_up = std::time::Instant::now() + PROMPT;
            while owner.mailbox_len() + owner.priority_len() != 2
                && std::time::Instant::now() < give_up
            {
                tokio::task::yield_now().await;
            }
            assert_eq!(owner.priority_len(), if gate { 0 } else { 1 });
            release.send(()).unwrap();
            timeout(PROMPT, asker).await.unwrap().unwrap().unwrap();
            let want = if gate { ["1", "2"] } else { ["2", "1"] };
            assert_eq!(order(&seen), want, "send_priority, gate {gate}");
        });
    }
}

/// An exit through an active priority alias, sent to a trapping owner that
/// has a lane, is handled ahead of the ordinary messages that were queued
/// before it, as the end of a `link_priority` peer is. The exit is put on the
/// lane by the owner's drain of its inbox, before the owner reads the next
/// message, so the order holds whether or not another thread runs meanwhile.
#[test]
fn an_exit_through_a_priority_alias_is_handled_ahead_of_messages_already_queued() {
    common::run(2, async {
        let seen = Arc::new(Seen::default());
        let mut owner = Owner::new(&seen);
        owner.trap = true;
        let owner = owner
            .spawn()
            .with_config(ActorConfig::default().with_priority_lane(8))
            .await
            .unwrap();
        let alias = mint(
            &owner,
            AliasOptions {
                priority: true,
                ..AliasOptions::default()
            },
        )
        .await;
        let release = hold(&owner).await;
        owner.notify(Msg::Data(1)).await.unwrap();
        owner.notify(Msg::Data(2)).await.unwrap();
        let signalled = StopReason::Failure(ActorError::user("signalled"));
        let exiter = Exiter.spawn().await.unwrap();
        exiter.send((alias, signalled)).await.unwrap();
        release.send(()).unwrap();
        owner.send(Msg::Sync).await.unwrap();
        assert_eq!(order(&seen), ["exit", "1", "2"]);
    });
}

/// How one `exit_priority` row is set up.
#[derive(Clone, Copy, Debug)]
struct ExitRow {
    priority_alias: bool,
    trapping: bool,
    lane: bool,
    ended_first: bool,
}

/// What the owner shows after the exit was sent.
#[derive(Debug)]
enum Shown {
    /// The messages it handled, in order, and how many are queued while its
    /// second turn is held: the exit is among them unless it was handled
    /// before that turn started.
    Order(&'static [&'static str], usize),
    /// The owner ends with the signalled reason.
    Ends,
}

/// Each missing piece degrades `exit_priority` to an ordinary exit: the signal
/// reaches a trapping receiver through its mailbox, after the messages queued
/// before it, and ends one that does not trap. A signal through an alias that
/// `unalias` ended has no effect, whether the receiver traps or not.
#[test]
fn exit_priority_needs_an_active_priority_alias_a_trapping_receiver_and_a_lane() {
    let full = ExitRow {
        priority_alias: true,
        trapping: true,
        lane: true,
        ended_first: false,
    };
    let rows = [
        (full, Shown::Order(&["exit", "1", "2"], 2)),
        (
            ExitRow {
                priority_alias: false,
                ..full
            },
            Shown::Order(&["1", "2", "exit"], 3),
        ),
        (
            ExitRow {
                lane: false,
                ..full
            },
            Shown::Order(&["1", "2", "exit"], 3),
        ),
        (
            ExitRow {
                ended_first: true,
                ..full
            },
            Shown::Order(&["1", "2"], 2),
        ),
        (
            ExitRow {
                ended_first: true,
                trapping: false,
                ..full
            },
            Shown::Order(&["1", "2"], 2),
        ),
        (
            ExitRow {
                trapping: false,
                ..full
            },
            Shown::Ends,
        ),
    ];
    for (row, want) in rows {
        common::run(2, async move {
            let seen = Arc::new(Seen::default());
            let mut owner = Owner::new(&seen);
            owner.trap = row.trapping;
            let mut config = ActorConfig::default();
            if row.lane {
                config = config.with_priority_lane(8);
            }
            let owner = owner.spawn().with_config(config).await.unwrap();
            let alias = mint(
                &owner,
                AliasOptions {
                    priority: row.priority_alias,
                    ..AliasOptions::default()
                },
            )
            .await;
            if row.ended_first {
                let (tx, rx) = oneshot::channel();
                owner.notify(Msg::Unalias(0, tx)).await.unwrap();
                assert!(rx.await.unwrap());
            }
            let signalled = StopReason::Failure(ActorError::user("signalled"));
            // The owner is held in a turn while the exit is sent, so its
            // inbox is read only after the release. A second held turn waits
            // behind the two messages: the owner reads its inbox before it
            // starts that turn, so the test sees the exit land in a queue
            // while the owner is held, and the order the owner handles the
            // three in shows the lane the exit used.
            let release = hold(&owner).await;
            let (entered, entered_rx) = oneshot::channel();
            let (release_second, release_second_rx) = oneshot::channel();
            owner
                .notify(Msg::Block(entered, release_second_rx))
                .await
                .unwrap();
            owner.notify(Msg::Data(1)).await.unwrap();
            owner.notify(Msg::Data(2)).await.unwrap();
            let exiter = Exiter.spawn().await.unwrap();
            exiter.send((alias, signalled.clone())).await.unwrap();
            release.send(()).unwrap();
            match want {
                Shown::Order(want, total) => {
                    timeout(PROMPT, entered_rx).await.unwrap().unwrap();
                    let give_up = std::time::Instant::now() + PROMPT;
                    while owner.mailbox_len() + owner.priority_len() != total
                        && std::time::Instant::now() < give_up
                    {
                        tokio::task::yield_now().await;
                    }
                    release_second.send(()).unwrap();
                    owner.send(Msg::Sync).await.unwrap();
                    // A slot the exit's task reserved counts as queued before
                    // the exit is in it, so the last message may land just
                    // after the owner handled the second one.
                    let give_up = std::time::Instant::now() + PROMPT;
                    while order(&seen).len() < want.len() && std::time::Instant::now() < give_up {
                        tokio::task::yield_now().await;
                    }
                    owner.send(Msg::Sync).await.unwrap();
                    assert_eq!(order(&seen), want, "{row:?}");
                }
                Shown::Ends => {
                    assert_eq!(
                        timeout(PROMPT, owner.wait_stopped()).await.unwrap(),
                        signalled,
                        "{row:?}"
                    );
                }
            }
        });
    }
}

/// The trap map of an exit that rides the priority lane runs in the owner's
/// own task, between turns: the stuck-turn watchdog sees it as a turn, as it
/// sees one that handles a message.
#[test]
fn an_inline_trap_map_is_a_turn_for_the_watchdog() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create_with(
            format!("alias-gate-{}", uuid::Uuid::new_v4()),
            SystemConfig::default().with_turn_warning_threshold(Duration::from_secs(3600)),
        )
        .unwrap();
        let seen = Arc::new(Seen::default());
        let hold = Arc::new(MapHold::default());
        let mut owner = Owner::new(&seen);
        owner.trap = true;
        owner.slow_map = Some(hold.clone());
        let owner = owner
            .spawn()
            .on_system(&sys)
            .with_config(ActorConfig::default().with_priority_lane(8))
            .await
            .unwrap();
        let alias = mint(
            &owner,
            AliasOptions {
                priority: true,
                ..AliasOptions::default()
            },
        )
        .await;
        let exiter = Exiter.spawn().on_system(&sys).await.unwrap();
        exiter
            .send((alias, StopReason::Failure(ActorError::user("signalled"))))
            .await
            .unwrap();
        let give_up = std::time::Instant::now() + PROMPT;
        while !hold.began.load(Ordering::SeqCst) && std::time::Instant::now() < give_up {
            tokio::task::yield_now().await;
        }
        assert!(hold.began.load(Ordering::SeqCst), "the trap map began");
        let in_flight = sys.turn_elapsed(owner.id());
        hold.open.store(true, Ordering::SeqCst);
        assert!(in_flight.is_some(), "the map's run is a turn in flight");
        owner.send(Msg::Sync).await.unwrap();
        assert_eq!(order(&seen), ["exit"]);
        assert_eq!(sys.turn_elapsed(owner.id()), None, "the turn ended");
    });
}

/// An alias keeps nothing alive: with every other handle dropped the actor
/// still runs until it is stopped, and every alias ends with its owner.
#[test]
fn an_alias_never_extends_the_actors_life_and_dies_with_its_owner() {
    common::run(2, async {
        let seen = Arc::new(Seen::default());
        let owner = Owner::new(&seen).spawn().await.unwrap();
        let alias = mint(&owner, AliasOptions::default()).await;
        let stopper = owner.clone();
        drop(owner);
        settle().await;
        assert_eq!(stopper.status(), tokio_actors::ActorStatus::Running);
        alias.notify(Msg::Data(1)).await.unwrap();
        assert!(alias.is_active());
        stopper.stop(StopReason::Graceful).await.unwrap();
        stopper.wait_stopped().await;
        assert!(!alias.is_active(), "the alias ended with its owner");
        let sent = alias.notify(Msg::Data(2)).await;
        assert!(
            matches!(sent, Err(AliasError::Inactive) | Err(AliasError::Closed)),
            "{sent:?}"
        );
        let tried = alias.try_notify(Msg::Data(3));
        assert!(
            matches!(tried, Err(AliasError::Inactive) | Err(AliasError::Closed)),
            "{tried:?}"
        );
    });
}

/// The status counts the aliases that still accept messages.
#[test]
fn snapshot_aliases_agrees() {
    common::run(2, async {
        let seen = Arc::new(Seen::default());
        let owner = Owner::new(&seen).spawn().await.unwrap();
        assert_eq!(owner.get_status().await.unwrap().aliases, 0);
        let a = mint(&owner, AliasOptions::default()).await;
        let _b = mint(&owner, AliasOptions::default()).await;
        let reply = mint(
            &owner,
            AliasOptions {
                reply: true,
                ..AliasOptions::default()
            },
        )
        .await;
        assert_eq!(owner.get_status().await.unwrap().aliases, 3);
        let (tx, rx) = oneshot::channel();
        owner.notify(Msg::Unalias(0, tx)).await.unwrap();
        assert!(rx.await.unwrap());
        assert_eq!(owner.get_status().await.unwrap().aliases, 2);
        reply.notify(Msg::Data(0)).await.unwrap();
        owner.send(Msg::Sync).await.unwrap();
        assert_eq!(owner.get_status().await.unwrap().aliases, 1);
        assert!(!a.is_active());
    });
}

/// A trapping owner that is linked to a peer takes an exit on the lane or in
/// queue order by how the exit was made: the end of a peer linked with
/// `link_priority` is handled ahead of the messages queued before it, and the
/// end of a plain link and an exit through a plain alias are handled in queue
/// order, the alias exit with the owner linked to a peer that stays running.
///
/// The lane counts a slot once the forwarder has reserved it, and the exit is
/// put in it a few instructions later, after the trap map ran and the link's
/// cell was taken. An owner released in that gap takes "1" from the mailbox
/// first, and the exit follows "1" or "2"; such a round of a priority row is
/// run again with new actors, and the order is asserted on the first round
/// whose exit was on the lane before the owner was released.
#[test]
fn a_trapping_linked_owner_orders_exits_by_the_lane_they_used() {
    #[derive(Clone, Copy, Debug)]
    enum How {
        PriorityLink,
        PlainLink,
        PlainAlias,
    }
    /// Rounds a priority row may take to have its exit on the lane before the
    /// owner is released.
    const ROUNDS: usize = 20;
    // The last field is whether the owner leaves its lane to priority aliases:
    // the end of a `link_priority` peer takes the lane either way.
    let rows = [
        (How::PriorityLink, false, ["exit", "1", "2"]),
        (How::PriorityLink, true, ["exit", "1", "2"]),
        (How::PlainLink, false, ["1", "2", "exit"]),
        (How::PlainAlias, false, ["1", "2", "exit"]),
    ];
    for (how, gate, want) in rows {
        common::run(2, async move {
            let priority = matches!(how, How::PriorityLink);
            for _ in 0..ROUNDS {
                let seen = Arc::new(Seen::default());
                let mut owner = Owner::new(&seen);
                owner.trap = true;
                let config = ActorConfig::default().with_priority_lane(8);
                let config = if gate {
                    config.with_priority_by_alias()
                } else {
                    config
                };
                let owner = owner.spawn().with_config(config).await.unwrap();
                // The owner is linked to the peer in every row; the alias row
                // leaves the peer running.
                let peer = Peer.spawn().await.unwrap();
                owner
                    .notify(Msg::Link(peer.clone(), priority))
                    .await
                    .unwrap();
                owner.send(Msg::Sync).await.unwrap();
                let alias = match how {
                    How::PlainAlias => Some(mint(&owner, AliasOptions::default()).await),
                    _ => None,
                };
                // The owner is held in a turn while two messages queue and the
                // exit is sent; the order it handles them in shows the lane.
                let release = hold(&owner).await;
                owner.notify(Msg::Data(1)).await.unwrap();
                owner.notify(Msg::Data(2)).await.unwrap();
                let signalled = StopReason::Failure(ActorError::user("signalled"));
                match alias {
                    Some(alias) => {
                        let exiter = Exiter.spawn().await.unwrap();
                        exiter.send((alias, signalled)).await.unwrap();
                    }
                    None => {
                        peer.stop(signalled).await.unwrap();
                        // The link's forwarder takes a slot for the exit while
                        // the owner is held; the slot is observed on the
                        // queues before the release.
                        timeout(PROMPT, async {
                            while owner.mailbox_len() + owner.priority_len() < 3 {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .expect("the exit reaches a queue");
                        if priority {
                            assert_eq!(
                                owner.priority_len(),
                                1,
                                "the exit's slot is on the lane, gate {gate}"
                            );
                        }
                    }
                }
                release.send(()).unwrap();
                timeout(PROMPT, async {
                    while order(&seen).len() < 3 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("{how:?}: got {:?}", order(&seen)));
                let got = order(&seen);
                // The exit's slot was on the lane, the exit was put in it
                // after the owner took "1", and every entry was handled once.
                let put_after_the_release = priority
                    && got.iter().filter(|entry| *entry == "exit").count() == 1
                    && got[0] != "exit"
                    && got
                        .iter()
                        .filter(|entry| *entry != "exit")
                        .eq(["1", "2"].iter());
                if !put_after_the_release {
                    assert_eq!(got, want, "{how:?} gate {gate}");
                    return;
                }
            }
            panic!("{how:?} gate {gate}: no round had the exit on the lane before the release");
        });
    }
}
