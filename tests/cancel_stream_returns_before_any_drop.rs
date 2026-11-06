#![allow(dead_code)]
//! `cancel_stream` and `cancel_all_streams` drop nothing inside the call: an
//! item or a `Finished` that waits for room in a full mailbox, and the stream
//! itself, are dropped afterwards by the stream's forwarder.
//!
//! A drop is attributed to the call by a thread-local that the handler sets
//! around the call and clears right after it, so a forwarder dropping on
//! another worker thread at the same moment is not counted.

mod common;

use std::cell::RefCell;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorHandle, ActorResult, StreamEvent,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;

const BOUND: Duration = Duration::from_secs(20);

thread_local! {
    /// The count of the call in progress on this thread, if any.
    static IN_CALL: RefCell<Option<Arc<AtomicUsize>>> = const { RefCell::new(None) };
}

/// Counts a drop that happens while the current thread is inside the call.
fn note_drop() {
    let _ = IN_CALL.try_with(|call| {
        if let Some(count) = &*call.borrow() {
            count.fetch_add(1, Ordering::SeqCst);
        }
    });
}

/// An item that notes where it is dropped.
struct Item(u32);

impl Drop for Item {
    fn drop(&mut self) {
        note_drop();
    }
}

/// The number of `Finished` messages dropped, in the one test that ends its
/// stream.
static FINISHED_DROPS: AtomicUsize = AtomicUsize::new(0);

struct FinishedMessage;

impl Drop for FinishedMessage {
    fn drop(&mut self) {
        FINISHED_DROPS.fetch_add(1, Ordering::SeqCst);
        note_drop();
    }
}

enum Msg {
    Begin { stream: Probe, cancel_all: bool },
    Ping,
    Data(Item),
    Finished(FinishedMessage),
}

impl From<StreamEvent<Item>> for Msg {
    fn from(event: StreamEvent<Item>) -> Self {
        match event {
            StreamEvent::Data(item) => Msg::Data(item),
            StreamEvent::Finished => Msg::Finished(FinishedMessage),
        }
    }
}

/// A stream that says when it ended and when it was dropped, and notes where
/// it is dropped.
struct Probe {
    inner: ReceiverStream<Item>,
    ended: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Stream for Probe {
    type Item = Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Item>> {
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
        note_drop();
    }
}

struct Sink {
    gate: Arc<Notify>,
    entered: Arc<Notify>,
    seen: Arc<Mutex<Vec<u32>>>,
    in_call: Arc<AtomicUsize>,
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
                IN_CALL.with(|call| *call.borrow_mut() = Some(self.in_call.clone()));
                if cancel_all {
                    ctx.cancel_all_streams();
                } else {
                    ctx.cancel_stream(id).expect("the stream is live");
                }
                IN_CALL.with(|call| *call.borrow_mut() = None);
            }
            Msg::Ping | Msg::Finished(_) => {}
            Msg::Data(item) => self.seen.lock().unwrap().push(item.0),
        }
        Ok(())
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let waiting = async {
        while !done() {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    timeout(BOUND, waiting)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
}

struct Run {
    handle: ActorHandle<Sink>,
    gate: Arc<Notify>,
    seen: Arc<Mutex<Vec<u32>>>,
    in_call: Arc<AtomicUsize>,
    ended: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
    /// The sender, kept open unless the stream is to end.
    tx: Option<tokio::sync::mpsc::Sender<Item>>,
}

/// Starts a sink with a one-slot mailbox, holds its turn after it attached a
/// stream over `items`, and returns once the stream has filled the slot.
async fn start(items: &[u32], close_stream: bool, cancel_all: bool) -> Run {
    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let in_call = Arc::new(AtomicUsize::new(0));
    let handle = Sink {
        gate: gate.clone(),
        entered: entered.clone(),
        seen: seen.clone(),
        in_call: in_call.clone(),
    }
    .spawn()
    .with_config(ActorConfig::default().with_mailbox_capacity(1))
    .await
    .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    for item in items {
        tx.send(Item(*item)).await.unwrap();
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
    timeout(BOUND, entered.notified())
        .await
        .expect("the turn begins");
    Run {
        handle,
        gate,
        seen,
        in_call,
        ended,
        dropped,
        tx: kept,
    }
}

/// An item waits for room and the stream is still open: the cancel leaves
/// both to the forwarder.
async fn an_item_waits_for_room(cancel_all: bool) {
    let run = start(&[1, 2, 3], false, cancel_all).await;
    // Item 1 holds the slot; item 2 is taken from the channel and waits for
    // room; item 3 is still in the channel.
    let tx = run.tx.clone().expect("the stream is open");
    until("item 2 waits for room", || {
        tx.capacity() == 7 && run.handle.mailbox_len() == 1
    })
    .await;
    run.gate.notify_one();
    until("the forwarder dropped the stream", || {
        run.dropped.load(Ordering::SeqCst)
    })
    .await;
    timeout(BOUND, run.handle.send(Msg::Ping))
        .await
        .expect("the actor answers")
        .unwrap();
    assert_eq!(*run.seen.lock().unwrap(), vec![1]);
    assert_eq!(
        run.in_call.load(Ordering::SeqCst),
        0,
        "the call dropped an item or the stream"
    );
}

#[test]
fn cancel_all_streams_drops_neither_a_waiting_item_nor_the_stream() {
    common::run(4, an_item_waits_for_room(true));
}
