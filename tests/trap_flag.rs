#![allow(dead_code)]
//! The trap flag: its default, when a link's end reads it, what a supervisor
//! does with it, and that a worker that sets it is stopped in order by its
//! supervisor's end.
//!
//! Every test drives real actors on both runtime flavors.

use tokio::time::sleep;

use tokio_actors::{ActorError, ActorHandle, DownReason, LinkExit, RestartType, StopReason};

mod common;
#[path = "common/link_node.rs"]
mod nodes;

use nodes::*;

/// Rounds the toggle scenario may take to have the link's end read the flag
/// before the toggle.
const ROUNDS: usize = 20;

fn boom() -> StopReason {
    StopReason::Failure(ActorError::user("boom"))
}

/// What a trapping owner shows first once its peer has ended: the exit
/// reached its `handle`, or the owner ended.
enum Seen {
    Exit(LinkExit),
    Ended(StopReason),
}

/// The first exit the owner's `handle` sees, other events skipped, or the
/// owner's end, whichever comes first.
async fn exit_or_end(owner: &ActorHandle<Node>, log: &mut Log) -> Seen {
    within("an exit or the owner's end", async {
        loop {
            tokio::select! {
                event = log.recv() => match event {
                    Some(Event::Exit(exit)) => return Seen::Exit(exit),
                    Some(_) => {}
                    None => return Seen::Ended(owner.wait_stopped().await),
                },
                reason = owner.wait_stopped() => return Seen::Ended(reason),
            }
        }
    })
    .await
}

/// An owner that traps exits is inside a turn that stops trapping at its
/// gate, with its mailbox full; its peer fails, and the gate opens once the
/// peer has ended. The link's end reads the flag when it sees the peer's end
/// and, in the same poll, waits for room in the full mailbox. Nothing public
/// marks that read, so the toggle can come first: the owner then ends as one
/// that does not trap, with no exit handled, and the round is run again with
/// a new pair. Returns the owner and the exit of the first round whose exit
/// reached `handle`.
async fn toggled_while_the_exit_waits() -> (ActorHandle<Node>, LinkExit) {
    for _ in 0..ROUNDS {
        let (a, mut alog) = node(Opts {
            capacity: Some(2),
            ..Opts::default()
        })
        .await;
        let (b, _blog) = node(Opts::default()).await;
        trap(&a).await;
        link(&a, Peer::Handle(b.clone())).await.unwrap();
        let held = pause();
        a.notify(Cmd::WaitThenUntrap(held.clone())).await.unwrap();
        held.entered().await;
        a.notify(Cmd::Data(1)).await.unwrap();
        a.notify(Cmd::Data(2)).await.unwrap();
        b.notify(Cmd::Fail).await.unwrap();
        assert_eq!(within("b's end", b.wait_stopped()).await, boom());
        held.release();
        match exit_or_end(&a, &mut alog).await {
            Seen::Exit(exit) => return (a, exit),
            Seen::Ended(reason) => {
                // The toggle came before the read: the end applies, as it
                // does to an owner that stopped trapping before its peer
                // ended, and no exit reached `handle`.
                assert!(reason == StopReason::Kill || reason == boom(), "{reason:?}");
                while let Ok(event) = alog.try_recv() {
                    assert!(
                        !matches!(event, Event::Exit(_)),
                        "an owner that ended also handled the exit"
                    );
                }
            }
        }
    }
    panic!("in no round did the link's end read the flag before the toggle");
}

/// A link's end reads the flag when the other end's end is seen: an actor
/// that began trapping after it linked is told, one that stopped trapping is
/// ended, and an exit that already passed the read and waits for a slot is not
/// redirected by a toggle that follows.
#[test]
fn flag_is_read_when_the_peers_end_is_seen() {
    common::run(4, async {
        // Trapping began after the link: the exit is delivered.
        {
            let (a, mut alog) = node(Opts::default()).await;
            let (b, _blog) = node(Opts::default()).await;
            link(&a, Peer::Handle(b.clone())).await.unwrap();
            trap(&a).await;
            b.notify(Cmd::Fail).await.unwrap();
            let exit = next_exit(&mut alog).await;
            assert_eq!(exit.reason, DownReason::Stopped(boom()));
            assert!(a.is_alive());
        }
        // Trapping ended after the link: the end applies.
        {
            let (a, mut alog) = node(Opts::default()).await;
            let (b, _blog) = node(Opts::default()).await;
            trap(&a).await;
            link(&a, Peer::Handle(b.clone())).await.unwrap();
            ask(&a, Cmd::Untrap).await;
            b.notify(Cmd::Fail).await.unwrap();
            let reason = within("a's end", a.wait_stopped()).await;
            assert!(reason == StopReason::Kill || reason == boom(), "{reason:?}");
            assert!(exits_after_quiet(&mut alog).await.is_empty());
        }
        // A toggle while the exit waits for a mailbox slot does not redirect
        // it.
        {
            let (a, exit) = toggled_while_the_exit_waits().await;
            assert_eq!(exit.reason, DownReason::Stopped(boom()));
            assert!(a.is_alive(), "the exit was already on its way as a message");
            assert!(!status(&a).await.trap_exit);
        }
    });
}

/// A supervisor with no map drops the exit of an actor that is not its child:
/// it goes on, its children go on, no child event is reported, and the link is
/// not held any more.
#[test]
fn supervisor_reads_true_and_drops_an_unmapped_exit_silently() {
    common::run(4, async {
        let (s, mut slog) = node(Opts {
            supervisor: true,
            ..Opts::default()
        })
        .await;
        let child = match ask(&s, Cmd::Spawn(uname("kid"), RestartType::Permanent)).await {
            Reply::Child(Some(child)) => child,
            other => panic!("no child: {other:?}"),
        };
        let (b, _blog) = node(Opts::default()).await;
        link(&s, Peer::Handle(b.clone())).await.unwrap();
        assert_eq!(status(&s).await.links, 1);
        b.notify(Cmd::Fail).await.unwrap();
        assert_eq!(within("b's end", b.wait_stopped()).await, boom());
        eventually("the link to be dropped", async || {
            status(&s).await.links == 0
        })
        .await;
        sleep(QUIET).await;
        assert!(
            s.is_alive(),
            "an unmapped supervisor does not end on the exit"
        );
        assert!(child.is_alive(), "its children go on");
        while let Ok(event) = slog.try_recv() {
            assert!(
                !matches!(event, Event::Exit(_) | Event::Child(_) | Event::Stopped(_)),
                "unexpected {event:?}"
            );
        }
    });
}

/// `untrap_exit` clears the map of an actor, and the flag too unless it
/// supervises: an actor that stopped trapping ends with its peer, a
/// supervisor that lost its map drops the exit.
#[test]
fn untrap_clears_the_map_but_not_a_supervisors_flag() {
    common::run(4, async {
        let (a, mut alog) = node(Opts::default()).await;
        let (b, _blog) = node(Opts::default()).await;
        trap(&a).await;
        ask(&a, Cmd::Untrap).await;
        link(&a, Peer::Handle(b.clone())).await.unwrap();
        b.notify(Cmd::Fail).await.unwrap();
        let reason = within("a's end", a.wait_stopped()).await;
        assert!(reason == StopReason::Kill || reason == boom(), "{reason:?}");
        assert!(exits_after_quiet(&mut alog).await.is_empty());

        let (s, mut slog) = node(Opts {
            supervisor: true,
            ..Opts::default()
        })
        .await;
        let (c, _clog) = node(Opts::default()).await;
        trap(&s).await;
        ask(&s, Cmd::Untrap).await;
        link(&s, Peer::Handle(c.clone())).await.unwrap();
        c.notify(Cmd::Fail).await.unwrap();
        assert_eq!(within("c's end", c.wait_stopped()).await, boom());
        assert!(exits_after_quiet(&mut slog).await.is_empty());
        assert!(s.is_alive());
    });
}

/// A worker that traps exits after it was spawned is stopped in order when
/// its supervisor is killed: the link guard reads the flag when it fires, so
/// the worker ends with `ParentKilled` and not with `Kill`.
#[test]
fn a_worker_that_traps_after_spawn_gets_parent_killed_when_the_guard_fires() {
    common::run(4, async {
        let (s, _slog) = node(Opts {
            supervisor: true,
            ..Opts::default()
        })
        .await;
        let worker = match ask(&s, Cmd::Spawn(uname("worker"), RestartType::Permanent)).await {
            Reply::Child(Some(child)) => child,
            other => panic!("no child: {other:?}"),
        };
        trap(&worker).await;
        s.stop(StopReason::Kill).await.unwrap();
        let reason = within("the worker's end", worker.wait_stopped()).await;
        assert_eq!(reason, StopReason::ParentKilled);
    });
}
