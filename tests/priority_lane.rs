//! Behavioral suite for the priority lane: `MailboxConfig::with_priority_lane`
//! / `ActorConfig::with_priority_lane`, and the four handle methods
//! (`notify_priority`, `try_notify_priority`, `send_priority`,
//! `send_priority_timeout`).
//!
//! Every ordering assertion is built around a gate actor: a `Block` message
//! whose handler signals "entered" then awaits "release", so a deterministic
//! backlog can be built up while the actor is known to be parked between two
//! `handle` invocations. No sleep is ever used as synchronization: tests wait
//! on the "entered" signal, then enqueue, then release.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::Instant;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorStatusInfo, ActorSystem, AskError,
    ChildEvent, RestartType, SpawnError, StopReason, TrySendError,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "priority-lane-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Polls until a live actor is registered under `name`, or panics after
/// `timeout_ms`.
async fn live_handle<A: Actor>(sys: &ActorSystem, name: &str, timeout_ms: u64) -> ActorHandle<A> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Some(h) = sys.get::<A>(name) {
            if h.is_alive() {
                return h;
            }
        }
        if Instant::now() >= deadline {
            panic!("timed out after {timeout_ms}ms waiting for live actor '{name}'");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Polls `log` until it holds at least `n` entries, or gives up (and returns
/// whatever is there) after `timeout_ms`.
async fn wait_for_len(log: &Arc<Mutex<Vec<String>>>, n: usize, timeout_ms: u64) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        {
            let guard = log.lock().unwrap();
            if guard.len() >= n {
                return guard.clone();
            }
        }
        if Instant::now() >= deadline {
            return log.lock().unwrap().clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Opens a gate on `handle`: sends `Block`, then waits for the handler to
/// signal it has entered and parked. Returns the sender that releases it.
async fn open_gate<A>(handle: &ActorHandle<A>) -> oneshot::Sender<()>
where
    A: Actor<Message = GateMsg>,
{
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    handle
        .notify(GateMsg::Block(entered_tx, release_rx))
        .await
        .unwrap();
    entered_rx.await.unwrap();
    release_tx
}

// ---------------------------------------------------------------------------
// Gate actor
// ---------------------------------------------------------------------------

/// A message that either parks the handler at a controllable gate, records a
/// tag in receive order, or deliberately crashes the actor.
enum GateMsg {
    /// Signals `.0` as soon as the handler begins, then awaits `.1` before
    /// returning: a controllable, deterministic backlog point.
    Block(oneshot::Sender<()>, oneshot::Receiver<()>),
    /// Appends the tag to the actor's log, in receive order.
    Tag(String),
    /// Returns `Err`, crashing the actor with `StopReason::Failure`.
    Crash,
}

impl std::fmt::Debug for GateMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateMsg::Block(..) => f.write_str("Block"),
            GateMsg::Tag(tag) => f.debug_tuple("Tag").field(tag).finish(),
            GateMsg::Crash => f.write_str("Crash"),
        }
    }
}

/// Shared handler body for [`Gate`] and [`Sup`] (identical message handling;
/// only `on_child_stopped` differs between the two).
async fn handle_gate_msg(log: &Arc<Mutex<Vec<String>>>, msg: GateMsg) -> ActorResult<()> {
    match msg {
        GateMsg::Block(entered, release) => {
            let _ = entered.send(());
            let _ = release.await;
        }
        GateMsg::Tag(tag) => {
            log.lock().unwrap().push(tag);
        }
        GateMsg::Crash => {
            return Err(ActorError::user("deliberate crash"));
        }
    }
    Ok(())
}

/// A plain actor recording the receive order of every `Tag` message into a
/// shared log, blockable at will via `Block`.
#[derive(Debug)]
struct Gate {
    log: Arc<Mutex<Vec<String>>>,
}

impl Gate {
    fn new(log: Arc<Mutex<Vec<String>>>) -> Self {
        Self { log }
    }
}

impl Actor for Gate {
    type Message = GateMsg;
    type Response = ();

    async fn handle(&mut self, msg: GateMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        handle_gate_msg(&self.log, msg).await
    }
}

// ---------------------------------------------------------------------------
// Supervisors (for the child-death-ordering and restart-survival tests)
// ---------------------------------------------------------------------------

/// Supervises one `Gate` child, appending `"child_stopped"` to its own log
/// when it observes the child's death.
#[derive(Debug)]
struct Sup {
    log: Arc<Mutex<Vec<String>>>,
    child_name: String,
}

impl Actor for Sup {
    type Message = GateMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Gate::new(Arc::new(Mutex::new(Vec::new()))))
            .named(self.child_name.clone())
            .restart_type(RestartType::Temporary)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, msg: GateMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        handle_gate_msg(&self.log, msg).await
    }

    async fn on_child_stopped(
        &mut self,
        _event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.log.lock().unwrap().push("child_stopped".to_string());
        Ok(())
    }
}

/// Supervises one `Gate` child spawned with a priority lane, so a test can
/// crash and restart it and check the lane survives.
#[derive(Debug)]
struct Sup2 {
    child_name: String,
    priority_capacity: usize,
    child_log: Arc<Mutex<Vec<String>>>,
}

impl Actor for Sup2 {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let log = self.child_log.clone();
        ctx.spawn_child(move || Gate::new(log.clone()))
            .named(self.child_name.clone())
            .restart_type(RestartType::Permanent)
            .with_config(ActorConfig::default().with_priority_lane(self.priority_capacity))
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn priority_message_overtakes_a_backlog_of_normal_messages() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = Gate::new(log.clone())
        .spawn()
        .named(uname("overtakes-backlog"))
        .with_config(ActorConfig::default().with_priority_lane(4))
        .await
        .unwrap();

    let release = open_gate(&handle).await;

    handle
        .notify(GateMsg::Tag("normal-1".into()))
        .await
        .unwrap();
    handle
        .notify(GateMsg::Tag("normal-2".into()))
        .await
        .unwrap();
    handle
        .notify_priority(GateMsg::Tag("priority-1".into()))
        .await
        .unwrap();

    release.send(()).unwrap();

    let log = wait_for_len(&log, 3, 2_000).await;
    assert_eq!(log, vec!["priority-1", "normal-1", "normal-2"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn priority_lane_is_fifo_within_itself() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = Gate::new(log.clone())
        .spawn()
        .named(uname("priority-fifo"))
        .with_config(ActorConfig::default().with_priority_lane(4))
        .await
        .unwrap();

    let release = open_gate(&handle).await;

    handle
        .notify_priority(GateMsg::Tag("p1".into()))
        .await
        .unwrap();
    handle
        .notify_priority(GateMsg::Tag("p2".into()))
        .await
        .unwrap();
    handle
        .notify_priority(GateMsg::Tag("p3".into()))
        .await
        .unwrap();

    release.send(()).unwrap();

    let log = wait_for_len(&log, 3, 2_000).await;
    assert_eq!(log, vec!["p1", "p2", "p3"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_sender_priority_message_overtakes_its_own_earlier_normal_message() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = Gate::new(log.clone())
        .spawn()
        .named(uname("same-sender-overtakes"))
        .with_config(ActorConfig::default().with_priority_lane(4))
        .await
        .unwrap();

    let release = open_gate(&handle).await;

    // Same sender (this same handle), normal message enqueued first, then a
    // priority one.
    handle
        .notify(GateMsg::Tag("normal-a".into()))
        .await
        .unwrap();
    handle
        .notify_priority(GateMsg::Tag("priority-a".into()))
        .await
        .unwrap();

    release.send(()).unwrap();

    let log = wait_for_len(&log, 2, 2_000).await;
    assert_eq!(log, vec!["priority-a", "normal-a"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_priority_lane_reports_full_and_timeout() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = Gate::new(log.clone())
        .spawn()
        .named(uname("priority-full"))
        .with_config(ActorConfig::default().with_priority_lane(1))
        .await
        .unwrap();

    let release = open_gate(&handle).await;

    // Fill the one-slot priority lane. Nothing drains it while the actor is
    // parked at the gate.
    handle
        .try_notify_priority(GateMsg::Tag("fills-it".into()))
        .unwrap();

    let full = handle.try_notify_priority(GateMsg::Tag("overflow".into()));
    assert!(matches!(full, Err(TrySendError::Full)));

    let timeout = handle
        .send_priority_timeout(
            GateMsg::Tag("also-overflow".into()),
            Duration::from_millis(50),
        )
        .await;
    assert!(matches!(
        timeout,
        Err(AskError::Timeout { enqueued: false })
    ));

    release.send(()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn priority_calls_degrade_to_the_mailbox_without_a_lane() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = Gate::new(log.clone())
        .spawn()
        .named(uname("no-lane-degrade"))
        .await
        .unwrap();
    assert_eq!(handle.priority_capacity(), 0);

    let release = open_gate(&handle).await;

    handle
        .notify(GateMsg::Tag("normal-1".into()))
        .await
        .unwrap();
    handle
        .notify_priority(GateMsg::Tag("priority-1".into()))
        .await
        .unwrap();

    release.send(()).unwrap();

    let log = wait_for_len(&log, 2, 2_000).await;
    assert_eq!(
        log,
        vec!["normal-1", "priority-1"],
        "without a lane, a priority send lands in the same mailbox and keeps strict FIFO order"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_priority_capacity_is_rejected_at_spawn() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let err = Gate::new(log)
        .spawn()
        .named(uname("zero-priority-capacity"))
        .with_config(ActorConfig::default().with_priority_lane(0))
        .await
        .unwrap_err();

    assert!(matches!(err, SpawnError::ZeroMailboxCapacity));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_is_honored_with_both_lanes_full() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = Gate::new(log.clone())
        .spawn()
        .named(uname("stop-both-full"))
        .with_config(
            ActorConfig::default()
                .with_mailbox_capacity(1)
                .with_priority_lane(1),
        )
        .await
        .unwrap();

    let release = open_gate(&handle).await;

    // Fill both lanes to capacity while the actor is parked at the gate.
    handle.try_notify(GateMsg::Tag("normal-1".into())).unwrap();
    handle
        .try_notify_priority(GateMsg::Tag("priority-1".into()))
        .unwrap();

    // Stop travels on its own lane, independent of either full channel, so
    // it is not blocked by them - only by the in-flight handler itself,
    // which the Isolated Turn Principle always lets finish first.
    handle.stop(StopReason::Graceful).await.unwrap();

    release.send(()).unwrap();
    handle.wait_stopped().await;

    assert!(
        log.lock().unwrap().is_empty(),
        "a full mailbox and a full priority lane must never delay a stop request"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn child_death_is_observed_before_a_queued_priority_message() {
    let sys = ActorSystem::create(uname("child-death-order")).unwrap();
    let child_name = uname("child");
    let log = Arc::new(Mutex::new(Vec::new()));

    let sup = Sup {
        log: log.clone(),
        child_name: child_name.clone(),
    }
    .spawn()
    .named(uname("sup"))
    .on_system(&sys)
    .with_config(ActorConfig::default().with_priority_lane(4))
    .supervisor()
    .await
    .unwrap();

    let child: ActorHandle<Gate> = live_handle(&sys, &child_name, 2_000).await;

    let release = open_gate(&sup).await;

    // Crash the child and wait for its OWN terminal status - the task is
    // fully dead - independent of whether the supervisor has processed the
    // death event yet (it cannot: it is still parked at the gate).
    child.notify(GateMsg::Crash).await.unwrap();
    child.wait_stopped().await;

    // On a single-threaded runtime, tasks are polled in FIFO order: the
    // supervisor's watcher - a separate task that awaits the child's
    // `JoinHandle` and is what actually records the death in the
    // supervisor's own death plane - is woken by the child's task
    // completing, which happens before `wait_stopped` above lets this test
    // continue. That makes the watcher already runnable at this point, so
    // it is polled to completion before this test is polled again; the
    // watcher finishes in a single poll (it only awaits the already-ready
    // `JoinHandle`, then does synchronous bookkeeping), so the death is
    // recorded in the supervisor's death plane before the priority message
    // below is enqueued. The yields here are margin, not the mechanism.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    // Enqueue a priority message to the supervisor while it is still parked.
    sup.notify_priority(GateMsg::Tag("priority-msg".into()))
        .await
        .unwrap();

    // Release the gate: the supervisor's run loop now processes, in order,
    // the already-queued child-death event (combined supervision branch)
    // ahead of the priority lane.
    release.send(()).unwrap();

    let log = wait_for_len(&log, 2, 2_000).await;
    assert_eq!(log, vec!["child_stopped", "priority-msg"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_the_priority_lane_separately() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = Gate::new(log.clone())
        .spawn()
        .named(uname("status-separate"))
        .with_config(
            ActorConfig::default()
                .with_mailbox_capacity(8)
                .with_priority_lane(4),
        )
        .await
        .unwrap();

    let baseline: ActorStatusInfo = handle.get_status().await.unwrap();
    assert_eq!(baseline.mailbox_len, 0);
    assert_eq!(baseline.mailbox_capacity, 8);
    assert_eq!(baseline.priority_len, 0);
    assert_eq!(baseline.priority_capacity, 4);

    let release = open_gate(&handle).await;

    handle.notify(GateMsg::Tag("n1".into())).await.unwrap();
    handle.notify(GateMsg::Tag("n2".into())).await.unwrap();
    handle
        .notify_priority(GateMsg::Tag("p1".into()))
        .await
        .unwrap();
    handle
        .notify_priority(GateMsg::Tag("p2".into()))
        .await
        .unwrap();
    handle
        .notify_priority(GateMsg::Tag("p3".into()))
        .await
        .unwrap();

    // `get_status` cannot answer while the actor is parked mid-handler (the
    // Isolated Turn Principle: the system channel is only serviced between
    // turns), so the backlog is observed through the handle's own instant,
    // capacity-based counters instead - the same ones `get_status` itself
    // reads once the actor is free to answer.
    assert_eq!(
        handle.mailbox_len(),
        2,
        "priority traffic must not change the mailbox counters"
    );
    assert_eq!(handle.mailbox_capacity(), 8);
    assert_eq!(handle.priority_len(), 3);
    assert_eq!(handle.priority_capacity(), 4);

    release.send(()).unwrap();
    let _ = wait_for_len(&log, 5, 2_000).await;

    let after: ActorStatusInfo = handle.get_status().await.unwrap();
    assert_eq!(after.mailbox_len, 0);
    assert_eq!(after.priority_len, 0);
    assert_eq!(after.mailbox_capacity, 8);
    assert_eq!(after.priority_capacity, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn priority_lane_survives_a_supervised_restart() {
    let sys = ActorSystem::create(uname("priority-survives-restart")).unwrap();
    let child_name = uname("worker");
    let child_log = Arc::new(Mutex::new(Vec::new()));

    let _sup = Sup2 {
        child_name: child_name.clone(),
        priority_capacity: 4,
        child_log: child_log.clone(),
    }
    .spawn()
    .named(uname("sup2"))
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();

    let original: ActorHandle<Gate> = live_handle(&sys, &child_name, 2_000).await;
    assert_eq!(original.priority_capacity(), 4);

    original.notify(GateMsg::Crash).await.unwrap();
    original.wait_stopped().await;

    let restarted: ActorHandle<Gate> = live_handle(&sys, &child_name, 2_000).await;
    assert!(restarted.is_alive());
    assert_eq!(
        restarted.priority_capacity(),
        4,
        "the priority lane must be rebuilt for the fresh incarnation from the same stored ActorConfig"
    );

    // Prove the lane is not just reported but actually usable end to end.
    restarted
        .notify_priority(GateMsg::Tag("after-restart".into()))
        .await
        .unwrap();
    let log = wait_for_len(&child_log, 1, 2_000).await;
    assert_eq!(log, vec!["after-restart"]);
}

// ---------------------------------------------------------------------------
// A priority flood starves the normal mailbox without losing a message
// ---------------------------------------------------------------------------

/// Messages of the flood test.
enum FloodMsg {
    /// Signals `.0` as soon as the handler begins, then awaits `.1`.
    Block(oneshot::Sender<()>, oneshot::Receiver<()>),
    /// A message on the normal mailbox.
    Normal,
    /// The `i`-th priority message of the flood.
    Priority(usize),
}

/// Handles every priority message after a fixed delay and, before it returns,
/// sends the next priority message to itself: the lane holds a message every
/// time the run loop looks at it, until the flood has been sent in full.
struct Flooder {
    received: Arc<Mutex<Vec<(String, Instant)>>>,
    flood: usize,
    delay: Duration,
}

impl Actor for Flooder {
    type Message = FloodMsg;
    type Response = ();

    async fn handle(&mut self, msg: FloodMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            FloodMsg::Block(entered, release) => {
                let _ = entered.send(());
                let _ = release.await;
            }
            FloodMsg::Normal => {
                self.received
                    .lock()
                    .unwrap()
                    .push(("normal".to_string(), Instant::now()));
            }
            FloodMsg::Priority(i) => {
                tokio::time::sleep(self.delay).await;
                self.received
                    .lock()
                    .unwrap()
                    .push((format!("priority-{i}"), Instant::now()));
                if i + 1 < self.flood {
                    ctx.self_handle()
                        .try_notify_priority(FloodMsg::Priority(i + 1))
                        .expect("the handler has just freed a slot in the lane");
                }
            }
        }
        Ok(())
    }
}

/// A normal message queued before a priority flood is received only after
/// the whole flood, and every message of both lanes is received exactly once.
#[test]
fn a_priority_flood_delays_the_normal_mailbox_and_loses_no_message() {
    common::run(0, async {
        const FLOOD: usize = 25;
        const DELAY: Duration = Duration::from_millis(4);
        let drain_bound = DELAY * FLOOD as u32 + Duration::from_secs(10);

        let sys = ActorSystem::create(uname("flood")).unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let handle = Flooder {
            received: received.clone(),
            flood: FLOOD,
            delay: DELAY,
        }
        .spawn()
        .named(uname("flooder"))
        .on_system(&sys)
        .with_config(ActorConfig::default().with_priority_lane(4))
        .await
        .unwrap();

        // Park the handler so both messages are queued before either runs.
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        handle
            .notify(FloodMsg::Block(entered_tx, release_rx))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), entered_rx)
            .await
            .expect("the gate must be entered")
            .unwrap();
        handle.notify(FloodMsg::Normal).await.unwrap();
        handle.notify_priority(FloodMsg::Priority(0)).await.unwrap();

        let released_at = Instant::now();
        release_tx.send(()).unwrap();

        let log = tokio::time::timeout(drain_bound, async {
            loop {
                {
                    let guard = received.lock().unwrap();
                    if guard.len() == FLOOD + 1 {
                        return guard.clone();
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the flood and the normal message must both be received");

        let order: Vec<&str> = log.iter().map(|(tag, _)| tag.as_str()).collect();
        let mut expected: Vec<String> = (0..FLOOD).map(|i| format!("priority-{i}")).collect();
        expected.push("normal".to_string());
        assert_eq!(
            order, expected,
            "the normal message is received after the whole flood, each message exactly once"
        );
        let (_, normal_at) = log.last().unwrap();
        assert!(
            normal_at.duration_since(released_at) >= DELAY * FLOOD as u32,
            "the normal message waited out every priority handler's delay"
        );
    });
}
