//! What a spawn tells the mailbox and the run loop about the runtime the
//! actor runs on: how many worker threads it has and whether it is a
//! current-thread runtime. Both are read once, at the spawn, from the handle
//! the task is spawned on, so a child placed on another runtime gets the facts
//! of that runtime and not of its parent's, and a restart lands with the same
//! facts. The two are separate facts: a multi-thread runtime with one worker
//! has one worker and is not a current-thread runtime.

use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::{Builder, Handle};
use tokio::sync::Notify;

use crate::actor::context::ActorContext;
use crate::actor::mailbox::MAILBOX_LANES;
use crate::actor::runtime::loop_facts;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult};
use crate::system::ActorSystem;
use crate::types::StopReason;

/// The longest a wait may take. It only limits how long a real hang takes to
/// report; a slow host must not read as one.
const HANG: Duration = Duration::from_secs(60);

fn unique(what: &str) -> String {
    format!("facts-{what}-{}", uuid::Uuid::new_v4())
}

enum Cmd {
    /// Answers, so the sender knows the run loop has started.
    Ping,
    /// Fails the handler, which ends the incarnation.
    Crash,
}

/// A worker that answers and can be told to fail.
struct Leaf;

impl Actor for Leaf {
    type Message = Cmd;
    type Response = ();

    async fn handle(&mut self, msg: Cmd, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Cmd::Ping => Ok(()),
            Cmd::Crash => Err(ActorError::user("crash on request")),
        }
    }
}

/// Starts a [`Leaf`] child on the runtime it is told, or on its own.
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

/// A current-thread runtime driven on a thread of its own until it is
/// dropped.
struct Driven {
    handle: Handle,
    stop: Arc<Notify>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Driven {
    fn new() -> Self {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let handle = runtime.handle().clone();
        let stop = Arc::new(Notify::new());
        let signal = stop.clone();
        let thread = std::thread::spawn(move || {
            runtime.block_on(async move { signal.notified().await });
        });
        Self {
            handle,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Driven {
    fn drop(&mut self) {
        self.stop.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Polls `ready` until it holds. The bound only limits how long a real hang
/// takes to report.
async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + HANG;
    while !ready() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// A child of a parent on a four-worker multi-thread runtime, placed on a
/// current-thread runtime, on a one-worker multi-thread runtime, on a
/// three-worker one and on none (the parent's own), each with the lane count
/// and the facts of its own runtime; the restart of the child placed on the
/// current-thread runtime lands with the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_placed_child_reads_the_worker_count_and_the_flavor_of_its_own_runtime() {
    let system = ActorSystem::create(unique("system")).unwrap();
    let parent = Parent
        .spawn()
        .named(unique("parent"))
        .on_system(&system)
        .supervisor()
        .await
        .unwrap();
    let current_thread = Driven::new();
    let one_worker = Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let three_workers = Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .unwrap();

    let cases = [
        ("the parent's own runtime", None, MAILBOX_LANES, (4, false)),
        (
            "a current-thread runtime",
            Some(current_thread.handle.clone()),
            1,
            (1, true),
        ),
        (
            "a one-worker multi-thread runtime",
            Some(one_worker.handle().clone()),
            1,
            (1, false),
        ),
        (
            "a three-worker multi-thread runtime",
            Some(three_workers.handle().clone()),
            MAILBOX_LANES,
            (3, false),
        ),
    ];
    let mut current_thread_child = None;
    for (label, placement, lanes, facts) in cases {
        let name = unique("child");
        parent.send((name.clone(), placement)).await.unwrap();
        let child = system
            .get::<Leaf>(&name)
            .unwrap_or_else(|| panic!("{label}: the child is registered"));
        // The answer comes from the run loop, which has recorded its facts.
        child.send(Cmd::Ping).await.unwrap();
        assert_eq!(child.mailbox_lanes(), lanes, "{label}: the mailbox's lanes");
        assert_eq!(
            loop_facts::read(child.id()),
            Some(facts),
            "{label}: the worker count and the current-thread flag of the run loop"
        );
        if label == "a current-thread runtime" {
            current_thread_child = Some((name, child));
        }
    }

    // The restart of a placed child goes through the same spawn with the
    // child's stored handle: the facts are the placement's again.
    let (name, child) = current_thread_child.expect("the current-thread child was placed");
    loop_facts::forget(child.id());
    let _ = child.send(Cmd::Crash).await;
    until("the restarted incarnation to start its loop", || {
        loop_facts::read(child.id()).is_some()
    })
    .await;
    assert_eq!(loop_facts::read(child.id()), Some((1, true)));
    let restarted = system.get::<Leaf>(&name).expect("the child is registered");
    restarted.send(Cmd::Ping).await.unwrap();
    assert_eq!(
        restarted.mailbox_lanes(),
        1,
        "the restarted incarnation's lanes"
    );

    // The parent stops its children, on the runtimes they were placed on,
    // before those runtimes end.
    parent.stop(StopReason::Graceful).await.unwrap();
    tokio::time::timeout(HANG, parent.wait_stopped())
        .await
        .expect("the parent stops with its children");
    one_worker.shutdown_background();
    three_workers.shutdown_background();
}
