#![allow(unused_imports)]
//! A timer or stream forwarder releases what it owns as soon as it sees its
//! actor's mailbox closed (a refused send, or the close itself while it
//! waits for a tick, a firing or an item), in its own poll, catching each
//! drop's panic and reporting it as made after the close. A drop it makes
//! before it sees the close is reported as the run's. A stream that ends while the mailbox is
//! open is dropped before its `Finished` reaches the mailbox.
//!
//! Every test runs on a paused clock: a wait ends only once every task is
//! idle.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{
    adjust_next, oneshot_forward, recurring_loop, stream_forward, take_parked, ActorContext,
    Adjusted, ForwarderKind, MessageFactory, PanicSlot, RecurringId, Released, StreamRegistration,
    TickEvents,
};
use crate::actor::handle::ActorHandle;
use crate::actor::subtree::SubtreeHold;
use crate::actor::{Actor, ActorEnvelope};
use crate::error::{ActorError, ActorResult};
use crate::types::{
    ActorId, ActorStatus, Envelope, MissPolicy, StatusTx, StatusValue, StopLane, StreamEvent,
    TurnClock,
};

/// Records its drop, then panics.
struct Bomb(Arc<AtomicBool>);

impl Drop for Bomb {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
        panic!("a forwarder's item panicked when dropped");
    }
}

/// The `Finished` message's drops, for the one test whose `Finished` panics
/// when dropped.
static FINISHED_DROPS: AtomicUsize = AtomicUsize::new(0);

/// Panics when dropped, counting in [`FINISHED_DROPS`].
struct FinishedBomb;

impl Drop for FinishedBomb {
    fn drop(&mut self) {
        FINISHED_DROPS.fetch_add(1, Ordering::SeqCst);
        panic!("a Finished message panicked when dropped");
    }
}

enum Msg {
    Bomb(#[allow(dead_code)] Bomb),
    Data,
    Finished,
}

impl From<StreamEvent<()>> for Msg {
    fn from(event: StreamEvent<()>) -> Self {
        match event {
            StreamEvent::Data(()) => Msg::Data,
            StreamEvent::Finished => Msg::Finished,
        }
    }
}

struct Sink;

impl Actor for Sink {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, _msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A message type whose `Finished` panics when dropped.
enum FMsg {
    Filler,
    Data,
    Finished(#[allow(dead_code)] FinishedBomb),
}

impl From<StreamEvent<()>> for FMsg {
    fn from(event: StreamEvent<()>) -> Self {
        match event {
            StreamEvent::Data(()) => FMsg::Data,
            StreamEvent::Finished => FMsg::Finished(FinishedBomb),
        }
    }
}

struct FinishedSink;

impl Actor for FinishedSink {
    type Message = FMsg;
    type Response = ();

    async fn handle(&mut self, _msg: FMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A handle on a bare one-slot mailbox, and the mailbox's receiver.
fn handle_of<A: Actor>() -> (
    ActorHandle<A>,
    crate::actor::mailbox::MailRx<ActorEnvelope<A>>,
) {
    handle_with_room(1)
}

/// As [`handle_of`], with room for `room` messages.
fn handle_with_room<A: Actor>(
    room: usize,
) -> (
    ActorHandle<A>,
    crate::actor::mailbox::MailRx<ActorEnvelope<A>>,
) {
    let (tx, rx) = crate::actor::mailbox::test_channel(room);
    let (_status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, _) = StopLane::new();
    let handle = ActorHandle::new(
        ActorId::from("forwarder-sink"),
        tx,
        stop_lane,
        room,
        status_rx,
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    (handle, rx)
}

/// A handle whose mailbox is closed, as at the end of its actor's loop.
fn closed() -> ActorHandle<Sink> {
    let (handle, mut rx) = handle_of::<Sink>();
    rx.close();
    handle
}

/// Yields `items` items, then ends; owns `bomb`.
struct Owned<B> {
    bomb: B,
    items: usize,
}

impl<B: Unpin> Stream for Owned<B> {
    type Item = ();
    fn poll_next(mut self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        let _ = &self.bomb;
        if self.items == 0 {
            return Poll::Ready(None);
        }
        self.items -= 1;
        Poll::Ready(Some(()))
    }
}

/// Runs a forwarder's future to its end and asserts it released what it
/// owned at the close: the drop ran, inside the future, and its panic is
/// reported as made after the close.
async fn releases_at_the_close<F: std::future::Future<Output = Released>>(
    fut: F,
    dropped: &AtomicBool,
) {
    let released = tokio::time::timeout(Duration::from_secs(3600), fut)
        .await
        .expect("the forwarder ends by itself");
    assert!(dropped.load(Ordering::SeqCst), "the drop ran in the future");
    assert!(released.run.is_none(), "no drop before the close");
    assert!(
        released
            .end
            .as_deref()
            .is_some_and(|p| p.contains("panicked when dropped")),
        "the drop's panic is reported as made after the close"
    );
}

/// Fills the one slot of an open mailbox, so a forwarder's next send waits.
fn full<A: Actor>(
    filler: A::Message,
) -> (
    ActorHandle<A>,
    crate::actor::mailbox::MailRx<ActorEnvelope<A>>,
) {
    let (handle, rx) = handle_of::<A>();
    assert!(handle.try_forward(filler).is_ok(), "the slot is filled");
    (handle, rx)
}

/// Lets every task run until all are idle (the paused clock moves only
/// then): a forwarder whose send waits for room has reached that send.
async fn until_idle() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

/// Closes the mailbox under a forwarder waiting in its send and asserts the
/// refused message and what the forwarder owns are released at the close.
async fn refused_at_the_close(
    task: tokio::task::JoinHandle<Released>,
    mut rx: crate::actor::mailbox::MailRx<ActorEnvelope<Sink>>,
    dropped: &AtomicBool,
) {
    until_idle().await;
    assert!(!task.is_finished(), "the send waits for room");
    assert!(!dropped.load(Ordering::SeqCst));
    rx.close();
    let released = tokio::time::timeout(Duration::from_secs(3600), task)
        .await
        .expect("the forwarder ends by itself")
        .expect("the forwarder's own drops are caught");
    assert!(dropped.load(Ordering::SeqCst), "released at the close");
    assert!(released.run.is_none(), "no drop before the close");
    assert!(
        released
            .end
            .as_deref()
            .is_some_and(|p| p.contains("panicked when dropped")),
        "the drop's panic is reported as made after the close"
    );
}

/// The first deadline is the one the timer was registered with, however late
/// its task is first polled: with the clock 250 ms past a registration whose
/// first tick was due after 100 ms, every policy sends that tick at once, and
/// a catch-up timer sends the one due at 200 ms too.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_recurring_timer_counts_its_first_interval_from_the_call() {
    let interval = Duration::from_millis(100);
    for (policy, ticks) in [
        (MissPolicy::Skip, 1),
        (MissPolicy::Delay, 1),
        (MissPolicy::CatchUp, 2),
    ] {
        let called = Instant::now();
        let built = Arc::new(Mutex::new(Vec::new()));
        let factory: Box<MessageFactory<Sink>> = {
            let built = built.clone();
            Box::new(move || {
                built.lock().unwrap().push(Instant::now());
                Msg::Data
            })
        };
        let (handle, _rx) = handle_with_room::<Sink>(8);
        tokio::time::advance(Duration::from_millis(250)).await;
        let token = CancellationToken::new();
        let task = tokio::spawn(recurring_loop(
            handle,
            RecurringId::new(0),
            factory,
            called + interval,
            interval,
            policy,
            token.clone(),
            PanicSlot::default(),
        ));
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        let built = built.lock().unwrap().clone();
        assert_eq!(
            built.len(),
            ticks,
            "{policy:?}: ticks sent at the first poll"
        );
        assert!(
            built
                .iter()
                .all(|at| *at == called + Duration::from_millis(250)),
            "{policy:?}: the first tick was due before the first poll"
        );
        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the timer ends");
    }
}

/// Never yields; owns a bomb.
struct Idle(#[allow(dead_code)] Bomb);

impl Stream for Idle {
    type Item = ();
    fn poll_next(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        Poll::Pending
    }
}

/// Yields an item on every poll, counting its polls; owns a bomb.
struct Counted {
    _bomb: Bomb,
    polls: Arc<AtomicUsize>,
}

impl Stream for Counted {
    type Item = ();
    fn poll_next(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(Some(()))
    }
}

/// Yields `None` after closing the mailbox's receiver itself, so the close
/// is read only after the stream ended; owns a bomb.
struct EndsClosing {
    _bomb: Bomb,
    rx: Arc<Mutex<crate::actor::mailbox::MailRx<ActorEnvelope<Sink>>>>,
}

impl Stream for EndsClosing {
    type Item = ();
    fn poll_next(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        self.rx.lock().unwrap().close();
        Poll::Ready(None)
    }
}

/// Records the mailbox's length at its drop, then panics.
struct LenBomb {
    handle: ActorHandle<Sink>,
    len_at_drop: Arc<Mutex<Option<usize>>>,
}

impl Drop for LenBomb {
    fn drop(&mut self) {
        *self.len_at_drop.lock().unwrap() = Some(self.handle.mailbox_len());
        panic!("an ended stream panicked when dropped");
    }
}

/// Panics with its text when dropped.
struct PanicsWith(String);

impl Drop for PanicsWith {
    fn drop(&mut self) {
        panic!("{}", self.0);
    }
}

/// Panics with its text when dropped, counting its drops.
struct Loud(String, Arc<AtomicUsize>);

impl Drop for Loud {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::SeqCst);
        panic!("{}", self.0);
    }
}

/// Counts its drops, quietly.
struct QuietDrops(Arc<AtomicUsize>);

impl Drop for QuietDrops {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

enum LMsg {
    Filler,
    Tick(#[allow(dead_code)] Option<Loud>),
    Quiet(#[allow(dead_code)] QuietDrops),
}

struct LSink;

impl Actor for LSink {
    type Message = LMsg;
    type Response = ();

    async fn handle(&mut self, _msg: LMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// How a recurring timer's ticks and factory behave in the tests below.
struct Ticks {
    /// The factory's calls.
    calls: Arc<AtomicUsize>,
    /// The drops of the ticks and of the factory's capture that panic.
    loud_drops: Arc<AtomicUsize>,
}

/// A factory counting its calls into `ticks`: the tick of call `n` panics
/// when dropped from call `loud_from` on, and the factory's capture panics
/// when dropped when `loud_factory` is set; each panic's text carries
/// `token`.
fn ticking(
    ticks: &Ticks,
    loud_from: Option<usize>,
    loud_factory: bool,
    token: &str,
) -> Box<MessageFactory<LSink>> {
    let calls = ticks.calls.clone();
    let loud = ticks.loud_drops.clone();
    let capture = loud_factory.then(|| Loud(format!("the factory panicked {token}"), loud.clone()));
    let tick_text = format!("the tick panicked {token}");
    Box::new(move || {
        let _ = &capture;
        let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
        let tick = loud_from
            .is_some_and(|from| n >= from)
            .then(|| Loud(tick_text.clone(), loud.clone()));
        LMsg::Tick(tick)
    })
}

/// Waits, by observation, until the factory was called `n` times. The bound only limits how long a real hang takes to report.
async fn until_called(ticks: &Ticks, n: usize) {
    tokio::time::timeout(Duration::from_secs(60), async {
        while ticks.calls.load(Ordering::SeqCst) < n {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the factory is called");
}

/// A recurring timer whose tick waits on a full mailbox is cancelled: the
/// tick is never sent, `adjust_next` never runs (the factory is called
/// once), and the timer ends with nothing released at the close. The tick
/// and the factory's capture drop each in its own catch: when both drops
/// panic the process does not abort, the tick's panic is the run's, and the
/// factory's is logged once, naming the actor; when only the factory's
/// drop panics, its panic is the run's.
async fn a_cancel_while_a_recurring_send_waits(
    policy: MissPolicy,
    loud_tick: bool,
    loud_factory: bool,
) {
    #[cfg(feature = "tracing")]
    let logged = crate::actor::supervision::lost_runner::capture::install();
    let token_text = uuid::Uuid::new_v4().to_string();
    let ticks = Ticks {
        calls: Arc::default(),
        loud_drops: Arc::default(),
    };
    let (handle, mut rx) = full::<LSink>(LMsg::Filler);
    let token = CancellationToken::new();
    let factory = ticking(&ticks, loud_tick.then_some(1), loud_factory, &token_text);
    let slot = PanicSlot::default();
    let task = tokio::spawn(recurring_loop(
        handle,
        RecurringId::new(0),
        factory,
        Instant::now() + Duration::from_millis(1),
        Duration::from_millis(1),
        policy,
        token.clone(),
        slot.clone(),
    ));
    until_called(&ticks, 1).await;
    token.cancel();
    let released = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the timer ends")
        .expect("no drop escaped its catch");
    assert!(
        take_parked(&slot).is_none(),
        "the factory was released, not left"
    );
    assert_eq!(
        ticks.calls.load(Ordering::SeqCst),
        1,
        "the factory ran once"
    );
    assert!(released.end.is_none());
    assert!(matches!(
        rx.try_recv(),
        Ok(Envelope::Message {
            payload: LMsg::Filler,
            ..
        })
    ));
    assert!(rx.try_recv().is_err(), "the tick was never sent");
    if loud_tick && loud_factory {
        assert_eq!(ticks.loud_drops.load(Ordering::SeqCst), 2);
        assert!(
            released
                .run
                .as_deref()
                .is_some_and(|p| p.contains("the tick panicked")),
            "{:?}",
            released.run
        );
        #[cfg(feature = "tracing")]
        {
            let lines: Vec<String> = logged
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains(&format!("the factory panicked {token_text}")))
                .cloned()
                .collect();
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].contains("actor_id=forwarder-sink "), "{lines:?}");
        }
    } else if loud_factory {
        assert!(
            released
                .run
                .as_deref()
                .is_some_and(|p| p.contains("the factory panicked")),
            "{:?}",
            released.run
        );
    } else {
        assert!(released.run.is_none());
    }
}

/// A `CatchUp` burst meets a full mailbox, and the tick it had no room for
/// panics when dropped (and, with `loud_factory`, so does the factory's
/// capture): each drops in its own catch, so the process does not abort,
/// and the timer ends with the tick's panic as the run's. Current-thread only, on a paused clock: the
/// timer falls behind by an advance of the clock between two of its polls.
async fn catch_up_without_room(loud_factory: bool) {
    let token_text = uuid::Uuid::new_v4().to_string();
    let ticks = Ticks {
        calls: Arc::default(),
        loud_drops: Arc::default(),
    };
    let (handle, mut rx) = handle_of::<LSink>();
    let factory = ticking(&ticks, Some(2), loud_factory, &token_text);
    let slot = PanicSlot::default();
    let task = tokio::spawn(recurring_loop(
        handle,
        RecurringId::new(0),
        factory,
        Instant::now() + Duration::from_millis(10),
        Duration::from_millis(10),
        MissPolicy::CatchUp,
        CancellationToken::new(),
        slot.clone(),
    ));
    // The timer waits for its first deadline, then the clock moves five
    // intervals past it before the timer runs again.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(50)).await;
    let released = tokio::time::timeout(Duration::from_secs(3600), task)
        .await
        .expect("the timer ends")
        .expect("no drop escaped its catch");
    assert!(
        take_parked(&slot).is_none(),
        "the factory was released, not left"
    );
    assert_eq!(
        ticks.calls.load(Ordering::SeqCst),
        2,
        "one tick sent, one burst tick"
    );
    assert_eq!(
        ticks.loud_drops.load(Ordering::SeqCst),
        1 + usize::from(loud_factory)
    );
    assert!(
        released
            .run
            .as_deref()
            .is_some_and(|p| p.contains("the tick panicked")),
        "{:?}",
        released.run
    );
    assert!(released.end.is_none());
    // The first tick was sent: it is the one message in the mailbox.
    assert!(matches!(
        rx.try_recv(),
        Ok(Envelope::Message {
            payload: LMsg::Tick(None),
            ..
        })
    ));
    assert!(rx.try_recv().is_err());
}

/// A one-shot timer whose cancel claimed it first never sends its message,
/// even with its deadline past and room in the mailbox: the message drops
/// once, in the forwarder's own poll, and the forwarder ends with nothing
/// released at the close.
async fn a_one_shot_whose_claim_was_lost_never_sends() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (handle, mut rx) = handle_of::<LSink>();
    let released = oneshot_forward(
        handle,
        LMsg::Quiet(QuietDrops(drops.clone())),
        Instant::now(),
        CancellationToken::new(),
        Arc::new(AtomicBool::new(true)),
    )
    .await;
    assert!(released.run.is_none() && released.end.is_none());
    assert!(rx.try_recv().is_err(), "the mailbox stays empty");
    assert_eq!(drops.load(Ordering::SeqCst), 1, "the message dropped once");
}

/// A context on a bare mailbox, for driving the end of its forwarders by
/// hand.
fn bare_context() -> (
    ActorContext<Sink>,
    crate::actor::mailbox::MailRx<ActorEnvelope<Sink>>,
) {
    let (handle, rx) = handle_of::<Sink>();
    let (status_tx, _status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let ctx = ActorContext::new(
        ActorId::from("forwarder-end"),
        handle,
        tokio::runtime::Handle::current(),
        status_tx,
        None,
        None,
        None,
        TurnClock::default(),
        SubtreeHold::new(None),
    );
    (ctx, rx)
}

#[cfg(feature = "tracing")]
mod captured {
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    /// Collects every WARN event as one line of `name=value` pairs.
    #[derive(Clone, Default)]
    pub(super) struct Warnings {
        lines: Arc<Mutex<Vec<String>>>,
    }

    impl Warnings {
        /// Makes this the subscriber of the calling thread until the guard
        /// drops.
        pub(super) fn install(&self) -> tracing::subscriber::DefaultGuard {
            crate::tracing_isolation::install(self.clone())
        }

        pub(super) fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }
    }

    struct Text(String);

    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    impl Subscriber for Warnings {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == Level::WARN
        }

        fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut text = Text(String::new());
            event.record(&mut text);
            self.lines.lock().unwrap().push(text.0);
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }
}

/// How a stream that ended during the run, with a panicking drop, has its
/// `Finished` end.
#[cfg(feature = "tracing")]
#[derive(Clone, Copy, Debug)]
enum FinishedEnds {
    /// The mailbox has room: `Finished` is delivered.
    Delivered,
    /// The mailbox is full and then closes: `Finished` is refused.
    Refused,
    /// The mailbox is full and the registration is cancelled.
    Cancelled,
    /// The mailbox is full and the forwarder is aborted before it reports.
    Aborted,
}

/// Runs the forwarder of a stream that ends during the run with a panicking
/// drop, ends its `Finished` as `how`, and returns what the forwarder
/// reported (nothing when it was aborted) with the lines it logged as a panic
/// that went unreported.
#[cfg(feature = "tracing")]
async fn an_ended_stream_whose_finished_ends(how: FinishedEnds) -> (Option<Released>, Vec<String>) {
    let warnings = captured::Warnings::default();
    let _installed = warnings.install();
    let dropped = Arc::new(AtomicBool::new(false));
    let (handle, mut rx) = match how {
        FinishedEnds::Delivered => handle_of::<Sink>(),
        _ => full::<Sink>(Msg::Data),
    };
    let token = CancellationToken::new();
    let task = tokio::spawn(stream_forward(
        Owned {
            bomb: Bomb(dropped.clone()),
            items: 0,
        },
        handle,
        token.clone(),
        PanicSlot::default(),
    ));
    let released = if let FinishedEnds::Delivered = how {
        Some(task.await.expect("the forwarder's own drops are caught"))
    } else {
        until_idle().await;
        assert!(
            dropped.load(Ordering::SeqCst),
            "{how:?}: the stream dropped"
        );
        assert!(!task.is_finished(), "{how:?}: Finished waits for room");
        match how {
            FinishedEnds::Refused => rx.close(),
            FinishedEnds::Cancelled => token.cancel(),
            _ => task.abort(),
        }
        match task.await {
            Ok(released) => Some(released),
            Err(err) => {
                assert!(err.is_cancelled(), "{how:?}: {err:?}");
                None
            }
        }
    };
    drop(rx);
    let lines = warnings
        .lines()
        .into_iter()
        .filter(|line| line.contains("ended before reporting"))
        .collect();
    (released, lines)
}
