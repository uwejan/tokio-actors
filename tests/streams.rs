#![allow(dead_code)]

use tokio::time::{sleep, Duration};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason, StreamEvent,
};
use tokio_stream::wrappers::ReceiverStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_infallible_stream() {
    struct StreamCollector {
        rx: Option<tokio::sync::mpsc::Receiver<i32>>,
        items: Vec<i32>,
        finished: bool,
    }

    enum Msg {
        Stream(StreamEvent<i32>),
        GetItems,
    }

    impl From<StreamEvent<i32>> for Msg {
        fn from(ev: StreamEvent<i32>) -> Self {
            Msg::Stream(ev)
        }
    }

    enum Resp {
        Ack,
        Items(Vec<i32>, bool),
    }

    impl Actor for StreamCollector {
        type Message = Msg;
        type Response = Resp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if let Some(rx) = self.rx.take() {
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
                    self.finished = true;
                    Ok(Resp::Ack)
                }
                Msg::GetItems => Ok(Resp::Items(self.items.clone(), self.finished)),
            }
        }
    }

    let (tx2, rx2) = tokio::sync::mpsc::channel(16);

    let handle = StreamCollector {
        rx: Some(rx2),
        items: Vec::new(),
        finished: false,
    }
    .spawn()
    .named("stream-collector")
    .with_config(ActorConfig::default())
    .await
    .unwrap();

    // Send items through the channel
    for i in 1..=5 {
        tx2.send(i).await.unwrap();
    }
    drop(tx2); // close channel -> stream yields None -> Finished

    sleep(Duration::from_millis(50)).await;

    if let Resp::Items(items, finished) = handle.send(Msg::GetItems).await.unwrap() {
        assert_eq!(items, vec![1, 2, 3, 4, 5]);
        assert!(finished, "Should have received Finished");
    } else {
        panic!("Expected Items response");
    }

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fallible_stream() {
    struct FallibleCollector {
        rx: Option<tokio::sync::mpsc::Receiver<Result<String, String>>>,
        oks: Vec<String>,
        errs: Vec<String>,
        finished: bool,
    }

    enum Msg {
        Stream(StreamEvent<Result<String, String>>),
        GetResults,
    }

    impl From<StreamEvent<Result<String, String>>> for Msg {
        fn from(ev: StreamEvent<Result<String, String>>) -> Self {
            Msg::Stream(ev)
        }
    }

    enum Resp {
        Ack,
        Results(Vec<String>, Vec<String>, bool),
    }

    impl Actor for FallibleCollector {
        type Message = Msg;
        type Response = Resp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if let Some(rx) = self.rx.take() {
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
                Msg::Stream(StreamEvent::Data(Ok(v))) => {
                    self.oks.push(v);
                    Ok(Resp::Ack)
                }
                Msg::Stream(StreamEvent::Data(Err(e))) => {
                    self.errs.push(e);
                    Ok(Resp::Ack)
                }
                Msg::Stream(StreamEvent::Finished) => {
                    self.finished = true;
                    Ok(Resp::Ack)
                }
                Msg::GetResults => Ok(Resp::Results(
                    self.oks.clone(),
                    self.errs.clone(),
                    self.finished,
                )),
            }
        }
    }

    let (tx, rx) = tokio::sync::mpsc::channel(16);

    let handle = FallibleCollector {
        rx: Some(rx),
        oks: Vec::new(),
        errs: Vec::new(),
        finished: false,
    }
    .spawn()
    .named("fallible")
    .with_config(ActorConfig::default())
    .await
    .unwrap();

    tx.send(Ok("hello".into())).await.unwrap();
    tx.send(Err("oops".into())).await.unwrap();
    tx.send(Ok("world".into())).await.unwrap();
    drop(tx);

    sleep(Duration::from_millis(50)).await;

    if let Resp::Results(oks, errs, finished) = handle.send(Msg::GetResults).await.unwrap() {
        assert_eq!(oks, vec!["hello", "world"]);
        assert_eq!(errs, vec!["oops"]);
        assert!(finished);
    } else {
        panic!("Expected Results response");
    }

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_stream_finished_on_drop() {
    struct DropCollector {
        rx: Option<tokio::sync::mpsc::Receiver<i32>>,
        finished: bool,
    }

    enum Msg {
        Stream(StreamEvent<i32>),
        Check,
    }

    impl From<StreamEvent<i32>> for Msg {
        fn from(ev: StreamEvent<i32>) -> Self {
            Msg::Stream(ev)
        }
    }

    enum Resp {
        Ack,
        Finished(bool),
    }

    impl Actor for DropCollector {
        type Message = Msg;
        type Response = Resp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if let Some(rx) = self.rx.take() {
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
                Msg::Stream(StreamEvent::Data(_)) => Ok(Resp::Ack),
                Msg::Stream(StreamEvent::Finished) => {
                    self.finished = true;
                    Ok(Resp::Ack)
                }
                Msg::Check => Ok(Resp::Finished(self.finished)),
            }
        }
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<i32>(16);

    let handle = DropCollector {
        rx: Some(rx),
        finished: false,
    }
    .spawn()
    .named("drop-test")
    .with_config(ActorConfig::default())
    .await
    .unwrap();

    // Drop sender immediately -- stream should yield None -> Finished
    drop(tx);

    sleep(Duration::from_millis(50)).await;

    if let Resp::Finished(finished) = handle.send(Msg::Check).await.unwrap() {
        assert!(finished, "Should receive Finished when sender is dropped");
    }

    handle.stop(StopReason::Graceful).await.unwrap();
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

    // Send some items first -- these should arrive
    tx.send(1).await.unwrap();
    tx.send(2).await.unwrap();
    sleep(Duration::from_millis(30)).await;

    // Now cancel the stream
    handle.notify(Msg::DoCancel).await.unwrap();
    sleep(Duration::from_millis(20)).await;

    // Send more items -- these should NOT arrive
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
async fn test_cancel_all_streams() {
    struct MultiCancelActor {
        rx1: Option<tokio::sync::mpsc::Receiver<i32>>,
        rx2: Option<tokio::sync::mpsc::Receiver<i32>>,
        pre_cancel_count: u32,
        post_cancel_count: u32,
        cancelling: bool,
    }

    enum Msg {
        Stream(StreamEvent<i32>),
        DoCancelAll,
        GetState,
    }

    impl From<StreamEvent<i32>> for Msg {
        fn from(ev: StreamEvent<i32>) -> Self {
            Msg::Stream(ev)
        }
    }

    enum Resp {
        Ack,
        State(u32, u32),
    }

    impl Actor for MultiCancelActor {
        type Message = Msg;
        type Response = Resp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if let Some(rx) = self.rx1.take() {
                ctx.add_stream(ReceiverStream::new(rx));
            }
            if let Some(rx) = self.rx2.take() {
                ctx.add_stream(ReceiverStream::new(rx));
            }
            assert_eq!(ctx.active_stream_count(), 2);
            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<Self::Response> {
            match msg {
                Msg::Stream(StreamEvent::Data(_)) => {
                    if self.cancelling {
                        self.post_cancel_count += 1;
                    } else {
                        self.pre_cancel_count += 1;
                    }
                    Ok(Resp::Ack)
                }
                Msg::Stream(StreamEvent::Finished) => Ok(Resp::Ack),
                Msg::DoCancelAll => {
                    self.cancelling = true;
                    ctx.cancel_all_streams();
                    assert_eq!(ctx.active_stream_count(), 0);
                    Ok(Resp::Ack)
                }
                Msg::GetState => Ok(Resp::State(self.pre_cancel_count, self.post_cancel_count)),
            }
        }
    }

    let (tx1, rx1) = tokio::sync::mpsc::channel(16);
    let (tx2, rx2) = tokio::sync::mpsc::channel(16);

    let handle = MultiCancelActor {
        rx1: Some(rx1),
        rx2: Some(rx2),
        pre_cancel_count: 0,
        post_cancel_count: 0,
        cancelling: false,
    }
    .spawn()
    .named("multi-cancel")
    .with_config(ActorConfig::default())
    .await
    .unwrap();

    // Send items -- these arrive before cancel
    tx1.send(1).await.unwrap();
    tx2.send(2).await.unwrap();
    sleep(Duration::from_millis(30)).await;

    // Cancel all streams
    handle.notify(Msg::DoCancelAll).await.unwrap();
    sleep(Duration::from_millis(20)).await;

    // Send more items -- should NOT arrive
    let _ = tx1.send(3).await;
    let _ = tx2.send(4).await;
    sleep(Duration::from_millis(50)).await;

    if let Resp::State(pre, post) = handle.send(Msg::GetState).await.unwrap() {
        assert_eq!(pre, 2, "Should have received 2 items before cancel");
        assert_eq!(post, 0, "No items should arrive after cancel_all_streams");
    }

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_active_stream_count() {
    struct CountActor {
        rx1: Option<tokio::sync::mpsc::Receiver<i32>>,
        rx2: Option<tokio::sync::mpsc::Receiver<i32>>,
        rx3: Option<tokio::sync::mpsc::Receiver<i32>>,
    }

    enum Msg {
        Stream(StreamEvent<i32>),
    }

    impl From<StreamEvent<i32>> for Msg {
        fn from(ev: StreamEvent<i32>) -> Self {
            Msg::Stream(ev)
        }
    }

    enum Resp {
        Ack,
    }

    impl Actor for CountActor {
        type Message = Msg;
        type Response = Resp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            assert_eq!(ctx.active_stream_count(), 0);

            if let Some(rx) = self.rx1.take() {
                ctx.add_stream(ReceiverStream::new(rx));
            }
            assert_eq!(ctx.active_stream_count(), 1);

            if let Some(rx) = self.rx2.take() {
                ctx.add_stream(ReceiverStream::new(rx));
            }
            assert_eq!(ctx.active_stream_count(), 2);

            if let Some(rx) = self.rx3.take() {
                let id = ctx.add_stream(ReceiverStream::new(rx));
                assert_eq!(ctx.active_stream_count(), 3);
                ctx.cancel_stream(id).unwrap();
                assert_eq!(ctx.active_stream_count(), 2);
            }

            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<Self::Response> {
            match msg {
                Msg::Stream(_) => Ok(Resp::Ack),
            }
        }
    }

    let (_tx1, rx1) = tokio::sync::mpsc::channel(16);
    let (_tx2, rx2) = tokio::sync::mpsc::channel(16);
    let (_tx3, rx3) = tokio::sync::mpsc::channel(16);

    let handle = CountActor {
        rx1: Some(rx1),
        rx2: Some(rx2),
        rx3: Some(rx3),
    }
    .spawn()
    .named("count")
    .with_config(ActorConfig::default())
    .await
    .unwrap();

    sleep(Duration::from_millis(50)).await;
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

    sleep(Duration::from_millis(50)).await;

    if let Resp::State(mut items, finish_count) = handle.send(Msg::GetState).await.unwrap() {
        items.sort();
        assert_eq!(items, vec![10, 20, 30, 40]);
        assert_eq!(finish_count, 2, "Both streams should send Finished");
    }

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_stream_cancelled_on_actor_stop() {
    struct StopActor {
        rx: Option<tokio::sync::mpsc::Receiver<i32>>,
    }

    enum Msg {
        Stream(StreamEvent<i32>),
    }

    impl From<StreamEvent<i32>> for Msg {
        fn from(ev: StreamEvent<i32>) -> Self {
            Msg::Stream(ev)
        }
    }

    enum Resp {
        Ack,
    }

    impl Actor for StopActor {
        type Message = Msg;
        type Response = Resp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if let Some(rx) = self.rx.take() {
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
                Msg::Stream(_) => Ok(Resp::Ack),
            }
        }
    }

    let (_tx, rx) = tokio::sync::mpsc::channel(16);

    let handle = StopActor { rx: Some(rx) }
        .spawn()
        .named("stop-test")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    // Give actor time to attach stream
    sleep(Duration::from_millis(20)).await;

    // Stop the actor -- stream forwarding task should be cancelled via Drop
    handle.stop(StopReason::Graceful).await.unwrap();

    // Try to send through channel -- should fail because receiver side is dropped
    // or the forwarding task exited (either way, no one is consuming).
    sleep(Duration::from_millis(20)).await;

    // The channel send may succeed if the mpsc buffer isn't full, but the forwarding
    // task should have stopped. We verify by checking the actor actually stopped.
    assert!(
        handle
            .send(Msg::Stream(StreamEvent::Data(1)))
            .await
            .is_err(),
        "Actor should be stopped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_empty_stream() {
    struct EmptyActor {
        finished: bool,
    }

    enum Msg {
        Stream(StreamEvent<i32>),
        Check,
    }

    impl From<StreamEvent<i32>> for Msg {
        fn from(ev: StreamEvent<i32>) -> Self {
            Msg::Stream(ev)
        }
    }

    enum Resp {
        Ack,
        Finished(bool),
    }

    impl Actor for EmptyActor {
        type Message = Msg;
        type Response = Resp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            ctx.add_stream(tokio_stream::empty::<i32>());
            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<Self::Response> {
            match msg {
                Msg::Stream(StreamEvent::Data(_)) => {
                    panic!("Empty stream should not yield data");
                }
                Msg::Stream(StreamEvent::Finished) => {
                    self.finished = true;
                    Ok(Resp::Ack)
                }
                Msg::Check => Ok(Resp::Finished(self.finished)),
            }
        }
    }

    let handle = EmptyActor { finished: false }
        .spawn()
        .named("empty")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    sleep(Duration::from_millis(50)).await;

    if let Resp::Finished(finished) = handle.send(Msg::Check).await.unwrap() {
        assert!(finished, "Empty stream should send Finished immediately");
    }

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cancel_nonexistent_stream() {
    struct NoopActor;

    enum Msg {
        Stream(StreamEvent<i32>),
    }

    impl From<StreamEvent<i32>> for Msg {
        fn from(ev: StreamEvent<i32>) -> Self {
            Msg::Stream(ev)
        }
    }

    enum Resp {
        Ack,
    }

    impl Actor for NoopActor {
        type Message = Msg;
        type Response = Resp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            // Try to cancel a stream ID that doesn't exist
            let fake_id = ctx.add_stream(tokio_stream::empty::<i32>());
            ctx.cancel_stream(fake_id).unwrap(); // cancel the real one first
            let result = ctx.cancel_stream(fake_id); // now it should fail
            assert!(result.is_err());
            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<Self::Response> {
            match msg {
                Msg::Stream(_) => Ok(Resp::Ack),
            }
        }
    }

    let handle = NoopActor
        .spawn()
        .named("noop")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    sleep(Duration::from_millis(50)).await;
    handle.stop(StopReason::Graceful).await.unwrap();
}
