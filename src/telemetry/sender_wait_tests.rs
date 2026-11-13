//! The sender-wait event: a send that found room raises nothing and reads no
//! clock; a send that parked reports how its wait ended, a grant, a close or
//! a cancel, all through the pump: the polling task may be a forwarder the
//! actor's end joins, so it never runs a handler.
//!
//! No step is ordered by a sleep: a parked state is reached by polling a
//! reservation by hand, and a wait for the pump polls a condition. The one
//! sleep sets a lower bound for a reported duration.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use super::{EventKind, HelperEnd, HelperKind, Identity, KindSet, TelemetryEvent, WaitOutcome};
use crate::actor::mailbox::{channel, Closed, MailRx, MailTx, Permit, Reserve, MAILBOX_LANES};
use crate::actor::subtree::AbortRecord;
use crate::system::ActorSystem;
use crate::types::ActorId;

/// The longest a wait for a condition may take; it only limits how long a
/// real hang takes to report.
const BOUND: Duration = Duration::from_secs(60);

/// The stretch a wait lasts before its end, which the reported duration must
/// cover.
const WAIT: Duration = Duration::from_millis(20);

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

fn unique() -> String {
    format!(
        "sender-wait-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

/// What a handler saw of one event.
#[derive(Clone, Debug)]
struct Seen {
    outcome: WaitOutcome,
    waited: Duration,
    owner: String,
    task: Option<tokio::task::Id>,
}

type Log = Arc<Mutex<Vec<Seen>>>;

fn entries(log: &Log) -> Vec<Seen> {
    log.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

const HANDLER: &str = "sender-wait-log";

/// The owner the sentinel names.
const MARK: &str = "sender-wait-mark";

/// A mailbox of the given capacity that belongs to an actor of a fresh system,
/// and the log of the events a handler on that system sees.
struct Rig {
    sys: Arc<ActorSystem>,
    tx: MailTx<u32>,
    rx: MailRx<u32>,
    log: Log,
    /// The sentinels the handler has seen.
    marks: Arc<AtomicUsize>,
}

impl Rig {
    fn new(capacity: usize, armed: bool) -> Self {
        let sys = ActorSystem::create(unique()).unwrap();
        let mode = sys.telemetry_mode();
        let record = AbortRecord::in_system(mode.clone(), crate::types::Incarnation::mint());
        mode.bind(
            record.key(),
            Arc::new(Identity::new(ActorId::from("owner"), 1, None, None, None)),
        );
        let (tx, rx) = channel(capacity, MAILBOX_LANES, None, Some(record));
        let rig = Self {
            sys,
            tx,
            rx,
            log: Log::default(),
            marks: Arc::new(AtomicUsize::new(0)),
        };
        if armed {
            rig.arm();
        }
        rig
    }

    fn arm(&self) {
        let log = self.log.clone();
        let marks = self.marks.clone();
        assert!(self.sys.attach_telemetry(
            HANDLER,
            KindSet::of(&[EventKind::SenderWaited, EventKind::HelperEnded]),
            move |event| match event {
                TelemetryEvent::SenderWaited {
                    owner,
                    waited,
                    outcome,
                } => {
                    log.lock().unwrap_or_else(|p| p.into_inner()).push(Seen {
                        outcome: *outcome,
                        waited: *waited,
                        owner: owner.id().to_string(),
                        task: tokio::task::try_id(),
                    });
                }
                TelemetryEvent::HelperEnded {
                    owner: Some(owner), ..
                } if owner.as_str() == MARK => {
                    marks.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            },
        ));
    }

    /// Pushes a sentinel through the pump and waits for the handler to see
    /// it: every event queued before it has been delivered, as the queue is
    /// one FIFO. The rig must be armed.
    async fn flush(&self) {
        let before = self.marks.load(Ordering::SeqCst);
        self.sys
            .telemetry_mode()
            .push(EventKind::HelperEnded, || TelemetryEvent::HelperEnded {
                kind: HelperKind::Pump,
                owner: Some(ActorId::from(MARK)),
                end: HelperEnd::Finished,
            });
        until("the sentinel is delivered", || {
            self.marks.load(Ordering::SeqCst) > before
        })
        .await;
    }

    fn disarm(&self) {
        assert!(self.sys.detach_telemetry(HANDLER));
    }

    /// Fills the mailbox to its capacity.
    async fn fill(&self) {
        for n in 0..self.tx.capacity() as u32 {
            match self.tx.reserve().await {
                Ok(permit) => {
                    permit.send(n);
                }
                Err(Closed) => panic!("closed"),
            }
        }
        assert_eq!(self.tx.capacity(), 0);
    }
}

/// Runs `body` on a multi-thread runtime and on a current-thread runtime.
fn both_flavors<F, Fut>(body: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    for multi in [true, false] {
        let runtime = if multi {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        };
        runtime.block_on(body());
    }
}

/// Waits until `done` holds, or fails after the bound.
async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + BOUND;
    while !done() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Polls a reservation once with a waker that does nothing.
fn poll_by_hand<'a>(reserve: &mut Reserve<'a, u32>) -> Poll<Result<Permit<'a, u32>, Closed>> {
    let mut cx = Context::from_waker(Waker::noop());
    Pin::new(reserve).poll(&mut cx)
}

/// Polls a reservation once and says whether it parked.
fn parks(reserve: &mut Reserve<'_, u32>) -> bool {
    poll_by_hand(reserve).is_pending()
}

/// Polls a reservation the receiver granted a unit to and gives the unit back.
fn takes_the_grant(reserve: &mut Reserve<'_, u32>) {
    match poll_by_hand(reserve) {
        Poll::Ready(Ok(permit)) => drop(permit),
        _ => panic!("the reservation did not take its grant"),
    }
}

#[derive(Clone, Copy, Debug)]
enum Ending {
    /// The receiver takes a message while the sender is parked.
    Granted,
    /// The receiver closes the mailbox while the sender is parked.
    Closed,
    /// The sender's future is dropped by a timeout that expires.
    TimedOut,
    /// A unit was granted and the reservation is dropped before it saw it.
    GrantedThenDropped,
}

#[test]
fn a_parked_send_reports_how_its_wait_ended() {
    // The ending and the outcome it reports. Every outcome takes the pump's
    // route, so the handler never runs on the sender's task.
    let rows = [
        (Ending::Granted, WaitOutcome::Granted),
        (Ending::Closed, WaitOutcome::Closed),
        (Ending::TimedOut, WaitOutcome::Cancelled),
        (Ending::GrantedThenDropped, WaitOutcome::Cancelled),
    ];
    both_flavors(|| async {
        for (ending, want) in rows {
            let rig = Rig::new(1, true);
            let log = rig.log.clone();
            let sender = tokio::spawn(async move {
                let mut rig = rig;
                let me = tokio::task::id();
                rig.fill().await;
                let tx = rig.tx.clone();
                match ending {
                    Ending::Granted => {
                        let mut reserve = tx.reserve();
                        assert!(parks(&mut reserve));
                        std::thread::sleep(WAIT);
                        assert!(rig.rx.try_recv().is_ok());
                        takes_the_grant(&mut reserve);
                    }
                    Ending::Closed => {
                        let mut reserve = tx.reserve();
                        assert!(parks(&mut reserve));
                        std::thread::sleep(WAIT);
                        rig.rx.close();
                        assert!(matches!(
                            poll_by_hand(&mut reserve),
                            Poll::Ready(Err(Closed))
                        ));
                    }
                    Ending::TimedOut => {
                        let timed = tokio::time::timeout(WAIT, tx.reserve()).await;
                        assert!(timed.is_err(), "the send found room");
                    }
                    Ending::GrantedThenDropped => {
                        let mut reserve = tx.reserve();
                        assert!(parks(&mut reserve));
                        std::thread::sleep(WAIT);
                        assert!(rig.rx.try_recv().is_ok());
                        drop(reserve);
                        assert_eq!(tx.capacity(), 1, "the granted unit was not given back");
                    }
                }
                me
            });
            let me = sender.await.unwrap();
            until("the event", || !entries(&log).is_empty()).await;
            let seen = entries(&log);
            assert_eq!(seen.len(), 1, "{ending:?}: one event for one wait");
            assert_eq!(seen[0].outcome, want, "{ending:?}");
            assert_eq!(seen[0].owner, "owner", "{ending:?}");
            assert!(
                seen[0].waited >= WAIT / 2,
                "{ending:?}: {:?}",
                seen[0].waited
            );
            assert!(
                seen[0].task.is_some() && seen[0].task != Some(me),
                "{ending:?}: the handler ran on the sender's task or off the runtime"
            );
        }
    });
}

/// A gate a handler blocks on, opened by the test.
struct Gate {
    open: Mutex<bool>,
    changed: std::sync::Condvar,
    entered: std::sync::atomic::AtomicBool,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            open: Mutex::new(false),
            changed: std::sync::Condvar::new(),
            entered: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Blocks the calling thread until the gate opens, or twenty seconds.
    fn block(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        while !*open {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return;
            }
            open = self
                .changed
                .wait_timeout(open, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    fn release(&self) {
        *self.open.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.changed.notify_all();
    }

    fn is_open(&self) -> bool {
        *self.open.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Opens its gate when it drops, so a failed assertion never leaves a handler
/// blocking a worker.
struct Release(Arc<Gate>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// An actor whose timer ticks into a mailbox of one while its turn waits on a
/// semaphore.
struct Ticker {
    turn: Arc<tokio::sync::Semaphore>,
}

impl crate::actor::Actor for Ticker {
    type Message = ();
    type Response = ();

    async fn on_started(
        &mut self,
        ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        ctx.every_with(Duration::from_millis(2), || ()).await?;
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        self.turn
            .acquire()
            .await
            .expect("the semaphore stays open")
            .forget();
        Ok(())
    }
}

/// A timer's forwarder is granted room on a full mailbox and reports the
/// wait: a `SenderWaited` handler that blocks does not hold up the end of the
/// actor, which joins the forwarder, because the handler runs on the pump's
/// helper and not in the forwarder's poll. Multi-thread only, as the handler
/// blocks a thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_sender_wait_handler_does_not_hold_up_the_end_of_the_actor() {
    use crate::actor::runtime::ActorConfig;
    use crate::actor::ActorExt;

    let sys = ActorSystem::create(unique()).unwrap();
    let gate = Gate::new();
    let _release = Release(gate.clone());
    let blocker = gate.clone();
    assert!(sys.attach_telemetry(
        "block-sender-waited",
        KindSet::of(&[EventKind::SenderWaited]),
        move |_| blocker.block()
    ));
    let turn = Arc::new(tokio::sync::Semaphore::new(0));
    let handle = Ticker { turn: turn.clone() }
        .spawn()
        .named(unique())
        .with_config(ActorConfig::default().with_mailbox_capacity(1))
        .on_system(&sys)
        .await
        .expect("the actor starts");
    // The first tick is inside the held turn, the second fills the queue and
    // the third parks the timer's forwarder.
    until("the timer's forwarder parked", || {
        handle.mailbox_waiting() == 1
    })
    .await;
    // The held turn returns and the actor takes the queued tick, which grants
    // its unit to the forwarder; the forwarder reports the wait.
    turn.add_permits(1);
    until("the wait was reported to the handler", || {
        gate.entered.load(Ordering::SeqCst)
    })
    .await;
    // The handler is blocked. The actor is stopped now; its end joins the
    // forwarder, which must not be inside the handler.
    handle
        .stop(crate::types::StopReason::Graceful)
        .await
        .expect("the stop is accepted");
    turn.add_permits(1000);
    let ended = tokio::time::timeout(Duration::from_secs(10), handle.wait_stopped()).await;
    let open = gate.is_open();
    assert!(
        ended.is_ok(),
        "the actor's end waited for a blocked handler"
    );
    assert!(!open, "the handler was released before the end");
}
