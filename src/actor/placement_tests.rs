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
