//! Event subscriptions: what a watcher receives, what the emitter never does,
//! and how a subscription ends with the incarnation that made it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::actor_ref::ActorRef;
use super::event::{EventKind, HelperEnd, HelperKind, KindSet, TelemetryEvent};
use super::mode::Mode;
use super::subscription::{
    self, Delivery, Filter, QueueItem, Scope, SubEntry, SubQueue, SubscriptionId,
    SubscriptionNotice, MAX_SUBSCRIPTION_QUEUE,
};
use crate::actor::context::ActorContext;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult};
use crate::system::ActorSystem;
use crate::types::{ActorId, StopReason};
use crate::{ActorConfig, ActorHandle};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type Got = Arc<Mutex<Vec<Delivery>>>;

fn uname(base: &str) -> String {
    format!("{base}-{}", uuid::Uuid::new_v4().simple())
}

fn new_system() -> Arc<ActorSystem> {
    ActorSystem::create(uname("sub-sys")).expect("system name must be unique")
}

fn aref(name: &str, incarnation: u64) -> ActorRef {
    ActorRef::new_root(ActorId::from(name), incarnation)
}

/// The reference of the last name, with the earlier names as its ancestors,
/// root first.
fn chain(names: &[&str]) -> ActorRef {
    let mut current = aref(names[0], 1);
    for name in &names[1..] {
        current = ActorRef::new_child(ActorId::from(*name), 1, &current);
    }
    current
}

fn ended(actor: ActorRef) -> TelemetryEvent<'static> {
    TelemetryEvent::ActorEnded {
        actor,
        reason: StopReason::Graceful,
        lifetime: None,
    }
}

fn helper_ended() -> TelemetryEvent<'static> {
    TelemetryEvent::HelperEnded {
        kind: HelperKind::Reaper,
        owner: None,
        end: HelperEnd::Finished,
    }
}

fn emit(mode: &Mode, event: TelemetryEvent<'static>) {
    let kind = event.kind().expect("an event, not a notice");
    mode.dispatch(kind, || event);
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The reference of an actor's incarnation, as its recorded identity carries
/// it; an actor no sink was armed for reads as its id with the unknown
/// incarnation until a subscription it makes (or an event) identifies it.
fn own_ref(sys: &Arc<ActorSystem>, handle: &ActorHandle<Watcher>) -> ActorRef {
    super::identity_of(sys, handle.stop_lane().abort_record())
        .map(|identity| identity.actor().clone())
        .unwrap_or_else(|| {
            ActorRef::new_root(handle.id().clone(), crate::telemetry::UNKNOWN_INCARNATION)
        })
}

/// The events a watcher received, counting each drop that a `missed_before`
/// or an overflow notice reports as one event.
fn accounted(got: &Got) -> u64 {
    got.lock()
        .unwrap()
        .iter()
        .map(|d| {
            d.missed_before
                + match &*d.event {
                    TelemetryEvent::Notice(SubscriptionNotice::Overflow { missed }) => *missed,
                    TelemetryEvent::Notice(SubscriptionNotice::MapPanicked) => 0,
                    _ => 1,
                }
        })
        .sum()
}

fn notices(got: &Got) -> Vec<SubscriptionNotice> {
    got.lock()
        .unwrap()
        .iter()
        .filter_map(|d| match &*d.event {
            TelemetryEvent::Notice(n) => Some(*n),
            _ => None,
        })
        .collect()
}

fn plain_events(got: &Got) -> usize {
    got.lock()
        .unwrap()
        .iter()
        .filter(|d| !matches!(&*d.event, TelemetryEvent::Notice(_)))
        .count()
}

// ---------------------------------------------------------------------------
// The watcher actor
// ---------------------------------------------------------------------------

/// How the watcher's map behaves.
enum MapMode {
    /// Hands every delivery to the mailbox.
    Plain,
    /// Panics on every call but the one for the panic notice.
    PanicOnEvents,
    /// Panics on every call, the notice included.
    PanicAlways,
    /// Blocks its thread for a while on the first call.
    SlowFirst {
        started: Arc<AtomicBool>,
        done: Arc<AtomicBool>,
    },
    /// Counts the calls made on the given thread.
    OnThread {
        thread: Arc<Mutex<Option<ThreadId>>>,
        calls: Arc<AtomicUsize>,
    },
}

enum Msg {
    Subscribe(Filter, MapMode),
    Unsubscribe(u64),
    Event(Delivery),
    Filler,
    Stall {
        started: Arc<Notify>,
        release: Arc<Notify>,
    },
    Crash,
}

struct Watcher {
    got: Got,
    /// Subscribed in `on_started` when set.
    auto: Option<Filter>,
}

impl Watcher {
    fn new(got: &Got) -> Self {
        Self {
            got: got.clone(),
            auto: None,
        }
    }
}

impl Actor for Watcher {
    type Message = Msg;
    type Response = u64;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(filter) = self.auto.clone() {
            ctx.subscribe(filter, Msg::Event);
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
        match msg {
            Msg::Subscribe(filter, mode) => {
                let map: Box<dyn FnMut(Delivery) -> Msg + Send> = match mode {
                    MapMode::Plain => Box::new(Msg::Event),
                    MapMode::PanicOnEvents => Box::new(|d: Delivery| {
                        if matches!(
                            &*d.event,
                            TelemetryEvent::Notice(SubscriptionNotice::MapPanicked)
                        ) {
                            Msg::Event(d)
                        } else {
                            panic!("the map panicked")
                        }
                    }),
                    MapMode::PanicAlways => {
                        Box::new(|_: Delivery| -> Msg { panic!("the map panicked") })
                    }
                    MapMode::SlowFirst { started, done } => {
                        let mut first = true;
                        Box::new(move |d: Delivery| {
                            if first {
                                first = false;
                                started.store(true, Ordering::SeqCst);
                                std::thread::sleep(Duration::from_millis(300));
                                done.store(true, Ordering::SeqCst);
                            }
                            Msg::Event(d)
                        })
                    }
                    MapMode::OnThread { thread, calls } => Box::new(move |d: Delivery| {
                        if *thread.lock().unwrap() == Some(std::thread::current().id()) {
                            calls.fetch_add(1, Ordering::SeqCst);
                        }
                        Msg::Event(d)
                    }),
                };
                Ok(ctx.subscribe(filter, map).0)
            }
            Msg::Unsubscribe(id) => Ok(u64::from(ctx.unsubscribe(SubscriptionId(id)))),
            Msg::Event(delivery) => {
                self.got.lock().unwrap().push(delivery);
                Ok(0)
            }
            Msg::Filler => Ok(0),
            Msg::Stall { started, release } => {
                started.notify_one();
                release.notified().await;
                Ok(0)
            }
            Msg::Crash => Err(ActorError::user("crash")),
        }
    }
}

async fn spawn_watcher(sys: &Arc<ActorSystem>, got: &Got, mailbox: usize) -> ActorHandle<Watcher> {
    Watcher::new(got)
        .spawn()
        .named(uname("watcher"))
        .on_system(sys)
        .with_config(ActorConfig::default().with_mailbox_capacity(mailbox))
        .kill_grace(Duration::from_millis(50))
        .await
        .expect("the watcher starts")
}

async fn subscribe(h: &ActorHandle<Watcher>, filter: Filter) -> u64 {
    h.send(Msg::Subscribe(filter, MapMode::Plain))
        .await
        .expect("subscribed")
}

/// Stalls the watcher's handler and fills its mailbox, so a forwarder that
/// maps a message waits for room. Notify the returned handle to let go.
async fn jam(h: &ActorHandle<Watcher>, mailbox: usize) -> Arc<Notify> {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    h.notify(Msg::Stall {
        started: started.clone(),
        release: release.clone(),
    })
    .await
    .unwrap();
    started.notified().await;
    for _ in 0..mailbox {
        h.try_notify(Msg::Filler).expect("room for a filler");
    }
    release
}

// ---------------------------------------------------------------------------
// What a watcher receives
// ---------------------------------------------------------------------------

/// A row of the matching table.
struct MatchRow {
    name: &'static str,
    scope: Scope,
    kinds: KindSet,
    event: TelemetryEvent<'static>,
    delivered: bool,
}

/// The scope, kind and self-exclusion rules, on the event's own ancestry.
#[test]
fn matching_rows() {
    let all = KindSet::all();
    let id = ActorId::from;
    let rows = vec![
        MatchRow {
            name: "system scope takes any subject",
            scope: Scope::System,
            kinds: all,
            event: ended(chain(&["r", "p", "g"])),
            delivered: true,
        },
        MatchRow {
            name: "the watcher's own event is skipped",
            scope: Scope::System,
            kinds: all,
            event: ended(aref("w", 1)),
            delivered: false,
        },
        MatchRow {
            name: "another incarnation of the watcher's id is not the watcher",
            scope: Scope::System,
            kinds: all,
            event: ended(aref("w", 2)),
            delivered: true,
        },
        MatchRow {
            name: "system scope takes an event with no subject",
            scope: Scope::System,
            kinds: all,
            event: helper_ended(),
            delivered: true,
        },
        MatchRow {
            name: "actor scope takes its actor",
            scope: Scope::Actor(id("g")),
            kinds: all,
            event: ended(chain(&["r", "p", "g"])),
            delivered: true,
        },
        MatchRow {
            name: "actor scope skips a descendant of its actor",
            scope: Scope::Actor(id("p")),
            kinds: all,
            event: ended(chain(&["r", "p", "g"])),
            delivered: false,
        },
        MatchRow {
            name: "actor scope skips an event with no subject",
            scope: Scope::Actor(id("g")),
            kinds: all,
            event: helper_ended(),
            delivered: false,
        },
        MatchRow {
            name: "subtree scope takes a grandchild",
            scope: Scope::Subtree(id("r")),
            kinds: all,
            event: ended(chain(&["r", "p", "g"])),
            delivered: true,
        },
        MatchRow {
            name: "subtree scope takes a child",
            scope: Scope::Subtree(id("p")),
            kinds: all,
            event: ended(chain(&["r", "p", "g"])),
            delivered: true,
        },
        MatchRow {
            name: "subtree scope takes its root",
            scope: Scope::Subtree(id("g")),
            kinds: all,
            event: ended(chain(&["r", "p", "g"])),
            delivered: true,
        },
        MatchRow {
            name: "subtree scope skips another tree",
            scope: Scope::Subtree(id("x")),
            kinds: all,
            event: ended(chain(&["r", "p", "g"])),
            delivered: false,
        },
        MatchRow {
            name: "subtree scope skips an event with no subject",
            scope: Scope::Subtree(id("r")),
            kinds: all,
            event: helper_ended(),
            delivered: false,
        },
        MatchRow {
            name: "a kind outside the filter is skipped",
            scope: Scope::System,
            kinds: KindSet::of(&[EventKind::StopRequested]),
            event: ended(chain(&["r", "p", "g"])),
            delivered: false,
        },
    ];
    for row in rows {
        let (queue, mut rx) = SubQueue::new(4);
        let entry = SubEntry {
            id: SubscriptionId(1),
            watcher: aref("w", 1),
            filter: Filter::new(row.scope).kinds(row.kinds),
            queue,
        };
        let kind = row.event.kind().unwrap();
        subscription::deliver(&[entry], kind, &Arc::new(row.event));
        assert_eq!(rx.try_recv().is_ok(), row.delivered, "{}", row.name);
    }
}

enum Step {
    Emit(usize),
    Pop(usize),
}

/// Drops are counted in the queue and carried by the next event that goes in.
#[test]
fn drops_are_counted_and_carried() {
    use Step::{Emit, Pop};
    struct Row {
        name: &'static str,
        capacity: usize,
        steps: Vec<Step>,
        /// The `missed_before` of the items left in the queue.
        items: Vec<u64>,
        /// The drops counted and not yet carried.
        dropped: u64,
    }
    let row = |name, capacity, steps, items, dropped| Row {
        name,
        capacity,
        steps,
        items,
        dropped,
    };
    let rows = vec![
        row("room for all", 3, vec![Emit(2)], vec![0, 0], 0),
        row(
            "three refused, carried by the next accepted one",
            2,
            vec![Emit(5), Pop(1), Emit(1)],
            vec![0, 3],
            0,
        ),
        row(
            "a count carried once is not carried again",
            1,
            vec![Emit(3), Pop(1), Emit(1), Pop(1), Emit(1)],
            vec![0],
            0,
        ),
        row(
            "refused again after the carry",
            1,
            vec![Emit(3), Pop(1), Emit(2)],
            vec![2],
            1,
        ),
    ];
    for Row {
        name,
        capacity: cap,
        steps,
        items: expect_items,
        dropped: expect_dropped,
    } in rows
    {
        let (queue, mut rx) = SubQueue::new(cap);
        let entry = SubEntry {
            id: SubscriptionId(1),
            watcher: aref("w", 1),
            filter: Filter::new(Scope::System),
            queue: queue.clone(),
        };
        let subs = [entry];
        let mut emitted = 0u64;
        for step in steps {
            match step {
                Emit(n) => {
                    for _ in 0..n {
                        let ev = Arc::new(ended(aref("e", emitted)));
                        subscription::deliver(&subs, EventKind::ActorEnded, &ev);
                        emitted += 1;
                    }
                }
                Pop(n) => {
                    for _ in 0..n {
                        rx.try_recv().expect("an item to pop");
                    }
                }
            }
        }
        let mut left = Vec::new();
        while let Ok(item) = rx.try_recv() {
            match item {
                QueueItem::Event { missed_before, .. } => left.push(missed_before),
                QueueItem::Wake => {}
            }
        }
        assert_eq!(
            left, expect_items,
            "{name}: missed_before of the items left"
        );
        assert_eq!(
            queue.dropped.load(Ordering::SeqCst),
            expect_dropped,
            "{name}: dropped left"
        );
    }
}

/// The queue bound is clamped to `1..=MAX_SUBSCRIPTION_QUEUE`, in the filter
/// and in the queue a subscription gets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_clamps() {
    let rows: [(usize, usize); 6] = [
        (0, 1),
        (1, 1),
        (64, 64),
        (MAX_SUBSCRIPTION_QUEUE, MAX_SUBSCRIPTION_QUEUE),
        (100_000, MAX_SUBSCRIPTION_QUEUE),
        (usize::MAX, MAX_SUBSCRIPTION_QUEUE),
    ];
    let sys = new_system();
    let got = Got::default();
    let h = spawn_watcher(&sys, &got, 8).await;
    let mode = sys.telemetry_mode().clone();
    for (asked, expected) in rows {
        assert_eq!(
            Filter::new(Scope::System).queue(asked).queue,
            expected,
            "filter for {asked}"
        );
        let id = subscribe(&h, Filter::new(Scope::System).queue(asked)).await;
        let capacity = {
            let tables = mode.sinks.tables.lock().unwrap();
            tables
                .subs
                .iter()
                .find(|entry| entry.id == SubscriptionId(id))
                .map(|entry| entry.queue.tx.max_capacity())
        };
        assert_eq!(capacity, Some(expected), "queue for {asked}");
    }
}

/// An overflow reaches the watcher as a notice with the count when the queue
/// runs empty, and nothing is lost or counted twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overflow_notice_when_idle_with_the_count() {
    const EMITTED: u64 = 40;
    let sys = new_system();
    let got = Got::default();
    let h = spawn_watcher(&sys, &got, 2).await;
    let mode = sys.telemetry_mode().clone();
    subscribe(&h, Filter::new(Scope::System).queue(3)).await;
    let release = jam(&h, 2).await;
    let queue = {
        let tables = mode.sinks.tables.lock().unwrap();
        tables.subs[0].queue.clone()
    };
    // The first event goes in alone, and the burst starts once the forwarder
    // has taken it out: from then on the forwarder waits for room in the
    // jammed mailbox and takes nothing more, so the queue fills once and the
    // events after the three it holds are all dropped. A burst that began
    // while the forwarder might still take its first event could free a slot
    // in the middle of the burst, which the burst's next event takes, and
    // the drops before it would then be reported as that event's
    // `missed_before` with no drop after it left to report as a notice.
    emit(&mode, ended(aref("e", 0)));
    wait_until("the forwarder took the first event", || {
        queue.tx.capacity() == queue.tx.max_capacity()
    })
    .await;
    for i in 1..EMITTED {
        emit(&mode, ended(aref("e", i)));
    }
    release.notify_one();
    wait_until("every event accounted for", || accounted(&got) >= EMITTED).await;
    // Nothing more arrives: the count is exact.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(accounted(&got), EMITTED, "delivered plus reported drops");
    // The first event and the three the queue held were delivered, and the
    // other thirty-six came as one notice after the queue ran empty.
    assert_eq!(plain_events(&got), 4, "the first event and a queue of 3");
    assert_eq!(
        notices(&got),
        vec![SubscriptionNotice::Overflow {
            missed: EMITTED - 4
        }],
        "the drops after the last accepted event came as one overflow notice"
    );
}

/// A watcher never receives events about itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_events_are_excluded() {
    let sys = new_system();
    let got = Got::default();
    let h = spawn_watcher(&sys, &got, 8).await;
    let mode = sys.telemetry_mode().clone();
    subscribe(&h, Filter::new(Scope::System)).await;
    emit(&mode, ended(own_ref(&sys, &h)));
    emit(&mode, ended(aref("other", 1)));
    wait_until("the other event", || plain_events(&got) >= 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let got = got.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].event.subject().unwrap().id().as_str(), "other");
}

/// A subtree watcher receives the event of a member whose parent has left the
/// registry: the match reads the event's own ancestry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subtree_match_reads_the_event_not_the_registry() {
    let sys = new_system();
    let got = Got::default();
    let watcher = spawn_watcher(&sys, &got, 8).await;
    let mode = sys.telemetry_mode().clone();
    mode.bind_system(&sys);
    // A parent that existed and is gone.
    let parent = spawn_watcher(&sys, &Got::default(), 8).await;
    let parent_ref = own_ref(&sys, &parent);
    let parent_id = parent_ref.id().clone();
    parent.stop(StopReason::Graceful).await.unwrap();
    parent.wait_stopped().await;
    assert!(
        sys.get_by_id::<Watcher>(&parent_id).is_none(),
        "the parent is freed"
    );
    let root = own_ref(&sys, &watcher);
    let middle = ActorRef::new_child(parent_id, parent_ref.incarnation(), &root);
    let grandchild = ActorRef::new_child(ActorId::from("grandchild"), 1, &middle);
    subscribe(&watcher, Filter::new(Scope::Subtree(root.id().clone()))).await;
    emit(&mode, ended(grandchild));
    wait_until("the grandchild's end", || plain_events(&got) == 1).await;
}

/// A map that panics ends its subscription with one notice, and the actor
/// goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn map_panic_ends_the_subscription_with_one_notice() {
    // (name, panics on events only, notices the watcher receives)
    let rows = [
        ("panics on events", true, 1usize),
        ("panics on the notice too", false, 0usize),
    ];
    for (name, on_events_only, expected) in rows {
        let sys = new_system();
        let got = Got::default();
        let h = spawn_watcher(&sys, &got, 8).await;
        let mode = sys.telemetry_mode().clone();
        let map_mode = if on_events_only {
            MapMode::PanicOnEvents
        } else {
            MapMode::PanicAlways
        };
        let id = h
            .send(Msg::Subscribe(Filter::new(Scope::System), map_mode))
            .await
            .unwrap();
        assert_eq!(mode.subscription_count(), 1, "{name}");
        emit(&mode, ended(aref("a", 1)));
        wait_until("the subscription to end", || mode.subscription_count() == 0).await;
        emit(&mode, ended(aref("b", 1)));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            h.send(Msg::Unsubscribe(id)).await.unwrap(),
            0,
            "{name}: the ended subscription is not held by the actor"
        );
        assert_eq!(notices(&got).len(), expected, "{name}: notices");
        if expected == 1 {
            assert_eq!(
                notices(&got),
                vec![SubscriptionNotice::MapPanicked],
                "{name}"
            );
        }
        assert_eq!(plain_events(&got), 0, "{name}: no event got through");
        assert_eq!(mode.class_bits(), 0, "{name}: the gate is disarmed");
        assert!(h.is_alive(), "{name}: the actor lives");
        assert!(
            h.send(Msg::Filler).await.is_ok(),
            "{name}: the actor still answers"
        );
    }
}

// ---------------------------------------------------------------------------
// A subscription ends with its incarnation
// ---------------------------------------------------------------------------
