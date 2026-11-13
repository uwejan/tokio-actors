#![allow(dead_code)]
//! Making and cancelling links: who installs the other end, what a cancel
//! guarantees against an end racing it, and what the counts read.
//!
//! Every test drives real actors on both runtime flavors. A paused node holds
//! its run loop between turns, so what other actors push at it waits in its
//! inbox in the order it was pushed.

use std::sync::Arc;

use tokio::sync::Barrier;

use tokio_actors::{DownReason, StopReason};

mod common;
#[path = "common/link_node.rs"]
mod nodes;

use nodes::*;

/// Rounds a scenario may take to have its exit taken before the cancel.
const ROUNDS: usize = 20;

fn failed() -> StopReason {
    StopReason::Failure(tokio_actors::ActorError::user("boom"))
}

fn linker_failed() -> StopReason {
    StopReason::Failure(tokio_actors::ActorError::user("linker fails"))
}

/// A links B and ends in the same turn while B's run loop is held: when B
/// goes on, it installs the link from the item A pushed, whose plane reads
/// ended, and a trapping B is told. The install belongs to B's own loop, not
/// to a task A owns and its end can abort.
#[test]
fn caller_ends_before_the_partner_drains() {
    common::run(4, async {
        let (a, _alog) = node(Opts::default()).await;
        let (b, mut blog) = node(Opts::default()).await;
        trap(&b).await;
        let held = pause();
        b.notify(Cmd::Wait(held.clone())).await.unwrap();
        held.entered().await;
        a.notify(Cmd::LinkThenFail(Peer::Handle(b.clone())))
            .await
            .unwrap();
        assert_eq!(within("a's end", a.wait_stopped()).await, linker_failed());
        held.release();
        let exit = next_exit(&mut blog).await;
        assert_eq!(exit.actor_id, *a.id());
        assert_eq!(exit.incarnation, Some(a.incarnation()));
        assert_eq!(exit.reason, DownReason::Stopped(linker_failed()));
    });
}

/// A cancel that returned true is final: B gets no exit when A ends after it,
/// whether B had installed its end before the cancel or the link and the
/// cancel both wait in its inbox, and whether or not its system channel is
/// full of requests.
#[test]
fn unlink_then_peer_dies_is_silent() {
    common::run(4, async {
        for (installed_first, flooded) in [(true, false), (false, false), (false, true)] {
            let (a, _alog) = node(Opts::default()).await;
            let (b, mut blog) = node(Opts::default()).await;
            trap(&b).await;
            if installed_first {
                link(&a, Peer::Handle(b.clone())).await.unwrap();
                eventually("b to install its end", async || status(&b).await.links == 1).await;
            }
            let held = pause();
            b.notify(Cmd::Wait(held.clone())).await.unwrap();
            held.entered().await;
            let mut askers = Vec::new();
            if flooded {
                for _ in 0..70 {
                    let b = b.clone();
                    askers.push(tokio::spawn(async move {
                        let _ = b.get_status().await;
                    }));
                }
            }
            if !installed_first {
                link(&a, Peer::Handle(b.clone())).await.unwrap();
            }
            assert!(unlink(&a, Peer::Handle(b.clone())).await);
            a.notify(Cmd::Fail).await.unwrap();
            assert_eq!(within("a's end", a.wait_stopped()).await, failed());
            held.release();
            assert!(
                exits_after_quiet(&mut blog).await.is_empty(),
                "installed_first {installed_first}, flooded {flooded}: an exit arrived after the cancel"
            );
            for asker in askers {
                let _ = asker.await;
            }
            eventually("b to drop the link", async || status(&b).await.links == 0).await;
        }
    });
}

/// A cancel made after the exit was acted on answers false, even while the
/// exit still waits in the mailbox, and that exit is delivered.
///
/// A is held inside the turn that cancels, B ends once that turn has begun,
/// and the turn goes on once the exit has its slot in A's mailbox. The slot is
/// taken before the exit is, so the cancel can still land in time; it then
/// answers true, and the round is run again with a new pair.
#[test]
fn unlink_after_the_exit_was_queued_is_false() {
    common::run(4, async {
        for _ in 0..ROUNDS {
            let (a, mut alog) = node(Opts::default()).await;
            let (b, _blog) = node(Opts::default()).await;
            trap(&a).await;
            link(&a, Peer::Handle(b.clone())).await.unwrap();
            let held = pause();
            let asker = {
                let a = a.clone();
                let peer = Peer::Handle(b.clone());
                let held = held.clone();
                tokio::spawn(async move { a.send(Cmd::WaitThenUnlink(held, peer)).await })
            };
            held.entered().await;
            b.notify(Cmd::Fail).await.unwrap();
            assert_eq!(within("b's end", b.wait_stopped()).await, failed());
            // The held turn is not counted, so the one slot in use is the
            // exit's.
            eventually("the exit to have its slot", async || a.mailbox_len() == 1).await;
            held.release();
            match asker.await.unwrap().unwrap() {
                // The cancel took the link before the exit did: none is owed.
                Reply::Bool(true) => continue,
                Reply::Bool(false) => {}
                other => panic!("not an unlink answer: {other:?}"),
            }
            let exit = next_exit(&mut alog).await;
            assert_eq!(exit.reason, DownReason::Stopped(failed()));
            assert_eq!(status(&a).await.links, 0);
            return;
        }
        panic!("no round acted on the exit before the cancel");
    });
}

/// A cancel that waits in B's inbox when B links A again belongs to the
/// earlier link: B's new link stays, and A's end reaches B.
#[test]
fn a_late_unlink_never_removes_a_newer_link() {
    common::run(4, async {
        let (a, _alog) = node(Opts::default()).await;
        let (b, mut blog) = node(Opts::default()).await;
        trap(&b).await;
        let held = pause();
        let relinker = {
            let b = b.clone();
            let peer = Peer::Handle(a.clone());
            let held = held.clone();
            tokio::spawn(async move { b.send(Cmd::WaitThenLink(held, peer)).await })
        };
        held.entered().await;
        link(&a, Peer::Handle(b.clone())).await.unwrap();
        assert!(unlink(&a, Peer::Handle(b.clone())).await);
        held.release();
        assert!(matches!(
            relinker.await.unwrap().unwrap(),
            Reply::Link(Ok(()))
        ));
        eventually("a to hold the new link", async || {
            status(&a).await.links == 1
        })
        .await;
        a.notify(Cmd::Fail).await.unwrap();
        let exit = next_exit(&mut blog).await;
        assert_eq!(exit.actor_id, *a.id());
        assert_eq!(exit.reason, DownReason::Stopped(failed()));
    });
}

/// A and B call `link` on each other at the same instant. Both ends share one
/// state: each side holds one link, and one `unlink` cancels both, so a
/// trapping B hears nothing when A ends.
#[test]
fn crossing_links_share_one_state() {
    common::run(4, async {
        for _ in 0..12 {
            let (a, _alog) = node(Opts::default()).await;
            let (b, mut blog) = node(Opts::default()).await;
            trap(&b).await;
            let barrier = Arc::new(Barrier::new(2));
            let (from_a, from_b) = tokio::join!(
                a.send(Cmd::LinkAt(Peer::Handle(b.clone()), barrier.clone())),
                b.send(Cmd::LinkAt(Peer::Handle(a.clone()), barrier.clone())),
            );
            assert!(matches!(from_a.unwrap(), Reply::Link(Ok(()))));
            assert!(matches!(from_b.unwrap(), Reply::Link(Ok(()))));
            eventually("one link on each side", async || {
                status(&a).await.links == 1 && status(&b).await.links == 1
            })
            .await;
            assert!(unlink(&a, Peer::Handle(b.clone())).await);
            a.notify(Cmd::Fail).await.unwrap();
            assert_eq!(within("a's end", a.wait_stopped()).await, failed());
            assert!(
                exits_after_quiet(&mut blog).await.is_empty(),
                "the cancel did not reach B's end of the link"
            );
            eventually("b to drop its end", async || status(&b).await.links == 0).await;
        }
    });
}

fn ended(runtime: tokio::runtime::Runtime) {
    std::thread::spawn(move || drop(runtime)).join().unwrap();
}
