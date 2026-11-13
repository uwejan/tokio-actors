//! Process-group tables, queues, counts and the orders an actor's end keeps.
//!
//! The tables are driven bare; the orders run real actors with a hook
//! attached at the point of the end under test, so each order is pinned at
//! its exact instant and not by luck.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Wake, Waker};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Semaphore};
use tokio::time::timeout;

use super::{MemberKey, PgScope, PgScopeInner};
use crate::actor::context::ActorContext;
use crate::actor::pg_monitor::SubKind;
use crate::actor::runtime::end_hooks;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorResult, PgError};
use crate::types::{ActorId, GroupChange, Incarnation, MonitorId, StopReason};
use crate::{ActorHandle, ActorSystem};

fn key(n: usize) -> MemberKey {
    (ActorId::from(format!("member-{n}")), Incarnation::mint())
}

fn handle_box() -> Arc<dyn std::any::Any + Send + Sync> {
    Arc::new(())
}

fn scope() -> PgScopeInner {
    PgScopeInner::new("t".into(), "sys".into(), std::sync::Weak::new())
}

// ---------------------------------------------------------------------------
// The tables, bare
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Real actors
// ---------------------------------------------------------------------------

static UNIQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn fresh_system(base: &str) -> Arc<ActorSystem> {
    ActorSystem::create(format!(
        "pg-tests-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ))
    .expect("a fresh system name")
}

enum Cmd {
    /// A handler that never returns.
    Hang,
    /// Holds the turn until the semaphore has a permit.
    Hold(Arc<Semaphore>),
    /// Monitors a group of the scope; answers the monitor's id.
    Watch(PgScope, &'static str),
    /// A group change reaching the watcher.
    Change(GroupChange),
    /// Revokes a monitor.
    Demonitor(MonitorId),
    /// How many forwarder tasks the actor holds.
    Tasks,
}

#[derive(Debug)]
enum Reply {
    Unit,
    Id(MonitorId),
    Count(usize),
}

struct Subject {
    changes: Option<mpsc::UnboundedSender<GroupChange>>,
}

impl Subject {
    fn plain() -> Self {
        Self { changes: None }
    }

    fn logging() -> (Self, mpsc::UnboundedReceiver<GroupChange>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { changes: Some(tx) }, rx)
    }
}

impl Actor for Subject {
    type Message = Cmd;
    type Response = Reply;

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        match msg {
            Cmd::Hang => std::future::pending::<()>().await,
            Cmd::Hold(gate) => {
                let _ = gate.acquire().await;
            }
            Cmd::Watch(scope, group) => {
                let (id, _) = ctx
                    .monitor_group(&scope, group, Cmd::Change)
                    .expect("running");
                return Ok(Reply::Id(id));
            }
            Cmd::Change(change) => {
                if let Some(log) = &self.changes {
                    let _ = log.send(change);
                }
            }
            Cmd::Demonitor(id) => {
                ctx.demonitor(id);
            }
            Cmd::Tasks => return Ok(Reply::Count(ctx.forwarder_tasks())),
        }
        Ok(Reply::Unit)
    }
}

async fn spawn_in(system: &Arc<ActorSystem>, subject: Subject) -> ActorHandle<Subject> {
    subject
        .spawn()
        .on_system(system)
        .await
        .expect("the subject spawns")
}

async fn wait_ended(handle: &ActorHandle<Subject>) -> StopReason {
    timeout(Duration::from_secs(10), handle.wait_stopped())
        .await
        .expect("the actor ends")
}

/// A join racing the end has one outcome, whichever side comes second: made
/// after the end marked the run, it is `NoProc`; made before, the member is
/// listed and the end removes it. No ghost either way.
#[tokio::test]
async fn join_losing_the_race_with_the_end_is_noproc() {
    for (point, expect_refused) in [(end_hooks::PG_MARKED, true), (end_hooks::NAME_FREED, false)] {
        let system = fresh_system("race");
        let scope = system.pg();
        let subject = spawn_in(&system, Subject::plain()).await;
        // The second row has a membership for the end to sweep; the first has
        // none and must still refuse.
        if !expect_refused {
            scope.join("first", &subject).unwrap();
        }
        let outcome: Arc<Mutex<Option<Result<(), PgError>>>> = Arc::default();
        let (probe, again, out) = (scope.clone(), subject.clone(), outcome.clone());
        end_hooks::attach(point, subject.id().clone(), move || {
            *out.lock().unwrap() = Some(probe.join("g", &again));
        });
        subject.stop(StopReason::Graceful).await.unwrap();
        wait_ended(&subject).await;
        let got = outcome.lock().unwrap().take().expect("the hook ran");
        if expect_refused {
            assert_eq!(
                got,
                Err(PgError::NoProc),
                "joined after the end marked the run"
            );
        } else {
            assert_eq!(got, Ok(()), "joined before the end marked the run");
        }
        assert!(
            scope.members_any("g").is_empty() && scope.members_any("first").is_empty(),
            "a member listed after the end"
        );
        assert!(scope.groups().is_empty());
    }
}

/// A waker that panics when woken, standing for the waker of a task parked on
/// a subscription's wake.
struct Loud(AtomicBool);

impl Wake for Loud {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
        panic!("the parked task's waker panicked");
    }
}

/// The wake that tells an entry task about a change runs inside a catch: a
/// parked waiter whose waker panics does not unwind into the join or the end
/// that made the change, the change is still recorded, and the next change
/// still wakes.
#[tokio::test]
async fn change_wake_is_contained() {
    let system = fresh_system("wake");
    let scope = system.pg();
    let subject = spawn_in(&system, Subject::plain()).await;
    let id = MonitorId::next();
    let (wake, _) = scope.inner().subscribe(id, SubKind::Group("g".into()), 8);
    let loud = Arc::new(Loud(AtomicBool::new(false)));
    let waker = Waker::from(loud.clone());
    let mut cx = Context::from_waker(&waker);
    let mut parked = Box::pin(wake.notified());
    assert!(parked.as_mut().poll(&mut cx).is_pending());

    let joined =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scope.join("g", &subject)));
    assert!(joined.is_ok(), "the join unwound with the waker's panic");
    assert_eq!(joined.unwrap(), Ok(()));
    assert!(loud.0.load(Ordering::SeqCst), "the parked waker was woken");
    assert_eq!(
        scope.inner().lock().subs.state_for_test(id),
        Some((1, false)),
        "the change was recorded"
    );
    assert_eq!(scope.members_any("g").len(), 1);
    // The notification the dropped waiter had is handed on, not lost.
    drop(parked);
    assert!(
        timeout(Duration::from_secs(5), wake.notified())
            .await
            .is_ok(),
        "a change made while the waiter's waker panicked still wakes the next waiter"
    );

    // The end of the member publishes a leave through the same wake.
    let waker = Waker::from(Arc::new(Loud(AtomicBool::new(false))));
    let mut cx = Context::from_waker(&waker);
    let mut parked = Box::pin(wake.notified());
    assert!(parked.as_mut().poll(&mut cx).is_pending());
    subject.stop(StopReason::Graceful).await.unwrap();
    wait_ended(&subject).await;
    assert!(scope.members_any("g").is_empty());
    assert_eq!(
        scope.inner().lock().subs.state_for_test(id),
        Some((2, false)),
        "the leave was recorded though the wake panicked"
    );
}

async fn spawn_many(system: &Arc<ActorSystem>, n: usize) -> Vec<ActorHandle<Subject>> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(spawn_in(system, Subject::plain()).await);
    }
    out
}

/// A scope kept past its system still holds the allocation of the system's
/// telemetry state, so the identity it compares by is never the identity of a
/// system built later: the scope refuses the handle of a newer system, and
/// the state it names has no owner left.
#[tokio::test]
async fn a_scope_kept_past_its_system_refuses_the_handle_of_a_newer_one() {
    let first = fresh_system("kept-scope");
    let scope = first.pg();
    let identity = scope.inner.system.clone();
    assert!(
        scope.inner.system.weak_count() >= 1 && scope.inner.system.strong_count() >= 1,
        "the scope's weak reference counts while the system lives"
    );
    // The shutdown lets the process map go, so the drop is the last one.
    first.shutdown().await;
    drop(first);
    let deadline = Instant::now() + Duration::from_secs(10);
    while scope.inner.system.strong_count() != 0 {
        assert!(Instant::now() < deadline, "the dropped system's state ends");
        tokio::task::yield_now().await;
    }
    assert!(
        std::ptr::eq(scope.inner.system.as_ptr(), identity.as_ptr()),
        "the scope still names the first state's allocation"
    );
    for _ in 0..4 {
        let newer = fresh_system("kept-scope-newer");
        let subject = spawn_in(&newer, Subject::plain()).await;
        assert_eq!(scope.join("g", &subject), Err(PgError::ForeignSystem));
        assert_eq!(scope.leave("g", &subject), Err(PgError::ForeignSystem));
        assert!(scope.members_any("g").is_empty());
    }
}
