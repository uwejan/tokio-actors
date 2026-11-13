#![allow(dead_code)]
//! What a link delivers when an end ends: to an actor that traps exits, the
//! exit with the other end's reason, in order and once; to one that does not,
//! its own end.
//!
//! Every test drives real actors on both runtime flavors. The cause an
//! ending partner carries is the lane's concern and is pinned where the lane
//! rule is; here a partner that does not trap is only required to end, with
//! `Kill` or with the other end's reason, and to run no hook.

use std::time::Duration;

use tokio::time::sleep;

use tokio_actors::{
    ActorError, ActorId, ActorStatus, ActorSystem, DownReason, RestartType, StopReason,
};

mod common;
#[path = "common/link_node.rs"]
mod nodes;

use nodes::*;

fn boom() -> StopReason {
    StopReason::Failure(ActorError::user("boom"))
}

/// Whether `reason` is what a partner that does not trap exits ends with:
/// the plain `Kill` of the lane, or the other end's reason as the cause.
fn ended_by(reason: &StopReason, cause: &StopReason) -> bool {
    *reason == StopReason::Kill || reason == cause
}

#[derive(Clone, Copy, Debug)]
enum End {
    Graceful,
    ParentRequest,
    Shutdown,
    ErrFailure,
    PanicFailure,
    Kill,
    Aborted,
}

fn expected(end: End) -> StopReason {
    match end {
        End::Graceful => StopReason::Graceful,
        End::ParentRequest => StopReason::ParentRequest,
        End::Shutdown => StopReason::Shutdown("x".into()),
        End::ErrFailure => boom(),
        End::PanicFailure => StopReason::Failure(ActorError::Panic("boom".into())),
        End::Kill | End::Aborted => StopReason::Kill,
    }
}

async fn end_it(h: &tokio_actors::ActorHandle<Node>, end: End) {
    match end {
        End::Graceful => h.stop(StopReason::Graceful).await.unwrap(),
        End::ParentRequest => h.stop(StopReason::ParentRequest).await.unwrap(),
        End::Shutdown => h.stop(StopReason::Shutdown("x".into())).await.unwrap(),
        End::ErrFailure => h.notify(Cmd::Fail).await.unwrap(),
        End::PanicFailure => h.notify(Cmd::Panic).await.unwrap(),
        End::Kill => h.stop(StopReason::Kill).await.unwrap(),
        End::Aborted => {
            h.notify(Cmd::Hang).await.unwrap();
            // A turn parked on a future that never completes: only the abort
            // after the kill grace ends it.
            sleep(Duration::from_millis(30)).await;
            ActorSystem::default()
                .kill_by_id(h.id())
                .await
                .expect("the node is registered");
        }
    }
}

/// The exit enters through an ordinary slot, after every message the ended
/// actor sent before it ended, also on an actor that has a priority lane.
#[test]
fn trapped_exit_follows_the_peers_messages() {
    common::run(4, async {
        let (a, mut alog) = node(Opts {
            priority_lane: true,
            ..Opts::default()
        })
        .await;
        let (b, _blog) = node(Opts::default()).await;
        trap(&a).await;
        link(&a, Peer::Handle(b.clone())).await.unwrap();
        let held = pause();
        a.notify(Cmd::Wait(held.clone())).await.unwrap();
        held.entered().await;
        b.send(Cmd::Tell(a.clone(), vec![1, 2, 3])).await.unwrap();
        b.notify(Cmd::Fail).await.unwrap();
        assert_eq!(within("b's end", b.wait_stopped()).await, boom());
        sleep(QUIET).await;
        held.release();
        let mut order = Vec::new();
        for _ in 0..4 {
            order.push(next_marker(&mut alog).await);
        }
        assert_eq!(order, [1, 2, 3, 0]);
    });
}

#[derive(Clone, Copy, Debug)]
enum Stop {
    Graceful,
    Kill,
}

/// An exit still owed when the actor stops is delivered once before
/// `on_stopped`, with the status reading `Stopping`, whether it is already
/// queued in the mailbox or the actor's full mailbox still holds its task
/// back; under `Kill` none is delivered. A supervisor with a map does the
/// same.
#[test]
fn pending_exit_is_delivered_once_at_the_stop() {
    common::run(4, async {
        for supervisor in [false, true] {
            for queued in [true, false] {
                for stop in [Stop::Graceful, Stop::Kill] {
                    let what = format!("supervisor {supervisor}, queued {queued}, {stop:?}");
                    let (a, mut alog) = node(Opts {
                        supervisor,
                        capacity: Some(2),
                        ..Opts::default()
                    })
                    .await;
                    let (b, _blog) = node(Opts::default()).await;
                    trap(&a).await;
                    link(&a, Peer::Handle(b.clone())).await.unwrap();
                    let held = pause();
                    a.notify(Cmd::Wait(held.clone())).await.unwrap();
                    held.entered().await;
                    if !queued {
                        // Two messages fill the mailbox, so the exit's task
                        // waits for room.
                        a.notify(Cmd::Data(1)).await.unwrap();
                        a.notify(Cmd::Data(2)).await.unwrap();
                    }
                    b.notify(Cmd::Fail).await.unwrap();
                    assert_eq!(within("b's end", b.wait_stopped()).await, boom());
                    sleep(QUIET).await;
                    match stop {
                        Stop::Graceful => a.stop(StopReason::Graceful).await.unwrap(),
                        Stop::Kill => a.stop(StopReason::Kill).await.unwrap(),
                    }
                    held.release();
                    let reason = within("a's end", a.wait_stopped()).await;
                    let mut exits = 0;
                    let mut saw_stopped = false;
                    let mut status_at_exit = None;
                    while let Ok(event) = alog.try_recv() {
                        match event {
                            Event::Status(status) => status_at_exit = Some(status),
                            Event::Exit(exit) => {
                                assert_eq!(exit.actor_id, *b.id(), "{what}");
                                assert!(!saw_stopped, "{what}: the exit came after on_stopped");
                                exits += 1;
                            }
                            Event::Stopped(_) => saw_stopped = true,
                            Event::Data(_) | Event::Child(_) | Event::LinkInStop(_) => {}
                        }
                    }
                    match stop {
                        Stop::Graceful => {
                            assert_eq!(reason, StopReason::Graceful, "{what}");
                            assert_eq!(exits, 1, "{what}: delivered once");
                            assert_eq!(status_at_exit, Some(ActorStatus::Stopping), "{what}");
                            assert!(saw_stopped, "{what}");
                        }
                        Stop::Kill => {
                            assert_eq!(reason, StopReason::Kill, "{what}");
                            assert_eq!(exits, 0, "{what}: none under Kill");
                        }
                    }
                }
            }
        }
    });
}

/// A link to an actor that is not running delivers `NoProc`: by handle to an
/// ended actor, by id of an ended actor, and by a name nothing holds. An
/// ended run keeps its incarnation in the exit, a key that matched nothing
/// has none.
#[test]
fn link_to_an_ended_target_delivers_noproc() {
    common::run(4, async {
        let (gone, _glog) = node(Opts::default()).await;
        gone.stop(StopReason::Graceful).await.unwrap();
        within("the end", gone.wait_stopped()).await;
        eventually("the name to be free", async || {
            ActorSystem::default()
                .get_by_id::<Node>(gone.id())
                .is_none()
        })
        .await;
        let missing = uname("missing");
        let rows: Vec<(&str, Peer, ActorId, bool)> = vec![
            (
                "ended handle",
                Peer::Handle(gone.clone()),
                gone.id().clone(),
                true,
            ),
            (
                "ended id",
                Peer::Id(gone.id().clone()),
                gone.id().clone(),
                false,
            ),
            (
                "missing name",
                Peer::Name(missing.clone()),
                ActorId::from(missing.as_str()),
                false,
            ),
        ];
        for (what, peer, id, has_run) in rows {
            let (a, mut alog) = node(Opts::default()).await;
            trap(&a).await;
            assert_eq!(link(&a, peer).await, Ok(()), "{what}");
            let exit = next_exit(&mut alog).await;
            assert_eq!(exit.reason, DownReason::NoProc, "{what}");
            assert_eq!(exit.actor_id, id, "{what}");
            assert_eq!(exit.incarnation.is_some(), has_run, "{what}");
            assert_eq!(status(&a).await.links, 0, "{what}");
            assert!(
                exits_after_quiet(&mut alog).await.is_empty(),
                "{what}: once"
            );
        }
    });
}

/// A link is bound to one run. A supervised child that fails and restarts
/// under the same id and name fires the old link once; the new run is not
/// linked until a new call, which binds to the new run even while the old
/// link's exit still waits in the mailbox.
#[test]
fn a_link_binds_one_incarnation() {
    common::run(4, async {
        let (s, _slog) = node(Opts {
            supervisor: true,
            ..Opts::default()
        })
        .await;
        let name = uname("child");
        let first = match ask(&s, Cmd::Spawn(name.clone(), RestartType::Permanent)).await {
            Reply::Child(Some(child)) => child,
            other => panic!("no child: {other:?}"),
        };
        let (a, mut alog) = node(Opts::default()).await;
        trap(&a).await;
        link(&a, Peer::Name(name.clone())).await.unwrap();
        let held = pause();
        a.notify(Cmd::WaitThenLink(held.clone(), Peer::Name(name.clone())))
            .await
            .unwrap();
        held.entered().await;
        first.notify(Cmd::Fail).await.unwrap();
        let system = ActorSystem::default();
        let mut second = None;
        eventually("the restart", async || {
            second = system
                .get::<Node>(&name)
                .filter(|h| h.incarnation() != first.incarnation());
            second.is_some()
        })
        .await;
        let second = second.unwrap();
        // The exit of the first run is queued while the turn is held.
        sleep(QUIET).await;
        held.release();
        let exit = next_exit(&mut alog).await;
        assert_eq!(exit.incarnation, Some(first.incarnation()));
        assert_eq!(exit.reason, DownReason::Stopped(boom()));
        // The call made in the held turn linked the second run.
        eventually("the second link", async || status(&a).await.links == 1).await;
        second.notify(Cmd::Fail).await.unwrap();
        let exit = next_exit(&mut alog).await;
        assert_eq!(exit.incarnation, Some(second.incarnation()));
        assert_ne!(exit.incarnation, Some(first.incarnation()));
        assert!(exits_after_quiet(&mut alog).await.is_empty());
    });
}
