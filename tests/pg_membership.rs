#![allow(dead_code)]
//! Who is a member of a process group, and when: after a join, a leave, a
//! member's end of every kind, a restart, a join that races the end, and a
//! handle of another system.
//!
//! Every test drives real actors in a system of its own. The orders inside an
//! actor's end (the member leaves before its reason is written, a join that
//! comes between the end's mark and its sweep) are pinned at their exact
//! instants by the tests in `src/actor/pg_tests.rs`; the rows here pin what a
//! caller can observe.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorSystem, GroupChange, PeerRef, PgError, PgScope,
    RestartType, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(250);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("pgm-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

fn system(base: &str) -> Arc<ActorSystem> {
    ActorSystem::create(uname(base)).expect("a fresh system name")
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

enum Cmd {
    Fail,
    Hang,
    Ping,
}

/// A member of the first type; joins `join` as it starts when it has one.
#[derive(Default, Clone)]
struct Alpha {
    join: Option<(PgScope, &'static str)>,
}

impl Actor for Alpha {
    type Message = Cmd;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some((scope, group)) = &self.join {
            scope
                .join(*group, &ctx.self_handle())
                .expect("the actor is running");
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Cmd, _: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Cmd::Fail => Err(ActorError::user("boom")),
            Cmd::Hang => std::future::pending().await,
            Cmd::Ping => Ok(()),
        }
    }
}

/// A member of the second type.
struct Beta;

impl Actor for Beta {
    type Message = Cmd;
    type Response = ();

    async fn handle(&mut self, _: Cmd, _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

enum PCmd {
    Spawn(&'static str, Option<(PgScope, &'static str)>),
}

/// A supervisor of `Alpha` children.
struct Parent;

impl Actor for Parent {
    type Message = PCmd;
    type Response = Option<ActorHandle<Alpha>>;

    async fn handle(
        &mut self,
        msg: PCmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            PCmd::Spawn(name, join) => {
                let child = ctx
                    .spawn_child(move || Alpha { join: join.clone() })
                    .named(uname(name))
                    .restart_type(RestartType::Permanent)
                    .await?;
                Ok(Some(child))
            }
        }
    }
}

/// Logs the changes of the group monitors it makes.
struct Collector {
    log: mpsc::UnboundedSender<GroupChange>,
}

enum CCmd {
    Group(PgScope, &'static str),
    Change(GroupChange),
}

impl Actor for Collector {
    type Message = CCmd;
    type Response = ();

    async fn handle(&mut self, msg: CCmd, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            CCmd::Group(scope, group) => {
                ctx.monitor_group(&scope, group, CCmd::Change)
                    .expect("running");
            }
            CCmd::Change(change) => {
                let _ = self.log.send(change);
            }
        }
        Ok(())
    }
}

async fn alpha(system: &Arc<ActorSystem>) -> ActorHandle<Alpha> {
    Alpha::default()
        .spawn()
        .on_system(system)
        .await
        .expect("spawns")
}

async fn beta(system: &Arc<ActorSystem>) -> ActorHandle<Beta> {
    Beta.spawn().on_system(system).await.expect("spawns")
}

fn peer<A: Actor>(handle: &ActorHandle<A>) -> PeerRef {
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

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// Two actor types in one group: `members::<A>` lists only the actors of type
/// `A`, `members_any` lists both, and a group with no member is not listed.
#[test]
fn join_leave_members_by_type() {
    common::run(4, async {
        let system = system("types");
        let scope = system.pg();
        let (a, b) = (alpha(&system).await, beta(&system).await);
        assert!(scope.groups().is_empty());
        scope.join("g", &a).unwrap();
        scope.join("g", &b).unwrap();
        assert_eq!(scope.members::<Alpha>("g"), vec![a.clone()]);
        assert_eq!(scope.members::<Beta>("g"), vec![b.clone()]);
        assert_eq!(
            sorted(scope.members_any("g")),
            sorted(vec![peer(&a), peer(&b)])
        );
        assert_eq!(scope.groups(), vec![Arc::<str>::from("g")]);
        assert!(scope.members_any("other").is_empty());

        scope.leave("g", &a).unwrap();
        assert!(scope.members::<Alpha>("g").is_empty());
        assert_eq!(scope.members_any("g"), vec![peer(&b)]);
        scope.leave("g", &b).unwrap();
        assert!(scope.groups().is_empty());
        assert!(scope.members_any("g").is_empty());
    });
}

#[derive(Clone, Copy, Debug)]
enum End {
    Graceful,
    Failure,
    Kill,
    Abort,
}

/// A member of every kind of end is out of every group by the time
/// `wait_stopped` returns, and so is a member that joined several groups, one
/// of them twice.
#[test]
fn dead_member_is_never_listed_after_wait_stopped() {
    common::run(4, async {
        for end in [End::Graceful, End::Failure, End::Kill, End::Abort] {
            let system = system("dead");
            let scope = system.pg();
            let a = alpha(&system).await;
            let bystander = alpha(&system).await;
            scope.join("g", &a).unwrap();
            scope.join("g", &a).unwrap();
            scope.join("h", &a).unwrap();
            scope.join("g", &bystander).unwrap();
            match end {
                End::Graceful => a.stop(StopReason::Graceful).await.unwrap(),
                End::Failure => a.notify(Cmd::Fail).await.unwrap(),
                End::Kill => a.stop(StopReason::Kill).await.unwrap(),
                End::Abort => {
                    a.notify(Cmd::Hang).await.unwrap();
                    sleep(Duration::from_millis(30)).await;
                    system.kill_by_id(a.id()).await.expect("registered");
                }
            }
            within("the member's end", a.wait_stopped()).await;
            assert_eq!(
                scope.members_any("g"),
                vec![peer(&bystander)],
                "{end:?}: the ended member is listed, or the bystander is not"
            );
            assert!(scope.members_any("h").is_empty(), "{end:?}");
            assert_eq!(scope.groups(), vec![Arc::<str>::from("g")], "{end:?}");
            assert_eq!(
                scope.leave("g", &a),
                Err(PgError::NotJoined),
                "{end:?}: an ended member is in no group"
            );
        }
    });
}

/// A handle of another system is refused by both calls and by neither is it
/// listed.
#[test]
fn foreign_handle_is_refused() {
    common::run(4, async {
        let (mine, theirs) = (system("mine"), system("theirs"));
        let scope = mine.pg();
        let outsider = alpha(&theirs).await;
        assert_eq!(scope.join("g", &outsider), Err(PgError::ForeignSystem));
        assert_eq!(scope.leave("g", &outsider), Err(PgError::ForeignSystem));
        assert!(scope.members_any("g").is_empty());
        assert!(scope.groups().is_empty());
        // The scope of the actor's own system takes it.
        theirs.pg().join("g", &outsider).unwrap();
        assert_eq!(theirs.pg().members_any("g"), vec![peer(&outsider)]);
        assert!(scope.members_any("g").is_empty());
    });
}

/// The same group name in two systems, and the same scope name in one system,
/// are different groups.
#[test]
fn scope_is_per_system() {
    common::run(4, async {
        let (one, two) = (system("one"), system("two"));
        let (a, b) = (alpha(&one).await, alpha(&two).await);
        one.pg().join("g", &a).unwrap();
        two.pg().join("g", &b).unwrap();
        assert_eq!(one.pg().members_any("g"), vec![peer(&a)]);
        assert_eq!(two.pg().members_any("g"), vec![peer(&b)]);
        // Named scopes of one system are independent of the default and of each other.
        let (left, right) = (one.pg_scope("left"), one.pg_scope("right"));
        assert_eq!(left.name(), "left");
        assert_eq!(one.pg().name(), "pg");
        left.join("g", &a).unwrap();
        assert_eq!(left.members_any("g"), vec![peer(&a)]);
        assert!(right.members_any("g").is_empty());
        // The same name gives the same scope.
        assert_eq!(one.pg_scope("left").members_any("g"), vec![peer(&a)]);
    });
}

/// A system that was shut down and reactivated still has its scope: a handle to
/// the scope made before the shutdown works after it, and the system's
/// default scope is that scope.
#[test]
fn scope_survives_reactivate() {
    common::run(4, async {
        let system = system("reactivate");
        let scope = system.pg();
        let first = alpha(&system).await;
        scope.join("g", &first).unwrap();
        within("the shutdown", system.shutdown()).await;
        assert!(
            scope.members_any("g").is_empty() && scope.groups().is_empty(),
            "the shutdown ended every member"
        );
        assert!(system.reactivate(), "a shut down system reactivates");
        let second = alpha(&system).await;
        scope.join("g", &second).unwrap();
        assert_eq!(system.pg().members_any("g"), vec![peer(&second)]);
    });
}

/// An actor joined twice is listed twice, leaves once per count, and when it
/// ends a group monitor is told of each join it held: two `Leave` changes for
/// a member joined twice.
#[test]
fn joined_twice_leaves_once_per_count_and_dies_with_two_leaves() {
    common::run(4, async {
        let system = system("twice");
        let scope = system.pg();
        let (log, mut changes) = mpsc::unbounded_channel();
        let watcher = Collector { log }.spawn().on_system(&system).await.unwrap();
        // `send` answers after the handler ran, so the monitor exists before
        // the first join; a cast returns once the message is queued, and a
        // join that came first would not be a change the monitor sees.
        watcher.send(CCmd::Group(scope.clone(), "g")).await.unwrap();
        let (a, b) = (alpha(&system).await, alpha(&system).await);
        scope.join("g", &a).unwrap();
        scope.join("g", &a).unwrap();
        assert_eq!(scope.members_any("g"), vec![peer(&a), peer(&a)]);
        assert_eq!(scope.members::<Alpha>("g").len(), 2);
        scope.leave("g", &a).unwrap();
        assert_eq!(scope.members_any("g"), vec![peer(&a)]);
        scope.leave("g", &a).unwrap();
        assert!(scope.members_any("g").is_empty());
        assert_eq!(scope.leave("g", &a), Err(PgError::NotJoined));

        // The same member joined twice, ending: two leaves.
        scope.join("g", &b).unwrap();
        scope.join("g", &b).unwrap();
        b.stop(StopReason::Graceful).await.unwrap();
        within("the end", b.wait_stopped()).await;
        assert!(scope.members_any("g").is_empty());

        let (mut joins, mut leaves) = (Vec::new(), Vec::new());
        while joins.len() + leaves.len() < 8 {
            match within("a change", changes.recv()).await.expect("logged") {
                GroupChange::Join { members, .. } => joins.push(members),
                GroupChange::Leave { members, .. } => leaves.push(members),
                GroupChange::Resync { .. } => panic!("no overflow here"),
            }
        }
        assert_eq!(joins.len(), 4, "a joined twice, b joined twice");
        assert_eq!(
            leaves,
            vec![
                vec![peer(&a)],
                vec![peer(&a)],
                vec![peer(&b)],
                vec![peer(&b)]
            ],
            "one Leave for each count, each naming the member once"
        );
        sleep(QUIET).await;
        assert!(changes.try_recv().is_err());
    });
}

async fn system_other() -> ActorHandle<Alpha> {
    let other = system("snapshot-other");
    alpha(&other).await
}
