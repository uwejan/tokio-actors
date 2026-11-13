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
use crate::actor::Actor;
use crate::error::{ActorError, ActorResult};

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
