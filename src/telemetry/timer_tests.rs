//! The `TimerTick` event of a recurring timer: the lateness and the missed
//! count each miss policy reports, the pause of a catch-up burst that meets a
//! full mailbox, and the timer's id and policy on the event.
//!
//! The policy rows run on a paused clock advanced by an exact amount, with
//! the adjustment called directly so the numbers are exact; the loop rows run
//! a real timer on both runtime flavors.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::{self, Instant};

use tokio_util::sync::CancellationToken;

use super::{EventKind, KindSet, TelemetryEvent};
use crate::actor::context::ActorContext;
use crate::actor::context::{adjust_next, Adjusted, MessageFactory, TickEvents};
use crate::actor::handle::ActorHandle;
use crate::actor::runtime::ActorConfig;
use crate::actor::{Actor, ActorExt};
use crate::error::ActorResult;
use crate::system::ActorSystem;
use crate::types::{MissPolicy, RecurringId};

const INTERVAL: Duration = Duration::from_millis(100);
const LATE: Duration = Duration::from_millis(350);

/// What one event reported: policy, timer, lateness, missed, paused.
type Seen = (String, RecurringId, Duration, u64, bool);

/// An actor whose turn waits on a gate, so its mailbox can be filled.
struct Gated {
    entered: Arc<AtomicUsize>,
    gate: Arc<Semaphore>,
}

impl Actor for Gated {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let permit = self.gate.acquire().await.expect("the gate stays open");
        permit.forget();
        Ok(())
    }
}

/// Collects the `TimerTick` events of `sys`.
fn collect(sys: &Arc<ActorSystem>) -> Arc<Mutex<Vec<Seen>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    assert!(sys.attach_telemetry(
        "timer-ticks",
        KindSet::of(&[EventKind::TimerTick]),
        move |event| {
            if let TelemetryEvent::TimerTick {
                policy,
                timer,
                lateness,
                missed,
                paused,
                ..
            } = event
            {
                sink.lock().unwrap().push((
                    format!("{policy:?}"),
                    *timer,
                    *lateness,
                    *missed,
                    *paused,
                ));
            }
        }
    ));
    seen
}

/// A gated actor with a mailbox of `capacity` on a fresh system, and the events its
/// timers raise.
async fn gated_actor(
    capacity: usize,
) -> (
    Arc<ActorSystem>,
    ActorHandle<Gated>,
    Arc<AtomicUsize>,
    Arc<Semaphore>,
    Arc<Mutex<Vec<Seen>>>,
) {
    let sys = ActorSystem::create(format!("timer-{}", uuid::Uuid::new_v4())).unwrap();
    let entered = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Semaphore::new(0));
    let handle = Gated {
        entered: entered.clone(),
        gate: gate.clone(),
    }
    .spawn()
    .named(format!("timer-{}", uuid::Uuid::new_v4()))
    .with_config(ActorConfig::default().with_mailbox_capacity(capacity))
    .on_system(&sys)
    .await
    .expect("the actor starts");
    let seen = collect(&sys);
    (sys, handle, entered, gate, seen)
}

/// Fills the one-slot mailbox: the first message is inside its turn, the
/// second waits in the queue.
async fn fill(handle: &ActorHandle<Gated>, entered: &AtomicUsize) {
    handle.notify(()).await.expect("the first message is taken");
    while entered.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    handle.notify(()).await.expect("the second message queues");
}

/// One row of the policy table.
struct Row {
    policy: MissPolicy,
    full: bool,
    /// The events: lateness in milliseconds, missed, paused.
    events: &'static [(u64, u64, bool)],
    /// The next deadline, in milliseconds after the tick's due time.
    next: u64,
}

const ROWS: &[Row] = &[
    Row {
        policy: MissPolicy::Skip,
        full: false,
        events: &[(250, 2, false)],
        next: 400,
    },
    Row {
        policy: MissPolicy::Delay,
        full: false,
        events: &[(250, 0, false)],
        next: 450,
    },
    Row {
        policy: MissPolicy::CatchUp,
        full: false,
        events: &[(250, 0, false), (150, 0, false), (50, 0, false)],
        next: 400,
    },
    Row {
        policy: MissPolicy::CatchUp,
        full: true,
        events: &[(250, 0, false), (150, 0, true)],
        next: 200,
    },
];

/// A 100 ms timer whose tick is delivered 350 ms after it was due reports,
/// per policy, the lateness and the ticks it missed; a catch-up burst that
/// meets a full mailbox reports one pause.
#[tokio::test(start_paused = true)]
async fn each_policy_reports_its_lateness_and_missed_ticks() {
    for row in ROWS {
        let (_sys, handle, entered, gate, seen) = gated_actor(if row.full { 1 } else { 8 }).await;
        if row.full {
            fill(&handle, &entered).await;
        }
        let due = Instant::now();
        let mut next = due + INTERVAL;
        time::advance(LATE).await;
        let mut factory: Box<MessageFactory<Gated>> = Box::new(|| ());
        let timer = RecurringId::new(7);
        let adjusted = adjust_next(
            &mut next,
            INTERVAL,
            row.policy,
            &CancellationToken::new(),
            &handle,
            &mut factory,
            TickEvents { timer },
        );
        assert!(matches!(adjusted, Adjusted::Next));
        let ms = |n: u64| Duration::from_millis(n);
        // The ticks reach the handler through the pump: wait for the count,
        // by yielding, on a clock that is paused.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while seen.lock().unwrap().len() < row.events.len() {
            assert!(
                std::time::Instant::now() < deadline,
                "{:?} full={}: the ticks were not delivered",
                row.policy,
                row.full
            );
            tokio::task::yield_now().await;
        }
        let got = seen.lock().unwrap().clone();
        let want: Vec<Seen> = row
            .events
            .iter()
            .map(|&(late, missed, paused)| {
                (format!("{:?}", row.policy), timer, ms(late), missed, paused)
            })
            .collect();
        assert_eq!(got, want, "{:?} full={}", row.policy, row.full);
        assert_eq!(
            next.duration_since(due),
            ms(row.next),
            "{:?} full={}: next deadline",
            row.policy,
            row.full
        );
        gate.add_permits(16);
    }
}

/// A running timer reports its own id and policy on every tick, on both
/// runtime flavors.
async fn a_running_timer_reports_its_id_and_policy() {
    struct Ticker {
        timer: Arc<Mutex<Option<RecurringId>>>,
    }
    impl Actor for Ticker {
        type Message = ();
        type Response = ();

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            let id = ctx
                .every_with(Duration::from_millis(5), || ())
                .on_miss(MissPolicy::Delay)
                .await?;
            *self.timer.lock().unwrap() = Some(id);
            Ok(())
        }

        async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
    }

    let sys = ActorSystem::create(format!("timer-{}", uuid::Uuid::new_v4())).unwrap();
    let seen = collect(&sys);
    let timer = Arc::new(Mutex::new(None));
    let _handle = Ticker {
        timer: timer.clone(),
    }
    .spawn()
    .named(format!("ticker-{}", uuid::Uuid::new_v4()))
    .on_system(&sys)
    .await
    .expect("the actor starts");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while seen.lock().unwrap().len() < 3 {
        assert!(std::time::Instant::now() < deadline, "no ticks reported");
        time::sleep(Duration::from_millis(5)).await;
    }
    let id = timer.lock().unwrap().expect("the timer started");
    for (policy, timer, _, missed, paused) in seen.lock().unwrap().iter() {
        assert_eq!(policy, "Delay");
        assert_eq!(*timer, id);
        assert_eq!(*missed, 0);
        assert!(!paused);
    }
}
