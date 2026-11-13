//! Quiescence while a group restart is in flight: the supervisor owes work
//! from the group decision until the group's last member has its fate, so the
//! span between the group's stops and its starts is not quiet.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use super::turn_tests::{both_flavors, unique, until_showing};
use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::runtime::end_hooks;
use crate::actor::supervision::SupervisionConfig;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult};
use crate::system::{ActorSystem, SystemConfig};
use crate::types::{ActorId, ChildEvent, SupervisionAction};

/// Opens once, for a handler that runs on a worker thread and cannot await.
#[derive(Default)]
struct ThreadGate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl ThreadGate {
    fn wait(&self) {
        let guard = self.open.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(30), |open| !*open)
            .unwrap();
    }

    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

/// Opens its gate when dropped, so a failed assertion never leaves a handler
/// holding a thread.
struct OpenOnDrop(Arc<ThreadGate>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

#[derive(Clone, Copy)]
enum Order {
    Ping,
    Crash,
}

/// A member of the group: it crashes on `Crash` and notes the task it handles
/// on.
struct Member {
    task: Arc<Mutex<Option<tokio::task::Id>>>,
}

impl Actor for Member {
    type Message = Order;
    type Response = ();

    async fn handle(&mut self, msg: Order, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        *self.task.lock().unwrap() = tokio::task::try_id();
        match msg {
            Order::Ping => Ok(()),
            Order::Crash => Err(ActorError::user("crash on command")),
        }
    }
}

/// A `one_for_all` supervisor with two permanent members, `a` and `b`.
struct Boss {
    b_task: Arc<Mutex<Option<tokio::task::Id>>>,
    a_built: Arc<AtomicUsize>,
    b_built: Arc<AtomicUsize>,
}

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (task, built) = (Arc::new(Mutex::new(None)), self.a_built.clone());
        ctx.spawn_child(move || {
            built.fetch_add(1, Ordering::SeqCst);
            Member { task: task.clone() }
        })
        .named("a")
        .await?;
        let (task, built) = (self.b_task.clone(), self.b_built.clone());
        ctx.spawn_child(move || {
            built.fetch_add(1, Ordering::SeqCst);
            Member { task: task.clone() }
        })
        .named("b")
        // `b` holds its thread in a handler until the test lets go, so its
        // stop must not be bounded by a clock: with a bound the supervisor
        // ends `b` as unresponsive after the shutdown timeout, and the group
        // with it, whatever the test is doing by then.
        .shutdown(crate::types::Shutdown::Infinity)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// The supervisor's owed-work book, read from its identity.
fn book(sys: &ActorSystem, boss: &ActorHandle<Boss>) -> (u32, bool) {
    let identity = super::identity_of(sys, boss.stop_lane_ref().abort_record())
        .expect("the supervisor has an identity");
    (identity.owed_deaths_for_test(), identity.group_is_pending())
}

/// A supervisor with one permanent child `c`, which counts its own turns.
struct Chief {
    handled: Arc<AtomicUsize>,
    child: &'static str,
}

impl Actor for Chief {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Member {
            task: Arc::default(),
        })
        .named(self.child)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.handled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A worker that fails on its first message; from its second build on, its
/// start waits on the gate it is given.
struct Kid {
    hold: Option<Arc<tokio::sync::Notify>>,
}

impl Actor for Kid {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(gate) = &self.hold {
            gate.notified().await;
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("crash on command"))
    }
}

/// A supervisor of one permanent child whose turn waits on `release`, and
/// which notes the reason of each death it is told of.
struct Adopter {
    child: &'static str,
    start: Arc<tokio::sync::Notify>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    deaths: Arc<Mutex<Vec<crate::types::StopReason>>>,
}

impl Actor for Adopter {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let built = Arc::new(AtomicUsize::new(0));
        let start = self.start.clone();
        ctx.spawn_child(move || Kid {
            hold: (built.fetch_add(1, Ordering::SeqCst) > 0).then(|| start.clone()),
        })
        .named(self.child)
        .shutdown(crate::types::Shutdown::Timeout(Duration::from_millis(50)))
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.deaths.lock().unwrap().push(event.reason.clone());
        Ok(())
    }
}

/// Where the supervisor adopts the restarted child.
#[derive(Clone, Copy, Debug)]
enum AdoptedIn {
    /// In its message loop, after the turn that held it.
    Loop,
    /// In its final collection, stopped inside that turn.
    FinalCollection,
}

/// How the restart attempt meets the restarted child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reads {
    /// The attempt reads the child's acknowledgment first.
    AckFirst,
    /// The attempt reads the child's end first: its acknowledgment branch is
    /// parked before the read, so the child's acknowledgment is stored and its
    /// task ended when the attempt reads.
    EndFirst,
    /// The attempt is held after it spawned the child, until the supervisor's
    /// final collection waits for it: the child starts, crashes and ends
    /// while the collection reads the attempt's slot.
    HeldAfterSpawn,
}

/// The rows of the test: each site that adopts an attempt, with the attempt
/// reading the child's acknowledgment first and its end first, and the final
/// collection also with the attempt held after its spawn.
const ADOPTED_ROWS: [(AdoptedIn, Reads); 5] = [
    (AdoptedIn::Loop, Reads::AckFirst),
    (AdoptedIn::Loop, Reads::EndFirst),
    (AdoptedIn::FinalCollection, Reads::AckFirst),
    (AdoptedIn::FinalCollection, Reads::EndFirst),
    (AdoptedIn::FinalCollection, Reads::HeldAfterSpawn),
];

/// A restarted child that started, then ended while its supervisor was held
/// in a turn, is owed to the supervisor from the moment the attempt that
/// started it settles: read just after the settle, before the runner exists,
/// the system is not quiet. The child is adopted, and its own end is told to
/// the supervisor, whichever way the attempt meets it ([`ADOPTED_ROWS`]).
async fn an_adopted_child_that_ended_first_is_owed_when_its_attempt_settles(
    row: AdoptedIn,
    reads: Reads,
) {
    let sys = ActorSystem::create_with(
        unique("adopted"),
        SystemConfig::default().with_turn_tracking(),
    )
    .unwrap();
    let name: &'static str = Box::leak(unique("adopted-child").into_boxed_str());
    let (start, entered, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let deaths: Arc<Mutex<Vec<crate::types::StopReason>>> = Arc::default();
    let boss = Adopter {
        child: name,
        start: start.clone(),
        entered: entered.clone(),
        release: release.clone(),
        deaths: deaths.clone(),
    }
    .spawn()
    .on_system(&sys)
    .with_supervision(SupervisionConfig::one_for_one().max_restarts(3, Duration::from_secs(60)))
    .await
    .unwrap();
    let identity = super::identity_of(&sys, boss.stop_lane_ref().abort_record())
        .expect("the supervisor has an identity");

    // Where the supervisor's book stands, for a wait that does not end.
    let state = || {
        format!(
            "supervisor alive={} restarts_in_flight={} owed={} open={} group={} child alive={}",
            boss.is_alive(),
            identity.restarts_in_flight(),
            identity.owed_deaths_for_test(),
            identity.open_deaths_for_test(),
            identity.group_is_pending(),
            sys.get::<Kid>(name).is_some_and(|kid| kid.is_alive()),
        )
    };

    // With `EndFirst`, the attempt's acknowledgment branch is parked before it
    // reads, and the attempt notes when its end branch reads the
    // acknowledgment. With `HeldAfterSpawn`, the attempt is held after its
    // spawn until the final collection waits for it.
    let late = Arc::new(AtomicBool::new(false));
    let held = Arc::new(tokio::sync::Notify::new());
    // No task of the death plane that follows the child may be lost: an
    // adopted child that ended first is followed by its runner to the end the
    // attempt took, and a lost runner would be told only by its fallback.
    let runner_lost = Arc::new(AtomicBool::new(false));
    {
        let runner_lost = runner_lost.clone();
        end_hooks::attach(end_hooks::RUNNER_LOST, ActorId::from(name), move || {
            runner_lost.store(true, Ordering::SeqCst)
        });
    }
    if reads == Reads::HeldAfterSpawn {
        end_hooks::attach_hold(end_hooks::SPAWNED, ActorId::from(name), held.clone());
        let held = held.clone();
        end_hooks::attach(end_hooks::ATTEMPT_WAIT, boss.id().clone(), move || {
            held.notify_one()
        });
    }
    if reads == Reads::EndFirst {
        end_hooks::attach_hold(
            end_hooks::ACK_GAP,
            ActorId::from(name),
            Arc::new(tokio::sync::Notify::new()),
        );
        let late = late.clone();
        end_hooks::attach(end_hooks::ACK_LATE, ActorId::from(name), move || {
            late.store(true, Ordering::SeqCst)
        });
    }

    // The first child crashes; the attempt's child holds its start.
    let first = sys.get::<Kid>(name).expect("the child runs");
    first.notify(()).await.unwrap();
    until_showing(
        &format!("the attempt is in flight ({row:?}, reads={reads:?})"),
        state,
        || identity.restarts_in_flight() == 1,
    )
    .await;
    // The name is free between the first incarnation's end and the attempt's
    // registration of the next, so wait until it names a different one. The
    // first handle stays held so its record's address is not reused.
    let first_key = first.stop_lane_ref().abort_record().key();
    let found: Arc<Mutex<Option<ActorHandle<Kid>>>> = Arc::default();
    until_showing(
        &format!("the attempt's child holds the name ({row:?}, reads={reads:?})"),
        state,
        || {
            let next = sys
                .get::<Kid>(name)
                .filter(|h| h.stop_lane_ref().abort_record().key() != first_key);
            let held = next.is_some();
            if held {
                *found.lock().unwrap() = next;
            }
            held
        },
    )
    .await;
    let restarted = found.lock().unwrap().take().expect("the child was found");

    // The supervisor is held in a turn while the restarted child starts,
    // crashes and ends.
    boss.notify(()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("the supervisor's turn begins");
    start.notify_one();
    restarted.notify(()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), restarted.wait_stopped())
        .await
        .expect("the restarted child ends");

    let seen: Arc<Mutex<Option<Option<bool>>>> = Arc::default();
    {
        let (seen, sys) = (seen.clone(), sys.clone());
        end_hooks::attach(end_hooks::SETTLED, ActorId::from(name), move || {
            *seen.lock().unwrap() = Some(sys.is_quiescent());
        });
    }
    if let AdoptedIn::FinalCollection = row {
        boss.stop(crate::types::StopReason::Graceful).await.unwrap();
    }
    release.notify_one();
    until_showing(
        &format!("the attempt settled ({row:?}, reads={reads:?})"),
        state,
        || seen.lock().unwrap().is_some(),
    )
    .await;
    assert_eq!(
        *seen.lock().unwrap(),
        Some(Some(false)),
        "{row:?}, reads={reads:?}: the restarted child ended before its claim and is owed when its attempt settles"
    );
    assert_eq!(
        late.load(Ordering::SeqCst),
        reads == Reads::EndFirst,
        "{row:?}, reads={reads:?}: the attempt read the acknowledgment from its end branch exactly when the end came first"
    );
    // The adopted child's end is told to the supervisor with the reason the
    // child ended with, whichever branch of the attempt read it.
    until_showing(
        &format!("the adopted child's death is told ({row:?}, reads={reads:?})"),
        state,
        || deaths.lock().unwrap().len() == 2,
    )
    .await;
    held.notify_one();
    let told = deaths.lock().unwrap()[1].clone();
    assert!(
        matches!(&told, crate::types::StopReason::Failure(err) if err.to_string().contains("crash on command")),
        "{row:?}, reads={reads:?}: the adopted child's own crash is told, got {told:?}"
    );
    assert!(
        !runner_lost.load(Ordering::SeqCst),
        "{row:?}, reads={reads:?}: no runner of the adopted child was lost"
    );
}

#[test]
fn an_adopted_child_that_ended_first_is_owed_when_its_attempt_settles_current_thread() {
    for (row, reads) in ADOPTED_ROWS {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(
                an_adopted_child_that_ended_first_is_owed_when_its_attempt_settles(row, reads),
            );
    }
}

/// A supervisor of one permanent member whose turn holds, and whose final
/// delivery of a death holds.
struct Warden {
    child: String,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    delivering: Arc<Notify>,
    proceed: Arc<Notify>,
}

impl Actor for Warden {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Member {
            task: Arc::default(),
        })
        .named(self.child.clone())
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if event.action == SupervisionAction::SupervisorStopping {
            self.delivering.notify_one();
            self.proceed.notified().await;
        }
        Ok(())
    }
}

/// A death the final collection takes for its final delivery is decided by
/// the collection: no restart is owed for it, so the supervisor reads quiet
/// of it while the delivery runs.
#[test]
fn a_death_collected_for_the_final_delivery_is_no_longer_owed() {
    both_flavors(|| async {
        let sys = ActorSystem::create_with(
            unique("collected"),
            SystemConfig::default().with_turn_tracking(),
        )
        .unwrap();
        let name = unique("collected-child");
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let (delivering, proceed) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let warden = Warden {
            child: name.clone(),
            entered: entered.clone(),
            release: release.clone(),
            delivering: delivering.clone(),
            proceed: proceed.clone(),
        }
        .spawn()
        .named(unique("warden"))
        .on_system(&sys)
        .with_supervision(SupervisionConfig::one_for_one().max_restarts(3, Duration::from_secs(60)))
        .await
        .unwrap();
        let child = loop {
            if let Some(child) = sys.get::<Member>(&name) {
                break child;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };

        // The supervisor's turn holds; the child then dies, and the supervisor
        // is stopped inside the turn, so the death waits for the final
        // collection.
        warden.notify(()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), entered.notified())
            .await
            .expect("the supervisor's turn begins");
        child.notify(Order::Crash).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), child.wait_stopped())
            .await
            .expect("the child ends");
        warden
            .stop(crate::types::StopReason::Graceful)
            .await
            .unwrap();
        release.notify_one();

        tokio::time::timeout(Duration::from_secs(10), delivering.notified())
            .await
            .expect("the final delivery runs");
        let identity = super::identity_of(&sys, warden.stop_lane_ref().abort_record())
            .expect("the supervisor has an identity");
        assert_eq!(
            identity.owed_deaths_for_test(),
            0,
            "the collected death is still owed a decision during its delivery"
        );
        proceed.notify_one();
        tokio::time::timeout(Duration::from_secs(10), warden.wait_stopped())
            .await
            .expect("the supervisor ends");
    });
}

/// What a runner's claim finds of the child it follows decides when the
/// child's death is owed: from the claim when the child's end is already
/// recorded by the registry guard's flag, by a final reason or by `Stopped`
/// (an end that ran while turn tracking was off wrote only the plane), and
/// from the child's end otherwise.
#[test]
fn a_child_found_ended_at_its_claim_is_owed_from_the_claim() {
    use crate::actor::subtree::AbortRecord;
    use crate::types::{StatusValue, StopReason};

    type Mark = fn(&AbortRecord, &crate::types::StatusTx);
    let rows: [(&str, Mark, u32); 4] = [
        ("no sign of an end", |_, _| {}, 0),
        (
            "the guard's end flag",
            |record, _| record.mark_guard_ended(),
            1,
        ),
        (
            "a final reason written",
            |_, tx| {
                StatusValue::write_reason(tx, StopReason::Graceful, false);
            },
            1,
        ),
        (
            "the plane reading stopped",
            |_, tx| {
                StatusValue::write_reason(tx, StopReason::Graceful, true);
            },
            1,
        ),
    ];
    both_flavors(|| async {
        let sys = ActorSystem::create_with(
            unique("found"),
            SystemConfig::default().with_turn_tracking(),
        )
        .unwrap();
        let sup = Member {
            task: Arc::default(),
        }
        .spawn()
        .named(unique("found-sup"))
        .on_system(&sys)
        .await
        .unwrap();
        let identity = super::identity::identity_for(
            sys.telemetry_mode(),
            sup.stop_lane_ref().abort_record(),
            Some(&sys),
            None,
        )
        .expect("the supervisor has an identity");
        for (index, (row, mark, owed)) in rows.iter().enumerate() {
            let incarnation = 9_000 + index as u64;
            let child = AbortRecord::new();
            let (tx, _rx) = child.status_channel();
            mark(&child, &tx);
            let before = identity.owed_deaths_for_test();
            super::death_claim(sup.stop_lane_ref().abort_record(), incarnation, &child);
            assert_eq!(
                identity.owed_deaths_for_test() - before,
                *owed,
                "{row}: deaths owed after the claim"
            );
            identity.death_decided(incarnation);
        }
    });
}
