//! The wait graph and the wait-cycle event: a send that waits is an edge from
//! the actor whose hook made the call to the actor it waits on, and the edge
//! that closes a cycle queues one event and changes no send.
//!
//! The scenarios stand in for a hook poll with the thread's caller token, so
//! the real actors here are the targets and the polls are made by hand.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::{sleep, Instant};

use super::token;
use super::wait_graph::WaitGraph;
use super::{identity_of, ActorRef, EventKind, KindSet, TelemetryEvent, TestSinkGuard};
use crate::actor::context::ActorContext;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorResult, AskError, SendError};
use crate::system::ActorSystem;
use crate::{ActorConfig, ActorHandle};

static SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// The graph alone
// ---------------------------------------------------------------------------

/// One scenario: the edges added in order, and the cycle the newest edge of
/// the last caller closes (caller first), if any.
struct GraphRow {
    name: &'static str,
    edges: &'static [(u64, u64)],
    cycle: Option<&'static [u64]>,
}

const GRAPH_ROWS: &[GraphRow] = &[
    GraphRow {
        name: "a chain closes nothing",
        edges: &[(1, 2), (2, 3)],
        cycle: None,
    },
    GraphRow {
        name: "two actors",
        edges: &[(1, 2), (2, 1)],
        cycle: Some(&[2, 1]),
    },
    GraphRow {
        name: "three actors",
        edges: &[(1, 2), (2, 3), (3, 1)],
        cycle: Some(&[3, 1, 2]),
    },
    GraphRow {
        name: "five actors",
        edges: &[(1, 2), (2, 3), (3, 4), (4, 5), (5, 1)],
        cycle: Some(&[5, 1, 2, 3, 4]),
    },
    GraphRow {
        name: "a tail leading into the cycle",
        edges: &[(9, 1), (1, 2), (2, 3), (3, 1)],
        cycle: Some(&[3, 1, 2]),
    },
    GraphRow {
        name: "a cycle the caller is not in",
        edges: &[(7, 8), (8, 7), (1, 7)],
        cycle: None,
    },
    GraphRow {
        name: "a branch that dead-ends before the way back",
        edges: &[(1, 2), (1, 3), (3, 4), (2, 5), (5, 6), (4, 1)],
        cycle: Some(&[4, 1, 3]),
    },
    GraphRow {
        name: "an actor waiting on itself",
        edges: &[(5, 5)],
        cycle: Some(&[5]),
    },
];

#[test]
fn the_search_finds_exactly_the_cycle_the_newest_edge_closes() {
    for row in GRAPH_ROWS {
        let graph = WaitGraph::default();
        for &(caller, target) in row.edges {
            graph.add(caller, target);
        }
        let last = row.edges.last().expect("a row has an edge").0;
        assert_eq!(
            graph.cycle_through(last).as_deref(),
            row.cycle,
            "{}",
            row.name
        );
    }
}

// ---------------------------------------------------------------------------
// Real targets
// ---------------------------------------------------------------------------

/// Parks every message it handles until the gate closes, then replies.
struct Node {
    gate: Arc<Semaphore>,
    /// Starts one child named `kid` when it starts.
    kid: bool,
}

impl Actor for Node {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.kid {
            let gate = self.gate.clone();
            ctx.spawn_child(move || Node {
                gate: gate.clone(),
                kid: false,
            })
            .named("kid")
            .await?;
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _ = self.gate.acquire().await;
        Ok(())
    }
}

type Call = Pin<Box<dyn Future<Output = Result<(), AskError>>>>;

fn call<A: Actor<Message = (), Response = ()>>(target: &ActorHandle<A>) -> Call {
    let target = target.clone();
    Box::pin(async move { target.send(()).await })
}

fn poll_once(call: &mut Call) -> Poll<Result<(), AskError>> {
    call.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

/// Polls once as the hook of the actor holding `token`.
fn poll_as(token: u64, call: &mut Call) -> Poll<Result<(), AskError>> {
    token::with(token, || poll_once(call))
}

struct Rig {
    system: Arc<ActorSystem>,
    sink: TestSinkGuard,
    gate: Arc<Semaphore>,
    nodes: Vec<ActorHandle<Node>>,
    tokens: Vec<u64>,
    refs: Vec<ActorRef>,
}

impl Rig {
    /// A system with the switch as given and a sink on the cycle event, then
    /// `plain` nodes (`n0`, `n1`, ...), then, when `with_kid`, a node `sup`
    /// with its child `kid` after them. The sink comes first, so every
    /// incarnation holds its token.
    async fn new(plain: usize, with_kid: bool, detect: bool) -> Rig {
        let name = format!("wait-cycle-{}", SEQ.fetch_add(1, Ordering::Relaxed));
        let system = ActorSystem::create(name).expect("a fresh system name");
        system.set_wait_cycle_detection(detect);
        let sink = system
            .telemetry_mode()
            .collect(KindSet::of(&[EventKind::WaitCycle]));
        let gate = Arc::new(Semaphore::new(0));
        let mut nodes = Vec::new();
        for index in 0..plain {
            nodes.push(spawn_node(&system, &gate, &format!("n{index}"), false, 64).await);
        }
        if with_kid {
            nodes.push(spawn_node(&system, &gate, "sup", true, 64).await);
            let deadline = Instant::now() + Duration::from_secs(10);
            let kid = loop {
                if let Some(kid) = system.get::<Node>("kid") {
                    break kid;
                }
                assert!(Instant::now() < deadline, "the child never registered");
                sleep(Duration::from_millis(5)).await;
            };
            nodes.push(kid);
        }
        let (mut tokens, mut refs) = (Vec::new(), Vec::new());
        for node in &nodes {
            let identity = identity_of(&system, node.stop_lane().abort_record())
                .expect("a spawn made while a sink takes events records an identity");
            tokens.push(identity.token());
            refs.push(identity.actor().clone());
        }
        Rig {
            system,
            sink,
            gate,
            nodes,
            tokens,
            refs,
        }
    }

    fn graph(&self) -> &WaitGraph {
        &self.system.telemetry_mode().wait
    }

    fn cycles(&self) -> Vec<Vec<ActorRef>> {
        self.sink
            .events()
            .into_iter()
            .map(|event| match event {
                TelemetryEvent::WaitCycle { members } => members,
                other => panic!("a sink on the cycle event saw {other:?}"),
            })
            .collect()
    }

    /// Waits until `count` cycle events arrived, up to ten seconds.
    async fn until_cycles(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.cycles().len() < count {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {count} cycle events; saw {:?}",
                self.cycles()
            );
            sleep(Duration::from_millis(5)).await;
        }
    }

    /// The events once they have stopped coming.
    async fn settled(&self) -> Vec<Vec<ActorRef>> {
        sleep(Duration::from_millis(150)).await;
        self.cycles()
    }
}

async fn spawn_node(
    system: &Arc<ActorSystem>,
    gate: &Arc<Semaphore>,
    name: &str,
    kid: bool,
    mailbox: usize,
) -> ActorHandle<Node> {
    Node {
        gate: gate.clone(),
        kid,
    }
    .spawn()
    .named(name)
    .on_system(system)
    .with_config(if kid {
        ActorConfig::default()
            .with_mailbox_capacity(mailbox)
            .supervisor()
    } else {
        ActorConfig::default().with_mailbox_capacity(mailbox)
    })
    .kill_grace(Duration::from_millis(50))
    .await
    .expect("the node starts")
}

/// One scenario: the waits made in order as `(caller, target)` indices into
/// the rig's nodes, and the members of the one cycle that must be reported.
struct Row {
    name: &'static str,
    plain: usize,
    with_kid: bool,
    waits: &'static [(usize, usize)],
    members: Option<&'static [usize]>,
}

const ROWS: &[Row] = &[
    Row {
        name: "two actors",
        plain: 2,
        with_kid: false,
        waits: &[(0, 1), (1, 0)],
        members: Some(&[1, 0]),
    },
    Row {
        name: "three actors",
        plain: 3,
        with_kid: false,
        waits: &[(0, 1), (1, 2), (2, 0)],
        members: Some(&[2, 0, 1]),
    },
    Row {
        name: "through a supervisor and its child",
        plain: 1,
        with_kid: true,
        waits: &[(0, 1), (1, 2), (2, 0)],
        members: Some(&[2, 0, 1]),
    },
    Row {
        name: "a chain",
        plain: 3,
        with_kid: false,
        waits: &[(0, 1), (1, 2)],
        members: None,
    },
    Row {
        name: "an actor waiting on itself",
        plain: 1,
        with_kid: false,
        waits: &[(0, 0)],
        members: Some(&[0]),
    },
];

async fn a_cycle_is_reported_once_and_no_send_fails() {
    for row in ROWS {
        let rig = Rig::new(row.plain, row.with_kid, true).await;
        let mut calls = Vec::new();
        for &(caller, target) in row.waits {
            let mut wait = call(&rig.nodes[target]);
            assert!(
                poll_as(rig.tokens[caller], &mut wait).is_pending(),
                "{}: a send to a parked actor waits",
                row.name
            );
            calls.push(wait);
        }
        assert_eq!(rig.graph().len(), row.waits.len(), "{}", row.name);

        if let Some(members) = row.members {
            rig.until_cycles(1).await;
            let expected: Vec<ActorRef> = members
                .iter()
                .map(|index| rig.refs[*index].clone())
                .collect();
            assert_eq!(rig.cycles(), vec![expected], "{}", row.name);
        }
        // The wait that closed the cycle still waits: nothing was failed.
        for wait in &mut calls {
            assert!(poll_once(wait).is_pending(), "{}: still waiting", row.name);
        }

        rig.gate.close();
        for wait in calls {
            wait.await
                .unwrap_or_else(|err| panic!("{}: a send failed: {err:?}", row.name));
        }
        assert_eq!(
            rig.graph().len(),
            0,
            "{}: an edge outlived its wait",
            row.name
        );
        let reported = rig.settled().await.len();
        assert_eq!(reported, usize::from(row.members.is_some()), "{}", row.name);
    }
}

async fn a_cancelled_wait_leaves_no_edge_and_an_ended_target_none() {
    let rig = Rig::new(2, false, true).await;

    // The first send is cancelled: the reverse send then closes no cycle.
    let mut forward = call(&rig.nodes[1]);
    assert!(poll_as(rig.tokens[0], &mut forward).is_pending());
    assert_eq!(rig.graph().len(), 1);
    drop(forward);
    assert_eq!(rig.graph().len(), 0, "a cancelled wait left its edge");
    let mut back = call(&rig.nodes[0]);
    assert!(poll_as(rig.tokens[1], &mut back).is_pending());
    assert_eq!(rig.graph().len(), 1);
    assert!(
        rig.settled().await.is_empty(),
        "the reverse send reported a cycle through a cancelled wait"
    );
    drop(back);

    // A target that ends while a send waits on it ends the wait and its edge.
    let mut waiting = call(&rig.nodes[1]);
    assert!(poll_as(rig.tokens[0], &mut waiting).is_pending());
    assert_eq!(rig.graph().len(), 1);
    rig.system.kill("n1").await.expect("n1 is registered");
    assert!(
        matches!(waiting.await, Err(AskError::ResponseDropped { .. })),
        "the wait ends with the target"
    );
    assert_eq!(rig.graph().len(), 0, "an ended target left an edge");

    // A send to an ended actor is refused at once and records nothing.
    let mut refused = call(&rig.nodes[1]);
    assert!(poll_as(rig.tokens[0], &mut refused).is_ready());
    assert_eq!(rig.graph().len(), 0);
    rig.gate.close();
}

async fn a_send_with_a_deadline_records_no_edge() {
    let rig = Rig::new(2, false, true).await;
    let caller = rig.tokens[0];
    let target = rig.nodes[1].clone();

    // Control: the same wait without a deadline is recorded.
    let mut plain = call(&target);
    assert!(poll_as(caller, &mut plain).is_pending());
    assert_eq!(rig.graph().len(), 1);
    drop(plain);

    let long = Duration::from_secs(30);
    let mut timed: Call = {
        let target = target.clone();
        Box::pin(async move { target.send_timeout((), long).await })
    };
    let mut timed_priority: Call = {
        let target = target.clone();
        Box::pin(async move { target.send_priority_timeout((), long).await })
    };
    assert!(poll_as(caller, &mut timed).is_pending());
    assert!(poll_as(caller, &mut timed_priority).is_pending());
    assert_eq!(rig.graph().len(), 0, "a timed send was recorded");
    rig.gate.close();
}

async fn nothing_is_recorded_with_the_switch_off_or_outside_a_hook() {
    // Switch off: a closing pair of waits makes no edge and no event.
    let off = Rig::new(2, false, false).await;
    let mut forward = call(&off.nodes[1]);
    let mut back = call(&off.nodes[0]);
    assert!(poll_as(off.tokens[0], &mut forward).is_pending());
    assert!(poll_as(off.tokens[1], &mut back).is_pending());
    assert_eq!(off.graph().len(), 0, "an edge with the switch off");
    assert!(off.settled().await.is_empty());
    off.gate.close();

    // Switch on, but the polls are made by no hook (token 0).
    let outside = Rig::new(2, false, true).await;
    let mut forward = call(&outside.nodes[1]);
    let mut back = call(&outside.nodes[0]);
    assert!(poll_once(&mut forward).is_pending());
    assert!(poll_once(&mut back).is_pending());
    assert_eq!(outside.graph().len(), 0, "an edge outside any hook");
    assert!(outside.settled().await.is_empty());
    outside.gate.close();
}

async fn a_send_parked_for_room_is_an_edge() {
    let rig = Rig::new(1, false, true).await;
    let full = spawn_node(&rig.system, &rig.gate, "full", false, 1).await;
    let identity =
        identity_of(&rig.system, full.stop_lane().abort_record()).expect("an identity is recorded");
    let caller = rig.tokens[0];

    // One message parks the handler and one fills the single place.
    let mut first = call(&full);
    let mut second = call(&full);
    assert!(poll_once(&mut first).is_pending());
    let deadline = Instant::now() + Duration::from_secs(10);
    while full.mailbox_available() < 1 {
        assert!(
            Instant::now() < deadline,
            "the actor never took the message"
        );
        sleep(Duration::from_millis(5)).await;
    }
    assert!(poll_once(&mut second).is_pending());
    assert_eq!(full.mailbox_available(), 0, "the place is not filled");
    assert_eq!(rig.graph().len(), 0, "a hook-less wait was recorded");

    // A notify and a send that cannot get room wait on the full actor.
    let target = full.clone();
    let mut notify: Call =
        Box::pin(async move { target.notify(()).await.map_err(|_| AskError::Closed) });
    assert!(poll_as(caller, &mut notify).is_pending());
    assert_eq!(rig.graph().len(), 1, "a notify parked for room");
    assert_eq!(rig.graph().target_of(caller), Some(identity.token()));
    let mut send = call(&full);
    assert!(poll_as(caller, &mut send).is_pending());
    assert_eq!(rig.graph().len(), 2, "a send parked for room");
    drop(notify);
    drop(send);
    assert_eq!(rig.graph().len(), 0);
    rig.gate.close();
}

async fn a_successor_does_not_inherit_the_edges_of_the_actor_it_replaced() {
    let rig = Rig::new(2, false, true).await;

    // n0 waits on n1 and has not been polled since, as when the reply sender
    // dropped at n1's end and n0's task is not yet scheduled.
    let mut waiting = call(&rig.nodes[1]);
    assert!(poll_as(rig.tokens[0], &mut waiting).is_pending());
    assert_eq!(rig.graph().target_of(rig.tokens[0]), Some(rig.tokens[1]));
    rig.system.kill("n1").await.expect("n1 is registered");

    // The replacement is built at once, in the allocations n1 just freed.
    let successor = spawn_node(&rig.system, &rig.gate, "n1b", false, 64).await;
    let token = identity_of(&rig.system, successor.stop_lane().abort_record())
        .expect("a spawn made while a sink takes events records an identity")
        .token();
    assert_ne!(token, 0);
    assert_ne!(
        token, rig.tokens[1],
        "a successor was given its predecessor's token"
    );
    assert_ne!(token, rig.tokens[0]);

    // The successor waits on n0, which waits on the actor it replaced: no
    // cycle exists, and none is reported.
    let mut back = call(&rig.nodes[0]);
    assert!(poll_as(token, &mut back).is_pending());
    assert!(
        rig.settled().await.is_empty(),
        "a cycle was reported through the dead predecessor"
    );
    drop(back);
    drop(waiting);
    assert_eq!(rig.graph().len(), 0);
    rig.gate.close();
}

async fn actors_of_two_systems_never_share_a_token() {
    let (x, y) = (
        Rig::new(1, false, true).await,
        Rig::new(1, false, true).await,
    );
    assert_ne!(x.tokens[0], 0, "the first actor of a system has no token");
    assert_ne!(y.tokens[0], 0);
    assert_ne!(x.tokens[0], y.tokens[0], "two systems gave out one token");

    // The first actor of one system waits on the first actor of the other:
    // one wait between two actors is no cycle.
    let mut across = call(&y.nodes[0]);
    assert!(poll_as(x.tokens[0], &mut across).is_pending());
    assert_eq!(y.graph().len(), 1);
    assert!(
        y.settled().await.is_empty(),
        "a wait across systems read as an actor waiting on itself"
    );
    drop(across);
    x.gate.close();
    y.gate.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actors_of_two_systems_never_share_a_token_multi_thread() {
    actors_of_two_systems_never_share_a_token().await;
}

async fn a_parked_send_makes_no_wrapper_and_no_wait_record_while_detection_is_off() {
    let off = Rig::new(2, false, false).await;
    let mut parked = call(&off.nodes[1]);
    let before = token::reads_this_thread();
    let wrapped = crate::actor::handle::edge_probe::made();
    assert!(poll_as(off.tokens[0], &mut parked).is_pending());
    assert_eq!(
        token::reads_this_thread(),
        before,
        "a send that parked with detection off recorded a wait"
    );
    assert_eq!(
        crate::actor::handle::edge_probe::made(),
        wrapped,
        "a send with detection off made a wait-edge wrapper"
    );
    drop(parked);
    off.gate.close();

    // The other sends that park for room on a full mailbox or lane take the
    // same decision: no wrapper and no wait record while detection is off,
    // even from a hook.
    let (full, full_gate) = full_step(&off.system).await;
    parked_for_room(
        &off.system,
        &full,
        off.tokens[0],
        "detection off, in a hook",
    );
    full_gate.close();

    let on = Rig::new(2, false, true).await;
    let mut parked = call(&on.nodes[1]);
    let before = token::reads_this_thread();
    let wrapped = crate::actor::handle::edge_probe::made();
    assert!(poll_as(on.tokens[0], &mut parked).is_pending());
    assert_eq!(
        token::reads_this_thread(),
        before + 1,
        "detection on reads it once"
    );
    assert!(
        crate::actor::handle::edge_probe::made() > wrapped,
        "detection on wraps the wait"
    );
    drop(parked);

    // A send from a task that is in no hook has no caller to record, so it
    // wraps nothing even with detection on.
    let mut parked = call(&on.nodes[1]);
    let wrapped = crate::actor::handle::edge_probe::made();
    assert!(poll_as(0, &mut parked).is_pending());
    assert_eq!(
        crate::actor::handle::edge_probe::made(),
        wrapped,
        "a send from outside every hook made a wait-edge wrapper"
    );
    drop(parked);
    on.gate.close();

    let (full, full_gate) = full_step(&on.system).await;
    parked_for_room(&on.system, &full, 0, "detection on, outside every hook");
    full_gate.close();
}

/// An actor parked in its handler with its mailbox and its priority lane
/// full, so a send to it waits for room.
async fn full_step(system: &Arc<ActorSystem>) -> (ActorHandle<Step>, Arc<Semaphore>) {
    let started = Arc::new(AtomicU64::new(0));
    let gate = Arc::new(Semaphore::new(0));
    let handle = Step {
        started: started.clone(),
        gate: gate.clone(),
    }
    .spawn()
    .on_system(system)
    .with_config(
        ActorConfig::default()
            .with_mailbox_capacity(1)
            .with_priority_lane(1),
    )
    .kill_grace(Duration::from_millis(50))
    .await
    .expect("the actor starts");
    handle.notify(()).await.unwrap();
    until_started(&started, 1).await;
    handle.notify(()).await.unwrap();
    handle.notify_priority(()).await.unwrap();
    assert_eq!(handle.mailbox_len(), 1, "the mailbox is full");
    assert_eq!(handle.priority_len(), 1, "the lane is full");
    (handle, gate)
}

/// Polls `notify`, `send_priority` and `notify_priority` once each as the hook
/// holding `token` (0 for a task in no hook), each against the full actor:
/// each parks, and none makes a wait-edge wrapper or a wait record.
fn parked_for_room(system: &Arc<ActorSystem>, full: &ActorHandle<Step>, token: u64, label: &str) {
    type Parked = Pin<Box<dyn Future<Output = ()>>>;
    let sends: [(&str, Parked); 3] = [
        ("notify", {
            let target = full.clone();
            Box::pin(async move {
                let _ = target.notify(()).await;
            })
        }),
        ("send_priority", {
            let target = full.clone();
            Box::pin(async move {
                let _ = target.send_priority(()).await;
            })
        }),
        ("notify_priority", {
            let target = full.clone();
            Box::pin(async move {
                let _ = target.notify_priority(()).await;
            })
        }),
    ];
    for (name, mut send) in sends {
        let wrapped = crate::actor::handle::edge_probe::made();
        let polled = token::with(token, || {
            send.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        });
        assert!(polled.is_pending(), "{label}: {name} did not park");
        assert_eq!(
            crate::actor::handle::edge_probe::made(),
            wrapped,
            "{label}: {name} made a wait-edge wrapper"
        );
        assert_eq!(
            system.telemetry_mode().wait.len(),
            0,
            "{label}: {name} recorded a wait"
        );
        drop(send);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_parked_send_makes_no_wrapper_and_no_wait_record_while_detection_is_off_multi_thread() {
    a_parked_send_makes_no_wrapper_and_no_wait_record_while_detection_is_off().await;
}

// ---------------------------------------------------------------------------
// A reply wait that begins in a hook
// ---------------------------------------------------------------------------

/// Counts the messages it starts and parks on each until it is given a
/// permit, which it keeps.
struct Step {
    started: Arc<AtomicU64>,
    gate: Arc<Semaphore>,
}

impl Actor for Step {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.started.fetch_add(1, Ordering::SeqCst);
        if let Ok(permit) = self.gate.acquire().await {
            permit.forget();
        }
        Ok(())
    }
}

struct ReplyRow {
    name: &'static str,
    /// The send is first polled inside the hook of the actor `a`.
    first_poll_in_hook: bool,
    /// Detection is on when the send is first polled.
    on_at_first_poll: bool,
    /// The cycle events the send's reply wait closes in all.
    cycles: usize,
}

const REPLY_ROWS: &[ReplyRow] = &[
    ReplyRow {
        name: "first polled in a hook with detection on",
        first_poll_in_hook: true,
        on_at_first_poll: true,
        cycles: 2,
    },
    ReplyRow {
        name: "first polled outside every hook",
        first_poll_in_hook: false,
        on_at_first_poll: true,
        cycles: 1,
    },
    ReplyRow {
        name: "first polled in a hook with detection off, then switched on",
        first_poll_in_hook: true,
        on_at_first_poll: false,
        cycles: 1,
    },
];

async fn until_started(started: &AtomicU64, want: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while started.load(Ordering::SeqCst) < want {
        assert!(
            Instant::now() < deadline,
            "the handler never started message {want}"
        );
        sleep(Duration::from_millis(5)).await;
    }
}

/// A send to a full lane waits for room, then for its reply, on the normal
/// lane and on the priority lane. The reply wait is an edge whenever it
/// begins inside a hook with detection on, whatever the reserve wait before it
/// saw.
async fn a_reply_wait_that_begins_in_a_hook_is_an_edge() {
    let mut wrong = Vec::new();
    for priority in [false, true] {
        for row in REPLY_ROWS {
            wrong.extend(reply_wait_row(priority, row).await);
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// Runs one row and returns what it saw that differs from the contract.
async fn reply_wait_row(priority: bool, row: &ReplyRow) -> Vec<String> {
    let lane = if priority { "priority" } else { "normal" };
    let name = format!("wait-reply-{}", SEQ.fetch_add(1, Ordering::Relaxed));
    let system = ActorSystem::create(name).expect("a fresh system name");
    system.set_wait_cycle_detection(row.on_at_first_poll);
    let sink = system
        .telemetry_mode()
        .collect(KindSet::of(&[EventKind::WaitCycle]));
    let spawn = |label: &'static str, mailbox: usize| {
        let started = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let step = Step {
            started: started.clone(),
            gate: gate.clone(),
        };
        let system = system.clone();
        async move {
            let handle = step
                .spawn()
                .named(label)
                .on_system(&system)
                .with_config(
                    ActorConfig::default()
                        .with_mailbox_capacity(mailbox)
                        .with_priority_lane(mailbox),
                )
                .kill_grace(Duration::from_millis(50))
                .await
                .expect("the actor starts");
            (handle, started, gate)
        }
    };
    let (a, a_started, a_gate) = spawn("a", 64).await;
    let (b, b_started, b_gate) = spawn("b", 1).await;
    let token_of = |handle: &ActorHandle<Step>| {
        identity_of(&system, handle.stop_lane().abort_record())
            .expect("a spawn made while a sink takes events records an identity")
            .token()
    };
    let (ta, tb) = (token_of(&a), token_of(&b));
    let label = format!("{lane} lane, {}", row.name);

    // a and b each park in a handler, and the lane under test of b is full.
    a.notify(()).await.unwrap();
    b.notify(()).await.unwrap();
    until_started(&a_started, 1).await;
    until_started(&b_started, 1).await;
    if priority {
        b.notify_priority(()).await.unwrap();
        assert_eq!(b.priority_len(), 1, "{label}");
    } else {
        b.notify(()).await.unwrap();
        assert_eq!(b.mailbox_available(), 0, "{label}");
    }

    // The send under test waits for room in b.
    let target = b.clone();
    let mut under_test: Call = if priority {
        Box::pin(async move { target.send_priority(()).await })
    } else {
        Box::pin(async move { target.send(()).await })
    };
    let first = if row.first_poll_in_hook {
        poll_as(ta, &mut under_test)
    } else {
        poll_once(&mut under_test)
    };
    assert!(first.is_pending(), "{label}: the reserve waits");
    if !row.on_at_first_poll {
        system.set_wait_cycle_detection(true);
    }
    // b's hook waits on a for a reply: the way back.
    let mut back = call(&a);
    assert!(poll_as(tb, &mut back).is_pending(), "{label}");

    // b takes its second message, which gives the send its room.
    b_gate.add_permits(1);
    until_started(&b_started, 2).await;

    // The reserve completes in a's hook and the reply wait begins there.
    assert!(
        poll_as(ta, &mut under_test).is_pending(),
        "{label}: the reply wait waits"
    );
    let mut wrong = Vec::new();
    let edges = system.telemetry_mode().wait.len();
    if edges != 2 {
        wrong.push(format!("{label}: {edges} edges, 2 expected"));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while edges == 2 && sink.events().len() < row.cycles {
        assert!(
            Instant::now() < deadline,
            "{label}: timed out waiting for {} cycle events; saw {}",
            row.cycles,
            sink.events().len()
        );
        sleep(Duration::from_millis(5)).await;
    }
    sleep(Duration::from_millis(150)).await;
    let cycles = sink.events().len();
    if cycles != row.cycles {
        wrong.push(format!(
            "{label}: {cycles} cycle events, {} expected",
            row.cycles
        ));
    }

    a_gate.close();
    b_gate.close();
    drop(under_test);
    drop(back);
    wrong
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_wait_that_begins_in_a_hook_is_an_edge_multi_thread() {
    a_reply_wait_that_begins_in_a_hook_is_an_edge().await;
}

// ---------------------------------------------------------------------------
// A cycle made only of notifies that wait for room on the priority lane
// ---------------------------------------------------------------------------

type Notify = Pin<Box<dyn Future<Output = Result<(), SendError>>>>;

fn poll_notify_as(token: u64, notify: &mut Notify) -> Poll<Result<(), SendError>> {
    token::with(token, || {
        notify
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
    })
}

/// Two actors park in a handler with a full priority lane, and each hook
/// notifies the other on that lane: the second wait closes a cycle, which is
/// reported once, and no notify fails.
async fn a_cycle_of_full_lane_priority_notifies_is_reported() {
    let name = format!("wait-prio-{}", SEQ.fetch_add(1, Ordering::Relaxed));
    let system = ActorSystem::create(name).expect("a fresh system name");
    system.set_wait_cycle_detection(true);
    let sink = system
        .telemetry_mode()
        .collect(KindSet::of(&[EventKind::WaitCycle]));
    let mut actors = Vec::new();
    for label in ["a", "b"] {
        let started = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let handle = Step {
            started: started.clone(),
            gate: gate.clone(),
        }
        .spawn()
        .named(label)
        .on_system(&system)
        .with_config(
            ActorConfig::default()
                .with_mailbox_capacity(1)
                .with_priority_lane(1),
        )
        .kill_grace(Duration::from_millis(50))
        .await
        .expect("the actor starts");
        // The actor parks in its handler, then its priority lane is filled.
        handle.notify(()).await.unwrap();
        until_started(&started, 1).await;
        handle.notify_priority(()).await.unwrap();
        assert_eq!(handle.priority_len(), 1, "{label}: the lane is full");
        actors.push((handle, gate));
    }
    let token_of = |handle: &ActorHandle<Step>| {
        identity_of(&system, handle.stop_lane().abort_record())
            .expect("a spawn made while a sink takes events records an identity")
            .token()
    };
    let (a, b) = (actors[0].0.clone(), actors[1].0.clone());
    let (ta, tb) = (token_of(&a), token_of(&b));

    let (to_b, to_a) = (b.clone(), a.clone());
    let mut a_waits: Notify = Box::pin(async move { to_b.notify_priority(()).await });
    let mut b_waits: Notify = Box::pin(async move { to_a.notify_priority(()).await });
    assert!(poll_notify_as(ta, &mut a_waits).is_pending());
    assert!(poll_notify_as(tb, &mut b_waits).is_pending());
    assert_eq!(system.telemetry_mode().wait.len(), 2, "one edge per wait");

    let deadline = Instant::now() + Duration::from_secs(10);
    while sink.events().is_empty() {
        assert!(Instant::now() < deadline, "no cycle event arrived");
        sleep(Duration::from_millis(5)).await;
    }
    sleep(Duration::from_millis(150)).await;
    assert_eq!(sink.events().len(), 1, "the cycle is reported once");

    // The waits end when the actors take their lanes; nothing failed.
    for (_, gate) in &actors {
        gate.close();
    }
    a_waits.await.expect("the notify to b completes");
    b_waits.await.expect("the notify to a completes");
    assert_eq!(
        system.telemetry_mode().wait.len(),
        0,
        "an edge outlived its wait"
    );
}
