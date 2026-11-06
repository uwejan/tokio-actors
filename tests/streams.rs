#![allow(dead_code)]

use tokio::time::{sleep, Duration, Instant};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason, StreamEvent,
};
use tokio_stream::wrappers::ReceiverStream;

/// The bound on every outcome a test awaits.
const BOUND: Duration = Duration::from_secs(10);

/// Asks `done` until it answers true or `BOUND` passes; the test's own
/// assertions then decide, so a state that never arrives fails there with
/// its full contents.
async fn settle<F, Fut>(mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + BOUND;
    while !done().await && Instant::now() < deadline {
        sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cancel_stream() {
    struct CancelActor {
        rx: Option<tokio::sync::mpsc::Receiver<i32>>,
        stream_id: Option<tokio_actors::StreamId>,
        items: Vec<i32>,
        finished: bool,
    }

    enum Msg {
        Stream(StreamEvent<i32>),
        DoCancel,
        GetState,
    }

    impl From<StreamEvent<i32>> for Msg {
        fn from(ev: StreamEvent<i32>) -> Self {
            Msg::Stream(ev)
        }
    }

    #[derive(Debug)]
    enum Resp {
        Ack,
        State(Vec<i32>, bool),
    }

    impl Actor for CancelActor {
        type Message = Msg;
        type Response = Resp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if let Some(rx) = self.rx.take() {
                let id = ctx.add_stream(ReceiverStream::new(rx));
                assert_eq!(ctx.active_stream_count(), 1);
                self.stream_id = Some(id);
            }
            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<Self::Response> {
            match msg {
                Msg::Stream(StreamEvent::Data(v)) => {
                    self.items.push(v);
                    Ok(Resp::Ack)
                }
                Msg::Stream(StreamEvent::Finished) => {
                    self.finished = true;
                    Ok(Resp::Ack)
                }
                Msg::DoCancel => {
                    if let Some(id) = self.stream_id.take() {
                        ctx.cancel_stream(id).unwrap();
                    }
                    Ok(Resp::Ack)
                }
                Msg::GetState => Ok(Resp::State(self.items.clone(), self.finished)),
            }
        }
    }

    let (tx, rx) = tokio::sync::mpsc::channel(16);

    let handle = CancelActor {
        rx: Some(rx),
        stream_id: None,
        items: Vec::new(),
        finished: false,
    }
    .spawn()
    .named("cancel-stream")
    .with_config(ActorConfig::default())
    .await
    .unwrap();

    // Send some items first - these should arrive
    tx.send(1).await.unwrap();
    tx.send(2).await.unwrap();
    let h = &handle;
    settle(|| async move {
        matches!(h.send(Msg::GetState).await, Ok(Resp::State(items, false)) if items == vec![1, 2])
    })
    .await;

    // Now cancel the stream; the reply comes once the cancel is done.
    assert!(matches!(handle.send(Msg::DoCancel).await, Ok(Resp::Ack)));

    // Send more items - these should NOT arrive
    let _ = tx.send(3).await;
    let _ = tx.send(4).await;

    sleep(Duration::from_millis(50)).await;

    if let Resp::State(items, finished) = handle.send(Msg::GetState).await.unwrap() {
        assert_eq!(items, vec![1, 2], "Only pre-cancel items should arrive");
        assert!(!finished, "No Finished should arrive after cancel");
    }

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multiple_streams() {
    struct DualCollector {
        rx1: Option<tokio::sync::mpsc::Receiver<i32>>,
        rx2: Option<tokio::sync::mpsc::Receiver<i32>>,
        items: Vec<i32>,
        finish_count: u32,
    }

    enum Msg {
        Stream(StreamEvent<i32>),
        GetState,
    }

    impl From<StreamEvent<i32>> for Msg {
        fn from(ev: StreamEvent<i32>) -> Self {
            Msg::Stream(ev)
        }
    }

    enum Resp {
        Ack,
        State(Vec<i32>, u32),
    }

    impl Actor for DualCollector {
        type Message = Msg;
        type Response = Resp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if let Some(rx) = self.rx1.take() {
                ctx.add_stream(ReceiverStream::new(rx));
            }
            if let Some(rx) = self.rx2.take() {
                ctx.add_stream(ReceiverStream::new(rx));
            }
            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<Self::Response> {
            match msg {
                Msg::Stream(StreamEvent::Data(v)) => {
                    self.items.push(v);
                    Ok(Resp::Ack)
                }
                Msg::Stream(StreamEvent::Finished) => {
                    self.finish_count += 1;
                    Ok(Resp::Ack)
                }
                Msg::GetState => Ok(Resp::State(self.items.clone(), self.finish_count)),
            }
        }
    }

    let (tx1, rx1) = tokio::sync::mpsc::channel(16);
    let (tx2, rx2) = tokio::sync::mpsc::channel(16);

    let handle = DualCollector {
        rx1: Some(rx1),
        rx2: Some(rx2),
        items: Vec::new(),
        finish_count: 0,
    }
    .spawn()
    .named("dual")
    .with_config(ActorConfig::default())
    .await
    .unwrap();

    tx1.send(10).await.unwrap();
    tx2.send(20).await.unwrap();
    tx1.send(30).await.unwrap();
    tx2.send(40).await.unwrap();
    drop(tx1);
    drop(tx2);

    let h = &handle;
    settle(|| async move {
        matches!(h.send(Msg::GetState).await, Ok(Resp::State(items, 2)) if items.len() == 4)
    })
    .await;

    if let Resp::State(mut items, finish_count) = handle.send(Msg::GetState).await.unwrap() {
        items.sort();
        assert_eq!(items, vec![10, 20, 30, 40]);
        assert_eq!(finish_count, 2, "Both streams should send Finished");
    }

    handle.stop(StopReason::Graceful).await.unwrap();
}

/// A stream that ends at once and panics when dropped.
struct EndsAndPanicsWhenDropped;

impl futures_core::Stream for EndsAndPanicsWhenDropped {
    type Item = ();
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<()>> {
        std::task::Poll::Ready(None)
    }
}

impl Drop for EndsAndPanicsWhenDropped {
    fn drop(&mut self) {
        panic!("the ended stream panicked when dropped");
    }
}

struct ReadsItsForwarderError;

enum ErrMsg {
    Stream,
    Read,
}

impl From<StreamEvent<()>> for ErrMsg {
    fn from(_: StreamEvent<()>) -> Self {
        ErrMsg::Stream
    }
}

impl Actor for ReadsItsForwarderError {
    type Message = ErrMsg;
    type Response = Option<String>;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.add_stream(EndsAndPanicsWhenDropped);
        Ok(())
    }

    async fn handle(
        &mut self,
        msg: ErrMsg,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Option<String>> {
        Ok(match msg {
            ErrMsg::Stream => None,
            ErrMsg::Read => ctx.last_forwarder_error().map(|err| err.to_string()),
        })
    }
}

/// A stream that ends during the run and panics when dropped: the panic is
/// the actor's last forwarder error, read by a later message, and the actor
/// keeps running.
async fn an_ended_stream_s_drop_panic_is_the_last_forwarder_error() {
    let handle = ReadsItsForwarderError.spawn().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let recorded = loop {
        if let Some(text) = handle.send(ErrMsg::Read).await.unwrap() {
            break text;
        }
        assert!(Instant::now() < deadline, "the drop's panic is recorded");
        sleep(Duration::from_millis(1)).await;
    };
    assert!(
        recorded.contains("the ended stream panicked when dropped"),
        "{recorded}"
    );
    assert!(
        handle.is_alive(),
        "a forwarder's panic never stops the actor"
    );
    handle.stop(StopReason::Graceful).await.unwrap();
    handle.wait_stopped().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_stream_s_drop_panic_is_the_last_forwarder_error_multi_thread() {
    an_ended_stream_s_drop_panic_is_the_last_forwarder_error().await;
}
