//! A cancelled stream delivers nothing more: an item, or the `Finished`, that
//! waits for room in a full mailbox when the stream is cancelled is dropped,
//! and so is the stream.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::Notify;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorHandle, ActorResult, StreamEvent,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;

const BOUND: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Data(u32),
    Finished,
}

/// How many `Finished` messages have left the system, handled by the actor or
/// dropped unsent by a forwarder. Only the test that closes its stream makes
/// one.
static FINISHED_GONE: AtomicUsize = AtomicUsize::new(0);

/// Counts the `Finished` message it travels with when that message is dropped.
struct Departed {
    finished: bool,
}

impl Drop for Departed {
    fn drop(&mut self) {
        if self.finished {
            FINISHED_GONE.fetch_add(1, Ordering::SeqCst);
        }
    }
}

enum Msg {
    Begin {
        stream: Probe,
        cancel_all: bool,
    },
    Ping,
    /// The event and the counter that is held for its drop.
    Stream(StreamEvent<u32>, #[allow(dead_code)] Departed),
}

impl From<StreamEvent<u32>> for Msg {
    fn from(event: StreamEvent<u32>) -> Self {
        let finished = matches!(event, StreamEvent::Finished);
        Msg::Stream(event, Departed { finished })
    }
}

/// A stream that says when it ended and when it was dropped.
struct Probe {
    inner: ReceiverStream<u32>,
    ended: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Stream for Probe {
    type Item = u32;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<u32>> {
        let polled = Pin::new(&mut self.inner).poll_next(cx);
        if let Poll::Ready(None) = polled {
            self.ended.store(true, Ordering::SeqCst);
        }
        polled
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

struct Sink {
    gate: Arc<Notify>,
    entered: Arc<Notify>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Actor for Sink {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Begin { stream, cancel_all } => {
                let id = ctx.add_stream(stream);
                // The turn is held: the forwarder fills the single slot of the
                // mailbox and waits for room.
                self.entered.notify_one();
                self.gate.notified().await;
                if cancel_all {
                    ctx.cancel_all_streams();
                } else {
                    ctx.cancel_stream(id).expect("the stream is live");
                }
            }
            Msg::Ping => {}
            Msg::Stream(StreamEvent::Data(v), _) => self.seen.lock().unwrap().push(Seen::Data(v)),
            Msg::Stream(StreamEvent::Finished, _) => self.seen.lock().unwrap().push(Seen::Finished),
        }
        Ok(())
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + BOUND;
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

struct Run {
    handle: ActorHandle<Sink>,
    gate: Arc<Notify>,
    seen: Arc<Mutex<Vec<Seen>>>,
    ended: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
    /// The sender, kept open unless the stream is to end.
    tx: Option<tokio::sync::mpsc::Sender<u32>>,
}

/// Starts a sink with a one-slot mailbox, holds its turn after it attached a
/// stream over `items`, and returns once the stream has filled the slot.
async fn start(items: &[u32], close_stream: bool, cancel_all: bool) -> Run {
    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handle = Sink {
        gate: gate.clone(),
        entered: entered.clone(),
        seen: seen.clone(),
    }
    .spawn()
    .with_config(ActorConfig::default().with_mailbox_capacity(1))
    .await
    .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    for item in items {
        tx.send(*item).await.unwrap();
    }
    let ended = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let kept = if close_stream {
        drop(tx);
        None
    } else {
        Some(tx)
    };
    handle
        .notify(Msg::Begin {
            stream: Probe {
                inner: ReceiverStream::new(rx),
                ended: ended.clone(),
                dropped: dropped.clone(),
            },
            cancel_all,
        })
        .await
        .unwrap();
    entered.notified().await;
    Run {
        handle,
        gate,
        seen,
        ended,
        dropped,
        tx: kept,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_item_waiting_for_room_is_dropped_by_cancel_stream() {
    let run = start(&[1, 2, 3], false, false).await;
    // Item 1 holds the slot; item 2 is taken from the channel and waits for
    // room; item 3 is still in the channel.
    let tx = run.tx.clone().expect("the stream is open");
    until("item 2 waits for room", || {
        tx.capacity() == 7 && run.handle.mailbox_len() == 1
    })
    .await;
    run.gate.notify_one();
    until("the stream is dropped", || {
        run.dropped.load(Ordering::SeqCst)
    })
    .await;
    run.handle.send(Msg::Ping).await.unwrap();
    assert_eq!(*run.seen.lock().unwrap(), vec![Seen::Data(1)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_item_waiting_for_room_is_dropped_by_cancel_all_streams() {
    let run = start(&[1, 2, 3], false, true).await;
    let tx = run.tx.clone().expect("the stream is open");
    until("item 2 waits for room", || {
        tx.capacity() == 7 && run.handle.mailbox_len() == 1
    })
    .await;
    run.gate.notify_one();
    until("the stream is dropped", || {
        run.dropped.load(Ordering::SeqCst)
    })
    .await;
    run.handle.send(Msg::Ping).await.unwrap();
    assert_eq!(*run.seen.lock().unwrap(), vec![Seen::Data(1)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_waiting_for_room_is_dropped_by_cancel_stream() {
    let run = start(&[1], true, false).await;
    // The stream ended and was dropped; its `Finished` waits for room behind
    // item 1.
    until("the stream ended and was dropped", || {
        run.ended.load(Ordering::SeqCst) && run.dropped.load(Ordering::SeqCst)
    })
    .await;
    run.gate.notify_one();
    // The `Finished` leaves the system either unsent, dropped by its
    // forwarder, or delivered and handled by the actor; the actor has handled
    // it by the time its message is gone.
    until("the Finished is gone", || {
        FINISHED_GONE.load(Ordering::SeqCst) == 1
    })
    .await;
    run.handle.send(Msg::Ping).await.unwrap();
    assert_eq!(*run.seen.lock().unwrap(), vec![Seen::Data(1)]);
}
