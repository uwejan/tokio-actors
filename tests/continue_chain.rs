#![allow(dead_code)]
//! `continue_with`: the message a callback asks to handle next runs after the
//! reply, before any other branch is read, and chains; a stored `Kill` is read
//! before each step; the message is dropped when the callback fails, stops its
//! actor or runs after the stop began.

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ChildEvent, RestartType, StopReason,
};

const PROMPT: Duration = Duration::from_secs(20);

/// A value whose drop is counted, to see a message that was dropped.
struct Tag(Arc<AtomicU32>);

impl Drop for Tag {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

enum Msg {
    Start,
    Step(u32),
    Count,
    Fail,
    Panic,
    /// Replies, and asks for `Fail` as the next step.
    ThenFail,
    /// Replies, and asks for `Panic` as the next step.
    ThenPanic,
    StopSelf,
    Marker,
    Tagged(&'static str, Tag),
    Twice(Arc<AtomicU32>),
    Block(oneshot::Sender<()>, oneshot::Receiver<()>),
    Watch(ActorHandle<Chain>),
    Down,
    Tick,
    /// A step that does nothing but ask for the next one, never awaiting.
    Spin,
    /// Makes a child that ends only when told to.
    SpawnChild(oneshot::Sender<ActorHandle<Leaf>>),
}

/// Where a spinning chain ends by itself.
const SPIN_LIMIT: u32 = 3_000_000;

/// A child of the actor under test.
struct Leaf;

impl Actor for Leaf {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StartHook {
    None,
    PreStart,
    OnStarted,
    OnStartedThenErr,
    OnStartedThenStop,
}

#[derive(Default)]
struct Seen {
    log: Mutex<Vec<String>>,
    steps: AtomicU32,
    markers: AtomicU32,
    stopped_hooks: AtomicU32,
    spins: AtomicU32,
    /// Calls of `on_child_stopped` and of `pre_stop` that finished.
    child_events: AtomicU32,
    pre_stops: AtomicU32,
    /// What the tagged messages' drop counter read when `on_stopped` began.
    dropped_at_stop: AtomicU32,
}

/// A step's gate: the sender that says the step entered and the receiver that
/// releases it, taken by the step that uses them.
type Gate = Arc<Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>>;

struct Chain {
    seen: Arc<Seen>,
    /// A chain started by `Start` has this many steps.
    limit: u32,
    /// Each step yields once, so other tasks run between two steps.
    yields: bool,
    /// The step that waits for a gate: it signals it entered and waits to be
    /// released.
    gate_at: Option<(u32, Gate)>,
    start: StartHook,
    stop_sets_marker: bool,
    /// The callbacks that do not run the chain ask for a tagged message, which
    /// this counter sees dropped.
    tagged: Option<Arc<AtomicU32>>,
    /// `pre_stop` vetoes the first stop it is asked about.
    veto_once: bool,
}

impl Chain {
    fn new(seen: &Arc<Seen>) -> Self {
        Self {
            seen: seen.clone(),
            limit: 1,
            yields: false,
            gate_at: None,
            start: StartHook::None,
            stop_sets_marker: false,
            tagged: None,
            veto_once: false,
        }
    }

    fn with_gate(mut self, step: u32) -> (Self, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        self.gate_at = Some((step, Arc::new(Mutex::new(Some((entered, release_rx))))));
        (self, entered_rx, release)
    }

    fn note(&self, what: String) {
        self.seen.log.lock().unwrap().push(what);
    }
}

impl Actor for Chain {
    type Message = Msg;
    type Response = u32;

    async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.start == StartHook::PreStart {
            ctx.continue_with(Msg::Step(1));
        }
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match self.start {
            StartHook::OnStarted => ctx.continue_with(Msg::Step(1)),
            StartHook::OnStartedThenErr => {
                ctx.continue_with(Msg::Marker);
                return Err(ActorError::user("start failed"));
            }
            StartHook::OnStartedThenStop => {
                ctx.continue_with(Msg::Step(1));
                ctx.stop(StopReason::Graceful)
                    .expect("a graceful stop is accepted");
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        _event: &ChildEvent,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if let Some(dropped) = &self.tagged {
            ctx.continue_with(Msg::Tagged("child", Tag(dropped.clone())));
        }
        self.seen.child_events.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, ctx: &mut ActorContext<Self>) -> bool {
        let first = self.veto_once && self.seen.pre_stops.load(Ordering::SeqCst) == 0;
        if first {
            if let Some(dropped) = &self.tagged {
                ctx.continue_with(Msg::Tagged("veto", Tag(dropped.clone())));
            }
        }
        self.seen.pre_stops.fetch_add(1, Ordering::SeqCst);
        !first
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.seen.stopped_hooks.fetch_add(1, Ordering::SeqCst);
        if let Some(dropped) = &self.tagged {
            self.seen
                .dropped_at_stop
                .store(dropped.load(Ordering::SeqCst), Ordering::SeqCst);
        }
        if self.stop_sets_marker {
            ctx.continue_with(Msg::Marker);
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        match msg {
            Msg::Start => {
                self.note("start".into());
                ctx.continue_with(Msg::Step(1));
            }
            Msg::Step(n) => {
                self.note(format!("step {n}"));
                self.seen.steps.fetch_add(1, Ordering::SeqCst);
                if self.yields {
                    tokio::task::yield_now().await;
                }
                let gate = match &self.gate_at {
                    Some((at, cell)) if *at == n => cell.lock().unwrap().take(),
                    _ => None,
                };
                if let Some((entered, release)) = gate {
                    let _ = entered.send(());
                    let _ = release.await;
                }
                if n < self.limit {
                    ctx.continue_with(Msg::Step(n + 1));
                }
            }
            Msg::Count => {}
            Msg::Fail => {
                ctx.continue_with(Msg::Marker);
                return Err(ActorError::user("failed"));
            }
            Msg::Panic => {
                ctx.continue_with(Msg::Marker);
                panic!("panicked");
            }
            Msg::ThenFail => ctx.continue_with(Msg::Fail),
            Msg::ThenPanic => ctx.continue_with(Msg::Panic),
            Msg::StopSelf => {
                ctx.continue_with(Msg::Marker);
                ctx.stop(StopReason::Graceful)
                    .expect("a graceful stop is accepted");
            }
            Msg::Marker => {
                self.seen.markers.fetch_add(1, Ordering::SeqCst);
            }
            Msg::Tagged(label, _tag) => self.note(label.to_string()),
            Msg::Twice(dropped) => {
                ctx.continue_with(Msg::Tagged("first", Tag(dropped.clone())));
                ctx.continue_with(Msg::Tagged("second", Tag(dropped)));
            }
            Msg::Block(entered, release) => {
                let _ = entered.send(());
                let _ = release.await;
            }
            Msg::Watch(other) => {
                ctx.monitor(&other, |_| Msg::Down).expect("monitor");
            }
            Msg::Down => {
                self.note("down".into());
                ctx.continue_with(Msg::Marker);
            }
            Msg::Tick => {
                self.note("tick".into());
                ctx.continue_with(Msg::Step(1));
            }
            Msg::Spin => {
                if self.seen.spins.fetch_add(1, Ordering::SeqCst) + 1 < SPIN_LIMIT {
                    ctx.continue_with(Msg::Spin);
                }
            }
            Msg::SpawnChild(reply) => {
                let child = ctx
                    .spawn_child(|| Leaf)
                    .restart_type(RestartType::Temporary)
                    .await?;
                let _ = reply.send(child);
            }
        }
        Ok(self.seen.steps.load(Ordering::SeqCst))
    }
}

async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

async fn spawn(chain: Chain) -> ActorHandle<Chain> {
    timeout(PROMPT, chain.spawn())
        .await
        .expect("the spawn is acknowledged")
        .unwrap()
}

/// The caller holds its answer before the step starts: the step waits at a
/// gate, and the reply still arrives.
#[test]
fn the_step_runs_after_the_reply() {
    common::run(2, async {
        let seen = Arc::new(Seen::default());
        let (chain, entered, release) = Chain::new(&seen).with_gate(1);
        let handle = spawn(chain).await;
        let answer = timeout(PROMPT, handle.send(Msg::Start))
            .await
            .expect("the reply does not wait for the step")
            .unwrap();
        assert_eq!(answer, 0, "the reply was made before the step ran");
        timeout(PROMPT, entered).await.unwrap().unwrap();
        release.send(()).unwrap();
        assert_eq!(handle.send(Msg::Count).await.unwrap(), 1);
        assert_eq!(*seen.log.lock().unwrap(), ["start", "step 1"]);
    });
}

/// A `Kill` stored while step 3 of a chain of 50 runs is read before step 4:
/// the rest of the chain does not begin and the stop hook does not run.
#[test]
fn kill_is_read_between_steps() {
    common::run(2, async {
        let seen = Arc::new(Seen::default());
        let (mut chain, entered, release) = Chain::new(&seen).with_gate(3);
        chain.limit = 50;
        chain.yields = true;
        let handle = spawn(chain).await;
        timeout(PROMPT, handle.send(Msg::Start))
            .await
            .expect("the reply does not wait for the chain")
            .unwrap();
        timeout(PROMPT, entered).await.unwrap().unwrap();
        handle.stop(StopReason::Kill).await.unwrap();
        release.send(()).unwrap();
        assert_eq!(
            timeout(PROMPT, handle.wait_stopped()).await.unwrap(),
            StopReason::Kill
        );
        assert_eq!(
            *seen.log.lock().unwrap(),
            ["start", "step 1", "step 2", "step 3"]
        );
        assert_eq!(seen.stopped_hooks.load(Ordering::SeqCst), 0);
    });
}

/// The message is dropped, not handled, when the callback returns an `Err`,
/// panics, stops its actor, runs after the stop began (`on_stopped`, a
/// notice delivered while the actor stops). A step of a chain that fails ends
/// the actor with `Failure` as the first callback's failure does, and the
/// message that step asked for is dropped too.
#[test]
fn dropped_on_err_panic_ctx_stop_and_after_the_stop_began() {
    #[derive(Clone, Copy, Debug)]
    enum Row {
        Err,
        Panic,
        StopSelf,
        OnStopped,
        NoticeAtTheStop,
        StepErr,
        StepPanic,
    }
    for row in [
        Row::Err,
        Row::Panic,
        Row::StopSelf,
        Row::OnStopped,
        Row::NoticeAtTheStop,
        Row::StepErr,
        Row::StepPanic,
    ] {
        common::run(2, async move {
            let seen = Arc::new(Seen::default());
            let mut chain = Chain::new(&seen);
            chain.stop_sets_marker = matches!(row, Row::OnStopped);
            let handle = spawn(chain).await;
            match row {
                Row::Err => {
                    let _ = handle.send(Msg::Fail).await;
                }
                Row::Panic => {
                    let _ = handle.send(Msg::Panic).await;
                }
                Row::StopSelf => {
                    let _ = handle.send(Msg::StopSelf).await;
                }
                Row::StepErr => {
                    let _ = handle.send(Msg::ThenFail).await;
                }
                Row::StepPanic => {
                    let _ = handle.send(Msg::ThenPanic).await;
                }
                Row::OnStopped => {
                    handle.stop(StopReason::Graceful).await.unwrap();
                }
                Row::NoticeAtTheStop => {
                    let other = spawn(Chain::new(&seen)).await;
                    handle.send(Msg::Watch(other.clone())).await.unwrap();
                    // The actor holds a turn while the watched actor ends, so
                    // the notice is still queued when its stop is read.
                    let (entered, entered_rx) = oneshot::channel();
                    let (release, release_rx) = oneshot::channel();
                    handle
                        .notify(Msg::Block(entered, release_rx))
                        .await
                        .unwrap();
                    entered_rx.await.unwrap();
                    other.stop(StopReason::Graceful).await.unwrap();
                    other.wait_stopped().await;
                    let give_up = std::time::Instant::now() + PROMPT;
                    while handle.mailbox_len() != 1 && std::time::Instant::now() < give_up {
                        tokio::task::yield_now().await;
                    }
                    assert_eq!(handle.mailbox_len(), 1, "the notice is queued");
                    handle.stop(StopReason::Graceful).await.unwrap();
                    release.send(()).unwrap();
                }
            }
            let reason = timeout(PROMPT, handle.wait_stopped())
                .await
                .unwrap_or_else(|_| panic!("{row:?}: the actor ends"));
            assert_eq!(seen.markers.load(Ordering::SeqCst), 0, "{row:?}");
            if matches!(row, Row::StepErr | Row::StepPanic) {
                assert!(
                    matches!(reason, StopReason::Failure(_)),
                    "{row:?}: a failing step ends the actor with Failure, got {reason:?}"
                );
            }
            if matches!(row, Row::NoticeAtTheStop) {
                assert_eq!(
                    *seen.log.lock().unwrap(),
                    ["down"],
                    "the notice was delivered"
                );
            }
        });
    }
}

/// A message set in a callback that does not run the chain is dropped when the
/// callback returns, not carried to a later event: the tag is dropped before
/// the next request is answered, and a delivery made while the actor stops
/// drops it before `on_stopped` begins. Rows: `on_child_stopped`, a `pre_stop`
/// that vetoes the stop, and the final delivery of a child's end that was
/// still pending when the supervisor's loop ended.
#[test]
fn a_message_set_where_the_chain_does_not_run_is_dropped_at_once() {
    #[derive(Clone, Copy, Debug)]
    enum Row {
        ChildEvent,
        Veto,
        FinalDelivery,
    }
    for row in [Row::ChildEvent, Row::Veto, Row::FinalDelivery] {
        common::run(2, async move {
            let seen = Arc::new(Seen::default());
            let dropped = Arc::new(AtomicU32::new(0));
            let mut chain = Chain::new(&seen);
            chain.tagged = Some(dropped.clone());
            chain.veto_once = matches!(row, Row::Veto);
            let handle = timeout(PROMPT, chain.spawn().supervisor())
                .await
                .expect("the spawn is acknowledged")
                .unwrap();
            let (want_calls, calls) = match row {
                Row::Veto => {
                    handle.stop(StopReason::Graceful).await.unwrap();
                    (&seen.pre_stops, 1)
                }
                Row::ChildEvent => {
                    let (tx, rx) = oneshot::channel();
                    handle.send(Msg::SpawnChild(tx)).await.unwrap();
                    rx.await.unwrap().stop(StopReason::Graceful).await.unwrap();
                    (&seen.child_events, 1)
                }
                Row::FinalDelivery => {
                    // The supervisor's turn is held while its child ends and
                    // the stop is raised, so the child's end is still pending
                    // when its loop ends.
                    let (tx, rx) = oneshot::channel();
                    handle.send(Msg::SpawnChild(tx)).await.unwrap();
                    let child = rx.await.unwrap();
                    let (entered, entered_rx) = oneshot::channel();
                    let (release, release_rx) = oneshot::channel();
                    let holder = handle.clone();
                    tokio::spawn(async move {
                        let _ = holder.send(Msg::Block(entered, release_rx)).await;
                    });
                    timeout(PROMPT, entered_rx).await.unwrap().unwrap();
                    child.stop(StopReason::Graceful).await.unwrap();
                    child.wait_stopped().await;
                    handle.stop(StopReason::Graceful).await.unwrap();
                    release.send(()).unwrap();
                    timeout(PROMPT, handle.wait_stopped()).await.unwrap();
                    assert_eq!(
                        seen.stopped_hooks.load(Ordering::SeqCst),
                        1,
                        "{row:?}: on_stopped ran"
                    );
                    assert_eq!(
                        seen.dropped_at_stop.load(Ordering::SeqCst),
                        1,
                        "{row:?}: the message was dropped before on_stopped began"
                    );
                    (&seen.child_events, 1)
                }
            };
            let give_up = std::time::Instant::now() + PROMPT;
            while want_calls.load(Ordering::SeqCst) < calls && std::time::Instant::now() < give_up {
                tokio::task::yield_now().await;
            }
            assert_eq!(want_calls.load(Ordering::SeqCst), calls, "{row:?}");
            if !matches!(row, Row::FinalDelivery) {
                // The status request is answered by the same task after the
                // callback returned.
                handle.get_status().await.unwrap();
            }
            assert_eq!(dropped.load(Ordering::SeqCst), 1, "{row:?}");
            assert_eq!(seen.markers.load(Ordering::SeqCst), 0);
            assert!(seen.log.lock().unwrap().is_empty(), "nothing was handled");
        });
    }
}

/// A start hook that continues and then stops its own actor runs no step: the
/// stop is read first.
#[test]
fn a_start_hook_that_stops_runs_no_step() {
    common::run(2, async {
        let seen = Arc::new(Seen::default());
        let mut chain = Chain::new(&seen);
        chain.start = StartHook::OnStartedThenStop;
        let handle = spawn(chain).await;
        assert_eq!(
            timeout(PROMPT, handle.wait_stopped()).await.unwrap(),
            StopReason::Graceful
        );
        assert_eq!(seen.steps.load(Ordering::SeqCst), 0);
        assert!(seen.log.lock().unwrap().is_empty());
    });
}

/// A chain of steps that never await gives the runtime's other tasks their
/// turn: on one thread a `Kill` asked for by the test task lands while the
/// chain runs, long before the chain would end by itself. Without the yield
/// the test task runs only after the chain ended on its own.
#[test]
fn a_chain_of_non_awaiting_steps_lets_a_kill_land() {
    common::run_current_thread_only(async {
        let seen = Arc::new(Seen::default());
        let handle = spawn(Chain::new(&seen)).await;
        handle.send(Msg::Spin).await.unwrap();
        handle.stop(StopReason::Kill).await.unwrap();
        assert_eq!(
            timeout(PROMPT, handle.wait_stopped()).await.unwrap(),
            StopReason::Kill
        );
        assert!(
            seen.spins.load(Ordering::SeqCst) < SPIN_LIMIT,
            "the chain ran on without a Kill landing"
        );
    });
}

/// A message taken from the priority lane may continue: the chain of steps
/// it starts runs to its end before the actor reads another branch, as one
/// started from the mailbox does.
#[test]
fn a_priority_message_may_continue() {
    common::run(2, async {
        let seen = Arc::new(Seen::default());
        let mut chain = Chain::new(&seen);
        chain.limit = 3;
        let handle = timeout(
            PROMPT,
            chain
                .spawn()
                .with_config(ActorConfig::default().with_priority_lane(8)),
        )
        .await
        .expect("the spawn is acknowledged")
        .unwrap();
        handle.send_priority(Msg::Start).await.unwrap();
        handle.send(Msg::Count).await.unwrap();
        assert_eq!(
            *seen.log.lock().unwrap(),
            ["start", "step 1", "step 2", "step 3"]
        );
    });
}
