//! A child placed on another runtime is known to the system by that runtime:
//! its registry entry holds it, so every kill path schedules the abort on its
//! reaper, and the link guard of the child feeds the same reaper. A child
//! that is not placed, or placed on its supervisor's own runtime, uses the
//! supervisor's reaper and no other.

use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::actor::context::ActorContext;
use crate::actor::{Actor, ActorExt};
use crate::error::ActorResult;
use crate::system::{ActorSystem, ReaperEntry};
use crate::types::StopReason;

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

fn unique(base: &str) -> String {
    format!("placement-{base}-{}", uuid::Uuid::new_v4())
}

/// A worker that only answers, or, when it is handed a channel, reports that
/// it began the message and then never finishes it. A child parked in a
/// handler cannot leave its registry entry on its own: a `Kill` reaches it
/// only through the abort its reaper entry owes.
struct Leaf;

impl Actor for Leaf {
    type Message = Option<mpsc::UnboundedSender<()>>;
    type Response = ();

    async fn handle(
        &mut self,
        msg: Option<mpsc::UnboundedSender<()>>,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if let Some(entered) = msg {
            let _ = entered.send(());
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

/// Starts a [`Leaf`] child, on the runtime it is told, or on its own.
struct Parent;

impl Actor for Parent {
    type Message = (String, Option<Handle>);
    type Response = ();

    async fn handle(
        &mut self,
        (name, runtime): (String, Option<Handle>),
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let spawn = ctx.spawn_child(|| Leaf).named(name);
        match runtime {
            Some(runtime) => spawn.on_runtime(&runtime).await?,
            None => spawn.await?,
        };
        Ok(())
    }
}

fn runtime() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

/// A system, its parent, and a child started by the parent on `placement`,
/// with the reapers of both runtimes replaced by counters before the child
/// exists.
struct Rig {
    system: Arc<ActorSystem>,
    parent: crate::ActorHandle<Parent>,
    child: String,
    own_reaper: mpsc::UnboundedReceiver<ReaperEntry>,
    placed_reaper: Option<mpsc::UnboundedReceiver<ReaperEntry>>,
}

async fn rig(placement: Placement<'_>) -> Rig {
    let system = ActorSystem::create(unique("system")).unwrap();
    let parent = Parent
        .spawn()
        .named(unique("parent"))
        .on_system(&system)
        .supervisor()
        .await
        .unwrap();
    let own_reaper = system.capture_reaper(&Handle::current());
    let (runtime, placed_reaper) = match placement {
        Placement::Unplaced => (None, None),
        Placement::OnItsOwn => (Some(Handle::current()), None),
        Placement::Elsewhere(other) => (Some(other.clone()), Some(system.capture_reaper(other))),
    };
    let child = unique("child");
    parent.send((child.clone(), runtime)).await.unwrap();
    Rig {
        system,
        parent,
        child,
        own_reaper,
        placed_reaper,
    }
}

/// Parks the child of `rig` in a handler and returns once it is there, so the
/// child cannot end, nor be replaced under its name, before the test's `Kill`:
/// an idle child can observe the `Kill`, leave the registry and be restarted
/// before the system reads its registry entry to arm the abort, and a target
/// already out of the registry is owed no abort.
async fn park_child(rig: &Rig) {
    let (entered_tx, mut entered) = mpsc::unbounded_channel();
    rig.system
        .get::<Leaf>(&rig.child)
        .unwrap()
        .notify(Some(entered_tx))
        .await
        .unwrap();
    timeout(HANG_BOUND, entered.recv())
        .await
        .expect("the child begins the message")
        .expect("the child reports");
}

enum Placement<'a> {
    Unplaced,
    OnItsOwn,
    Elsewhere(&'a Handle),
}

fn end_runtime(runtime: Runtime) {
    std::thread::spawn(move || drop(runtime)).join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_registry_entry_of_a_placed_child_holds_the_placed_runtime() {
    let other = runtime();
    let rig = rig(Placement::Elsewhere(other.handle())).await;
    let child = rig.system.get::<Leaf>(&rig.child).unwrap();
    let (_, _, runtime) = rig
        .system
        .kill_target_for(child.id(), &child.status_rx())
        .unwrap();
    assert_eq!(runtime.id(), other.handle().id());
    end_runtime(other);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_registry_entry_of_an_unplaced_child_holds_the_supervisors_runtime() {
    let rig = rig(Placement::Unplaced).await;
    let child = rig.system.get::<Leaf>(&rig.child).unwrap();
    let (_, _, runtime) = rig
        .system
        .kill_target_for(child.id(), &child.status_rx())
        .unwrap();
    assert_eq!(runtime.id(), Handle::current().id());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_of_a_placed_child_is_scheduled_on_the_placed_runtime_alone() {
    let other = runtime();
    let mut rig = rig(Placement::Elsewhere(other.handle())).await;
    park_child(&rig).await;
    rig.system.kill(&rig.child).await.unwrap();
    // The child is parked, so it is still registered when the `Kill` is armed,
    // and the system hands the entry to the reaper before `kill` returns: an
    // entry is queued by now or never will be.
    let placed = rig.placed_reaper.as_mut().unwrap();
    assert!(placed.try_recv().is_ok(), "the kill reaches the reaper");
    assert!(
        rig.own_reaper.try_recv().is_err(),
        "the supervisor's reaper holds nothing"
    );
    end_runtime(other);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_link_guard_of_a_placed_child_feeds_the_placed_reaper() {
    let other = runtime();
    let mut rig = rig(Placement::Elsewhere(other.handle())).await;
    // A killed supervisor drops the runners that hold its children's guards.
    rig.parent.stop(StopReason::Kill).await.unwrap();
    timeout(HANG_BOUND, rig.parent.wait_stopped())
        .await
        .expect("the supervisor ends");
    let placed = rig.placed_reaper.as_mut().unwrap();
    timeout(HANG_BOUND, placed.recv())
        .await
        .expect("the guard reaches the reaper")
        .expect("the reaper is open");
    assert!(placed.try_recv().is_err(), "one entry for one child");
    // The supervisor's reaper holds at most the abort of the supervisor
    // itself, which is registered on its own runtime, and nothing of the
    // child. The supervisor's own entry is absent when its task ended between
    // the raise and the read of its registry entry, so it is not required.
    let mut own = 0;
    while rig.own_reaper.try_recv().is_ok() {
        own += 1;
    }
    assert!(
        own <= 1,
        "the supervisor's reaper holds only the supervisor's own entry, not {own}"
    );
    end_runtime(other);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_on_its_supervisors_runtime_uses_one_reaper() {
    for placement in [Placement::Unplaced, Placement::OnItsOwn] {
        let mut rig = rig(placement).await;
        park_child(&rig).await;
        let child = rig.system.get::<Leaf>(&rig.child).unwrap();
        let (_, _, runtime) = rig
            .system
            .kill_target_for(child.id(), &child.status_rx())
            .unwrap();
        assert_eq!(runtime.id(), Handle::current().id());
        rig.system.kill(&rig.child).await.unwrap();
        assert!(
            rig.own_reaper.try_recv().is_ok(),
            "the kill reaches the reaper"
        );
        assert_eq!(rig.system.reaper_slots(), 1, "one reaper in all");
    }
}
