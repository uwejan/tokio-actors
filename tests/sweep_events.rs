//! The events the sampling task of a system raises: `TurnStuck` for a turn
//! past the smallest threshold in effect and `MailboxBacklog` for a mailbox
//! crossing the monitor's lengths. They are raised without the `tracing`
//! feature and are told to a handler and to the monitor alike.
//!
//! The sampling takes a look every 50 ms or more, so a test that waits for
//! something not to happen waits for several looks; every wait for something
//! to happen ends when it does.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, timeout, Instant};

use tokio_actors::telemetry::{
    Delivery, EventKind, KindSet, LongQueue, MonitorConfig, TelemetryEvent,
};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorSystem, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(20);
/// Several looks of the sampling task, which looks at least every 50 ms when
/// a threshold is small.
const SEVERAL_LOOKS: Duration = Duration::from_millis(500);

fn uname(base: &str) -> String {
    format!("sweep-events-{base}-{}", uuid::Uuid::new_v4())
}

type Told = Arc<TelemetryEvent<'static>>;

/// Forwards what the system tells it to the test.
struct Mon {
    tx: mpsc::UnboundedSender<Told>,
}

impl Actor for Mon {
    type Message = Delivery;
    type Response = ();

    async fn handle(&mut self, msg: Delivery, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _ = self.tx.send(msg.event);
        Ok(())
    }
}

/// What a monitor whose turns park is sent: a report, or a message that
/// fills its mailbox.
enum Parked {
    Told(Delivery),
    Fill,
}

/// A monitor whose every turn parks until the test adds a permit, so its
/// mailbox can be held full.
struct ParkedMon {
    tx: mpsc::UnboundedSender<Told>,
    taken: Arc<AtomicUsize>,
    gate: Arc<Semaphore>,
}

impl Actor for ParkedMon {
    type Message = Parked;
    type Response = ();

    async fn handle(&mut self, msg: Parked, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.taken.fetch_add(1, Ordering::SeqCst);
        if let Parked::Told(delivery) = msg {
            let _ = self.tx.send(delivery.event);
        }
        self.gate.acquire().await.unwrap().forget();
        Ok(())
    }
}

/// Parks inside every message until the test adds a permit.
struct Gated {
    taken: Arc<AtomicUsize>,
    gate: Arc<Semaphore>,
}

impl Actor for Gated {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.taken.fetch_add(1, Ordering::SeqCst);
        self.gate.acquire().await.unwrap().forget();
        Ok(())
    }
}

async fn spawn_gated(
    sys: &Arc<ActorSystem>,
    gate: &Arc<Semaphore>,
) -> (ActorHandle<Gated>, Arc<AtomicUsize>) {
    let taken = Arc::new(AtomicUsize::new(0));
    let handle = Gated {
        taken: taken.clone(),
        gate: gate.clone(),
    }
    .spawn()
    .on_system(sys)
    .await
    .unwrap();
    (handle, taken)
}

async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + BOUND;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(2)).await;
    }
}

/// Returns once the monitor's first look is over. A mailbox found long at
/// that look stands before the monitor and is not told to it, so a test that
/// waits for a mailbox it fills to be told fills it after this. A probe's
/// parked turn is reported by a look; a second parked turn, begun after that
/// report arrived, is reported by a later look, and no look from that one on
/// is this monitor's first.
async fn past_the_first_look(sys: &Arc<ActorSystem>, told: &mut mpsc::UnboundedReceiver<Told>) {
    let gate = Arc::new(Semaphore::new(0));
    let (probe, taken) = spawn_gated(sys, &gate).await;
    for turn in 1..=2 {
        probe.notify(()).await.unwrap();
        until("the probe's turn to park", || {
            taken.load(Ordering::SeqCst) == turn
        })
        .await;
        told_a_stuck_turn(told).await;
        gate.add_permits(1);
    }
}

/// Waits until the monitor is told a stuck turn.
async fn told_a_stuck_turn(rx: &mut mpsc::UnboundedReceiver<Told>) {
    loop {
        let event = timeout(BOUND, rx.recv())
            .await
            .expect("a stuck turn is reported")
            .expect("the monitor is open");
        if event.kind() == Some(EventKind::TurnStuck) {
            return;
        }
    }
}

/// The next `MailboxBacklog` the monitor was told: its actor's incarnation,
/// whether it entered, its length and its capacity.
async fn next_backlog(rx: &mut mpsc::UnboundedReceiver<Told>) -> (u64, bool, usize, usize) {
    loop {
        let event = timeout(BOUND, rx.recv())
            .await
            .expect("a report arrives")
            .expect("the monitor is open");
        if let TelemetryEvent::MailboxBacklog {
            actor,
            entered,
            len,
            capacity,
        } = &*event
        {
            return (actor.incarnation(), *entered, *len, *capacity);
        }
    }
}

/// The length of the next `MailboxBacklog` a handler saw for `who` with
/// `entered`; the handler sees every edge a look finds, so its report is the
/// handshake that a look found that edge.
async fn saw(
    seen: &mut mpsc::UnboundedReceiver<(tokio_actors::ActorId, bool, usize)>,
    who: &tokio_actors::ActorId,
    entered: bool,
) -> usize {
    loop {
        let (id, edge, len) = timeout(BOUND, seen.recv())
            .await
            .expect("an edge is seen")
            .expect("the handler is attached");
        if &id == who && edge == entered {
            return len;
        }
    }
}

/// The next `MailboxBacklog` in `rx`: the actor it names, whether it entered,
/// and its length.
async fn next_backlog_named(
    rx: &mut mpsc::UnboundedReceiver<Told>,
) -> (tokio_actors::ActorId, bool, usize) {
    loop {
        let event = timeout(BOUND, rx.recv())
            .await
            .expect("a report arrives")
            .expect("the monitor is open");
        if let TelemetryEvent::MailboxBacklog {
            actor,
            entered,
            len,
            ..
        } = &*event
        {
            return (actor.id().clone(), *entered, *len);
        }
    }
}

/// Nothing of kind `kind` arrives within several looks.
async fn quiet(rx: &mut mpsc::UnboundedReceiver<Told>, kind: EventKind) {
    let end = Instant::now() + SEVERAL_LOOKS;
    while let Ok(Some(event)) =
        timeout(end.saturating_duration_since(Instant::now()), rx.recv()).await
    {
        assert_ne!(event.kind(), Some(kind), "unexpected {event:?}");
    }
}

/// A turn is reported once, at the smallest threshold in effect (the monitor's,
/// under the user's larger one), to a handler and to the monitor; the next turn
/// of the same actor is reported again. A report the monitor's full mailbox
/// refuses is offered again while the turn lasts, and once the mailbox has
/// room the monitor is told the turn once, then the count of the refusals.
#[test]
fn a_stuck_turn_is_reported_once_at_the_smallest_threshold() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("stuck")).unwrap();
        sys.set_turn_warning_threshold(Some(Duration::from_secs(30)));
        let (tx, mut handled) = mpsc::unbounded_channel();
        assert!(sys.attach_telemetry(
            "stuck",
            KindSet::of(&[EventKind::TurnStuck]),
            move |event| {
                if let TelemetryEvent::TurnStuck {
                    actor,
                    elapsed,
                    threshold,
                } = event
                {
                    let _ = tx.send((actor.id().clone(), *elapsed, *threshold));
                }
            }
        ));
        let (mon_tx, mut told) = mpsc::unbounded_channel();
        let mon = Mon { tx: mon_tx }.spawn().on_system(&sys).await.unwrap();
        let config = MonitorConfig {
            long_schedule: Some(Duration::from_millis(100)),
            long_message_queue: None,
        };
        assert_eq!(sys.system_monitor(&mon, |d| d, config), Ok(None));

        let gate = Arc::new(Semaphore::new(0));
        let (held, taken) = spawn_gated(&sys, &gate).await;
        for turn in 1..=2usize {
            held.notify(()).await.unwrap();
            until("the turn to begin", || taken.load(Ordering::SeqCst) == turn).await;

            let (id, elapsed, threshold) = timeout(BOUND, handled.recv()).await.unwrap().unwrap();
            assert_eq!(&id, held.id());
            assert_eq!(threshold, Duration::from_millis(100));
            assert!(elapsed >= threshold, "{elapsed:?} against {threshold:?}");
            let event = timeout(BOUND, told.recv()).await.unwrap().unwrap();
            assert_eq!(event.kind(), Some(EventKind::TurnStuck));

            // Several more looks find the same turn and say nothing.
            let end = Instant::now() + SEVERAL_LOOKS;
            assert!(
                timeout(
                    end.saturating_duration_since(Instant::now()),
                    handled.recv()
                )
                .await
                .is_err(),
                "turn {turn} was reported twice"
            );
            gate.add_permits(1);
        }

        // The monitor's mailbox (one message) is full while its turn parks.
        let (parked_tx, mut parked_told) = mpsc::unbounded_channel();
        let parked_taken = Arc::new(AtomicUsize::new(0));
        let parked_gate = Arc::new(Semaphore::new(0));
        let parked = ParkedMon {
            tx: parked_tx,
            taken: parked_taken.clone(),
            gate: parked_gate.clone(),
        }
        .spawn()
        .with_config(tokio_actors::ActorConfig::default().with_mailbox_capacity(1))
        .on_system(&sys)
        .await
        .unwrap();
        let mapped = Arc::new(AtomicUsize::new(0));
        let counted = mapped.clone();
        let map = move |delivery: Delivery| {
            if delivery.event.kind() == Some(EventKind::TurnStuck) {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            Parked::Told(delivery)
        };
        let config = MonitorConfig {
            long_schedule: Some(Duration::from_millis(100)),
            long_message_queue: None,
        };
        assert!(sys.system_monitor(&parked, map, config).unwrap().is_some());
        parked.notify(Parked::Fill).await.unwrap();
        until("the monitor's turn to park", || {
            parked_taken.load(Ordering::SeqCst) == 1
        })
        .await;
        parked.notify(Parked::Fill).await.unwrap();

        held.notify(()).await.unwrap();
        until("the third turn to begin", || {
            taken.load(Ordering::SeqCst) == 3
        })
        .await;
        // The handler is told of the monitor's own parked turn as well.
        loop {
            let (id, _, _) = timeout(BOUND, handled.recv()).await.unwrap().unwrap();
            if &id == held.id() {
                break;
            }
            assert_eq!(&id, parked.id());
        }
        // A second map of the same turn's report is its offer after a
        // refusal: a report the monitor took is never offered again.
        until("the refused report to be offered again", || {
            mapped.load(Ordering::SeqCst) >= 2
        })
        .await;
        parked_gate.add_permits(1000);
        let mut stuck = 0;
        let mut missed = 0;
        while missed == 0 {
            let event = timeout(BOUND, parked_told.recv()).await.unwrap().unwrap();
            match &*event {
                TelemetryEvent::TurnStuck { actor, .. } => {
                    assert_eq!(actor.id(), held.id());
                    stuck += 1;
                }
                TelemetryEvent::Notice(tokio_actors::telemetry::SubscriptionNotice::Overflow {
                    missed: count,
                }) => missed = *count,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(stuck, 1, "the turn is told once");
        assert!(missed >= 2, "each refusal is counted: {missed}");
        quiet(&mut parked_told, EventKind::TurnStuck).await;
        gate.add_permits(1);
    });
}

/// A mailbox is reported when it reaches the length, once, and again when it
/// falls to the lower one; nothing is said between the two. An actor that
/// ends while its mailbox is reported long is reported short once. A monitor
/// is owed nothing for a mailbox already long when it is installed (with no
/// monitor before it, or replacing one), neither the long report nor the
/// short one, and is told that mailbox's next crossing; a monitor is never
/// told about its own mailbox.
#[test]
fn a_mailbox_is_reported_at_each_edge_once_and_when_its_actor_ends() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("edges")).unwrap();
        let (mon_tx, mut told) = mpsc::unbounded_channel();
        let mon = Mon { tx: mon_tx }.spawn().on_system(&sys).await.unwrap();
        // The short `long_schedule` makes the looks frequent; the only ones
        // of its reports read here are the probe's.
        let config = MonitorConfig {
            long_schedule: Some(Duration::from_millis(40)),
            long_message_queue: Some(LongQueue {
                disable: 1,
                enable: 3,
            }),
        };
        assert_eq!(sys.system_monitor(&mon, |d| d, config), Ok(None));
        past_the_first_look(&sys, &mut told).await;

        let gate = Arc::new(Semaphore::new(0));
        let (x, taken) = spawn_gated(&sys, &gate).await;
        // One message is inside its turn, three wait.
        x.notify(()).await.unwrap();
        until("the first message to be taken", || {
            taken.load(Ordering::SeqCst) == 1
        })
        .await;
        for _ in 0..3 {
            x.notify(()).await.unwrap();
        }
        let capacity = x.mailbox_capacity();
        let (incarnation, entered, len, cap) = next_backlog(&mut told).await;
        assert_eq!((entered, len, cap), (true, 3, capacity));

        // Two wait: between the lengths, nothing is said.
        gate.add_permits(1);
        until("the length to fall to two", || x.mailbox_len() == 2).await;
        quiet(&mut told, EventKind::MailboxBacklog).await;

        // One waits: it left.
        gate.add_permits(1);
        let (again, entered, len, _) = next_backlog(&mut told).await;
        assert_eq!((again, entered, len), (incarnation, false, 1));
        quiet(&mut told, EventKind::MailboxBacklog).await;

        // It grows again and is reported again; then its actor ends.
        for _ in 0..2 {
            x.notify(()).await.unwrap();
        }
        let (_, entered, len, _) = next_backlog(&mut told).await;
        assert_eq!((entered, len), (true, 3));
        x.stop(StopReason::Kill).await.unwrap();
        // A look that runs while the ended actor's mail is dropped reads
        // what is left of it, one message or none.
        let (gone, entered, len, cap) = next_backlog(&mut told).await;
        assert_eq!((gone, entered, cap), (incarnation, false, capacity));
        assert!(len <= 1, "the ended actor's mailbox is reported at {len}");
        quiet(&mut told, EventKind::MailboxBacklog).await;

        let edges = || MonitorConfig {
            long_schedule: Some(Duration::from_millis(40)),
            long_message_queue: Some(LongQueue {
                disable: 1,
                enable: 3,
            }),
        };
        let (seen_tx, mut seen) = mpsc::unbounded_channel();
        assert!(sys.attach_telemetry(
            "edges",
            KindSet::of(&[EventKind::MailboxBacklog]),
            move |event| {
                if let TelemetryEvent::MailboxBacklog {
                    actor,
                    entered,
                    len,
                    ..
                } = event
                {
                    let _ = seen_tx.send((actor.id().clone(), *entered, *len));
                }
            }
        ));
        // Each step's outcome is kept, so one run shows every step that
        // does not hold.
        let mut wrong: Vec<String> = Vec::new();

        // No monitor while y grows to four waiting; a monitor installed then
        // finds it long at its first look.
        assert!(sys.clear_system_monitor().is_some());
        let y_gate = Arc::new(Semaphore::new(0));
        let (y, y_taken) = spawn_gated(&sys, &y_gate).await;
        y.notify(()).await.unwrap();
        until("y's first message to be taken", || {
            y_taken.load(Ordering::SeqCst) == 1
        })
        .await;
        for _ in 0..4 {
            y.notify(()).await.unwrap();
        }
        let (tx2, mut told2) = mpsc::unbounded_channel();
        let mon2 = Mon { tx: tx2 }.spawn().on_system(&sys).await.unwrap();
        assert_eq!(sys.system_monitor(&mon2, |d| d, edges()), Ok(None));
        assert_eq!(saw(&mut seen, y.id(), true).await, 4);
        y_gate.add_permits(3);
        until("y to fall to one waiting", || y.mailbox_len() == 1).await;
        saw(&mut seen, y.id(), false).await;
        for _ in 0..2 {
            y.notify(()).await.unwrap();
        }
        assert_eq!(saw(&mut seen, y.id(), true).await, 3);
        let first = next_backlog_named(&mut told2).await;
        if first != (y.id().clone(), true, 3) {
            wrong.push(format!(
                "none before: the first report is {first:?}, not y's next crossing"
            ));
        }

        // y is long (three waiting) and told to mon2; mon3 replaces mon2.
        let (tx3, mut told3) = mpsc::unbounded_channel();
        let mon3 = Mon { tx: tx3 }.spawn().on_system(&sys).await.unwrap();
        assert!(sys.system_monitor(&mon3, |d| d, edges()).unwrap().is_some());
        y_gate.add_permits(2);
        until("y to fall to one waiting", || y.mailbox_len() == 1).await;
        saw(&mut seen, y.id(), false).await;
        // The look that saw the fall is the new monitor's first or a later
        // one, and the monitor was told neither the standing mailbox nor its
        // fall.
        quiet(&mut told3, EventKind::MailboxBacklog).await;
        for _ in 0..4 {
            y.notify(()).await.unwrap();
        }
        // The mailbox is long from the second of these sends on, and a look
        // between two of them reports the length it finds.
        let crossed = saw(&mut seen, y.id(), true).await;
        assert!((3..=5).contains(&crossed), "y crossed at {crossed}");
        let first = next_backlog_named(&mut told3).await;
        if first != (y.id().clone(), true, crossed) {
            wrong.push(format!(
                "replacing a monitor: the first report is {first:?}, not y's next crossing"
            ));
        }

        // A monitor whose own mailbox goes long and falls short is told
        // nothing about itself: its first report is y's next crossing.
        let (ptx, mut ptold) = mpsc::unbounded_channel();
        let p_taken = Arc::new(AtomicUsize::new(0));
        let p_gate = Arc::new(Semaphore::new(0));
        let parked = ParkedMon {
            tx: ptx,
            taken: p_taken.clone(),
            gate: p_gate.clone(),
        }
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();
        assert!(sys
            .system_monitor(&parked, Parked::Told, edges())
            .unwrap()
            .is_some());
        parked.notify(Parked::Fill).await.unwrap();
        until("the monitor's turn to park", || {
            p_taken.load(Ordering::SeqCst) == 1
        })
        .await;
        for _ in 0..3 {
            parked.notify(Parked::Fill).await.unwrap();
        }
        saw(&mut seen, parked.id(), true).await;
        p_gate.add_permits(1000);
        saw(&mut seen, parked.id(), false).await;
        y_gate.add_permits(4);
        until("y to fall to one waiting", || y.mailbox_len() == 1).await;
        saw(&mut seen, y.id(), false).await;
        for _ in 0..2 {
            y.notify(()).await.unwrap();
        }
        assert_eq!(saw(&mut seen, y.id(), true).await, 3);
        let first = next_backlog_named(&mut ptold).await;
        if first != (y.id().clone(), true, 3) {
            wrong.push(format!(
                "own mailbox: the first report is {first:?}, not y's next crossing"
            ));
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
        y_gate.add_permits(1000);
    });
}
