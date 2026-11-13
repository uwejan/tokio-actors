#![allow(dead_code)]
//! The actor the link tests drive. A `Node` links, unlinks, traps exits and
//! ends the way a command tells it to, and logs what its `handle`,
//! `on_stopped` and `on_child_stopped` see, in the order they see it.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Barrier, Semaphore};
use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorId, ActorResult, ActorStatus, ActorStatusInfo,
    ActorSystem, ChildEvent, LinkError, LinkExit, MailboxConfig, RestartType, StopReason,
};

pub const BOUND: Duration = Duration::from_secs(10);
pub const QUIET: Duration = Duration::from_millis(300);

static UNIQ: AtomicU64 = AtomicU64::new(0);

pub fn uname(base: &str) -> String {
    format!("link-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

pub async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

/// A hold a test opens: a turn that waits on it is suspended, so the actor's
/// run loop polls nothing else until the test releases it, on either runtime
/// flavor.
pub type Hold = Arc<Semaphore>;

/// A turn's pause: the turn signals `entered`, then waits for `release`.
#[derive(Clone, Debug)]
pub struct Pause {
    entered: Hold,
    release: Hold,
}

pub fn pause() -> Pause {
    Pause {
        entered: Arc::new(Semaphore::new(0)),
        release: Arc::new(Semaphore::new(0)),
    }
}

impl Pause {
    /// Waits until the turn that holds this pause has started.
    pub async fn entered(&self) {
        within("a paused turn to start", self.entered.acquire())
            .await
            .expect("the semaphore is open")
            .forget();
    }

    /// Lets the turn go on.
    pub fn release(&self) {
        self.release.add_permits(1);
    }
}

/// How a command names the actor it links to.
pub enum Peer {
    Handle(ActorHandle<Node>),
    Id(ActorId),
    Name(String),
}

impl Peer {
    fn link(&self, ctx: &mut ActorContext<Node>, priority: bool) -> Result<(), LinkError> {
        match (self, priority) {
            (Peer::Handle(h), false) => ctx.link(h),
            (Peer::Handle(h), true) => ctx.link_priority(h),
            (Peer::Id(id), false) => ctx.link(id),
            (Peer::Id(id), true) => ctx.link_priority(id),
            (Peer::Name(n), false) => ctx.link(n.as_str()),
            (Peer::Name(n), true) => ctx.link_priority(n.as_str()),
        }
    }

    fn unlink(&self, ctx: &mut ActorContext<Node>) -> bool {
        match self {
            Peer::Handle(h) => ctx.unlink(h),
            Peer::Id(id) => ctx.unlink(id),
            Peer::Name(n) => ctx.unlink(n.as_str()),
        }
    }
}

pub enum Cmd {
    Trap,
    /// Traps with a map that panics.
    TrapPanicking,
    Untrap,
    Link(Peer),
    LinkPriority(Peer),
    Unlink(Peer),
    /// Waits at the barrier, then links.
    LinkAt(Peer, Arc<Barrier>),
    /// Links and fails in the same turn.
    LinkThenFail(Peer),
    /// Pauses, and links once the pause is released, in the same turn.
    WaitThenLink(Pause, Peer),
    /// Pauses, and cancels the link once the pause is released, in the same
    /// turn.
    WaitThenUnlink(Pause, Peer),
    /// Pauses, and stops trapping once the pause is released, in the same turn.
    WaitThenUntrap(Pause),
    /// The message a trap map makes.
    Exit(LinkExit),
    Data(u32),
    Tell(ActorHandle<Node>, Vec<u32>),
    Wait(Pause),
    Fail,
    Panic,
    Hang,
    SelfStop(StopReason),
    Spawn(String, RestartType),
}

#[derive(Debug)]
pub enum Reply {
    Unit,
    Bool(bool),
    Link(Result<(), LinkError>),
    Child(Option<ActorHandle<Node>>),
}

#[derive(Debug)]
pub enum Event {
    /// The actor's status when an exit reached `handle`; logged just before
    /// the exit.
    Status(ActorStatus),
    Exit(LinkExit),
    Data(u32),
    Stopped(StopReason),
    Child(StopReason),
    LinkInStop(Result<(), LinkError>),
}

pub struct Node {
    pub log: mpsc::UnboundedSender<Event>,
    /// When set, `on_stopped` tries to link to this actor and logs the answer.
    pub link_in_stop: Option<ActorHandle<Node>>,
}

impl Actor for Node {
    type Message = Cmd;
    type Response = Reply;

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        match msg {
            Cmd::Trap => {
                ctx.trap_exit(Cmd::Exit);
                Ok(Reply::Unit)
            }
            Cmd::TrapPanicking => {
                ctx.trap_exit(|_: LinkExit| -> Cmd { panic!("the trap map panics") });
                Ok(Reply::Unit)
            }
            Cmd::Untrap => {
                ctx.untrap_exit();
                Ok(Reply::Unit)
            }
            Cmd::Link(peer) => Ok(Reply::Link(peer.link(ctx, false))),
            Cmd::LinkPriority(peer) => Ok(Reply::Link(peer.link(ctx, true))),
            Cmd::Unlink(peer) => Ok(Reply::Bool(peer.unlink(ctx))),
            Cmd::LinkAt(peer, barrier) => {
                barrier.wait().await;
                Ok(Reply::Link(peer.link(ctx, false)))
            }
            Cmd::WaitThenLink(pause, peer) => {
                pause.entered.add_permits(1);
                let _ = pause.release.acquire().await;
                Ok(Reply::Link(peer.link(ctx, false)))
            }
            Cmd::WaitThenUnlink(pause, peer) => {
                pause.entered.add_permits(1);
                let _ = pause.release.acquire().await;
                Ok(Reply::Bool(peer.unlink(ctx)))
            }
            Cmd::WaitThenUntrap(pause) => {
                pause.entered.add_permits(1);
                let _ = pause.release.acquire().await;
                ctx.untrap_exit();
                Ok(Reply::Unit)
            }
            Cmd::LinkThenFail(peer) => {
                let _ = peer.link(ctx, false);
                Err(ActorError::user("linker fails"))
            }
            Cmd::Exit(exit) => {
                let _ = self.log.send(Event::Status(ctx.status()));
                let _ = self.log.send(Event::Exit(exit));
                Ok(Reply::Unit)
            }
            Cmd::Data(value) => {
                let _ = self.log.send(Event::Data(value));
                Ok(Reply::Unit)
            }
            Cmd::Tell(other, values) => {
                for value in values {
                    let _ = other.notify(Cmd::Data(value)).await;
                }
                Ok(Reply::Unit)
            }
            Cmd::Wait(pause) => {
                pause.entered.add_permits(1);
                let _ = pause.release.acquire().await;
                Ok(Reply::Unit)
            }
            Cmd::Fail => Err(ActorError::user("boom")),
            Cmd::Panic => panic!("boom"),
            Cmd::Hang => {
                std::future::pending::<()>().await;
                Ok(Reply::Unit)
            }
            Cmd::SelfStop(reason) => {
                ctx.stop(reason).expect("the lane takes the request");
                Ok(Reply::Unit)
            }
            Cmd::Spawn(name, restart) => {
                let log = self.log.clone();
                let child = ctx
                    .spawn_child(move || Node {
                        log: log.clone(),
                        link_in_stop: None,
                    })
                    .named(name)
                    .restart_type(restart)
                    .await?;
                Ok(Reply::Child(Some(child)))
            }
        }
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.log.send(Event::Stopped(reason.clone()));
        if let Some(target) = &self.link_in_stop {
            let _ = self.log.send(Event::LinkInStop(ctx.link(target)));
        }
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.log.send(Event::Child(event.reason.clone()));
        Ok(())
    }
}

pub type Log = mpsc::UnboundedReceiver<Event>;

/// What a node is built with.
#[derive(Clone, Copy, Default)]
pub struct Opts {
    pub supervisor: bool,
    pub priority_lane: bool,
    /// The mailbox capacity, when not the default.
    pub capacity: Option<usize>,
}

pub async fn node(opts: Opts) -> (ActorHandle<Node>, Log) {
    node_with(opts, None).await
}

pub async fn node_with(
    opts: Opts,
    link_in_stop: Option<ActorHandle<Node>>,
) -> (ActorHandle<Node>, Log) {
    let (log, rx) = mpsc::unbounded_channel();
    let mut config = ActorConfig::default();
    if let Some(capacity) = opts.capacity {
        config.mailbox = MailboxConfig::default().with_capacity(capacity);
    }
    if opts.priority_lane {
        config.mailbox = config.mailbox.with_priority_lane(8);
    }
    let mut builder = Node { log, link_in_stop }
        .spawn()
        .named(uname("node"))
        .with_config(config);
    if opts.supervisor {
        builder = builder.supervisor();
    }
    (builder.await.expect("the node spawns"), rx)
}

/// A node in the given system, running on the given runtime: a root is placed
/// by spawning the spawn call onto that runtime.
pub async fn node_placed(
    system: &Arc<ActorSystem>,
    runtime: &tokio::runtime::Handle,
) -> (ActorHandle<Node>, Log) {
    let (log, rx) = mpsc::unbounded_channel();
    let system = system.clone();
    let name = uname("node");
    let handle = runtime
        .spawn(async move {
            Node {
                log,
                link_in_stop: None,
            }
            .spawn()
            .named(name)
            .on_system(&system)
            .await
        })
        .await
        .expect("the spawn task runs")
        .expect("the node spawns");
    (handle, rx)
}

pub async fn ask(h: &ActorHandle<Node>, cmd: Cmd) -> Reply {
    within("a node's answer", h.send(cmd))
        .await
        .expect("the node answers")
}

pub async fn trap(h: &ActorHandle<Node>) {
    ask(h, Cmd::Trap).await;
}

pub async fn link(h: &ActorHandle<Node>, peer: Peer) -> Result<(), LinkError> {
    match ask(h, Cmd::Link(peer)).await {
        Reply::Link(result) => result,
        other => panic!("not a link answer: {other:?}"),
    }
}

pub async fn unlink(h: &ActorHandle<Node>, peer: Peer) -> bool {
    match ask(h, Cmd::Unlink(peer)).await {
        Reply::Bool(done) => done,
        other => panic!("not an unlink answer: {other:?}"),
    }
}

pub async fn status(h: &ActorHandle<Node>) -> ActorStatusInfo {
    within("a status", h.get_status())
        .await
        .expect("the node answers")
}

/// The next exit the node's `handle` saw, skipping other events.
pub async fn next_exit(log: &mut Log) -> LinkExit {
    loop {
        match within("an exit", log.recv()).await {
            Some(Event::Exit(exit)) => return exit,
            Some(_) => {}
            None => panic!("the node's log closed"),
        }
    }
}

/// The next message or exit as a number: 0 for an exit, the value for data.
/// Status marks are skipped.
pub async fn next_marker(log: &mut Log) -> u32 {
    loop {
        match next_event(log).await {
            Event::Exit(_) => return 0,
            Event::Data(n) => return n,
            Event::Status(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// The next event of any kind.
pub async fn next_event(log: &mut Log) -> Event {
    within("an event", log.recv())
        .await
        .expect("the node's log is open")
}

pub async fn assert_quiet(log: &mut Log) {
    sleep(QUIET).await;
    while let Ok(event) = log.try_recv() {
        if !matches!(
            event,
            Event::Stopped(_) | Event::Child(_) | Event::Status(_)
        ) {
            panic!("unexpected event after the quiet window: {event:?}");
        }
    }
}

/// Every exit seen after a quiet window.
pub async fn exits_after_quiet(log: &mut Log) -> Vec<LinkExit> {
    sleep(QUIET).await;
    let mut seen = Vec::new();
    while let Ok(event) = log.try_recv() {
        if let Event::Exit(exit) = event {
            seen.push(exit);
        }
    }
    seen
}

/// Polls `read` until it returns true or the bound passes.
pub async fn eventually(what: &str, mut read: impl AsyncFnMut() -> bool) {
    let start = std::time::Instant::now();
    while !read().await {
        assert!(start.elapsed() < BOUND, "timed out waiting for: {what}");
        sleep(Duration::from_millis(5)).await;
    }
}
