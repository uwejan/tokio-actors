//! A child placed on another runtime with `on_runtime` runs its timers and
//! its streams there too: the forwarder of a recurring timer and the forwarder
//! of a stream both run on the placed runtime, for the first incarnation and
//! for the incarnation a crash restart starts.

mod common;

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use tokio::runtime::{Builder, Handle, Id, Runtime};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_actors::{
    actor::context::ActorContext, Actor, ActorExt, ActorResult, ActorSystem, RestartType,
    StopReason, StreamEvent, SupervisionConfig,
};

/// The longest a wait for an event may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

fn unique(base: &str) -> String {
    format!(
        "placed-forwarders-{base}-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

/// What a worker reports: the incarnation it belongs to, which forwarder or
/// callback ran, and the runtime it ran on.
type Report = (usize, &'static str, Id);

enum Msg {
    Tick,
    Stream(StreamEvent<u8>),
    Crash,
}

impl From<StreamEvent<u8>> for Msg {
    fn from(event: StreamEvent<u8>) -> Self {
        Msg::Stream(event)
    }
}

/// A stream that reports the runtime each poll runs on and then ends.
struct ReportsItsPoll {
    incarnation: usize,
    reports: UnboundedSender<Report>,
}

impl Stream for ReportsItsPoll {
    type Item = u8;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<u8>> {
        let _ = self
            .reports
            .send((self.incarnation, "stream", Handle::current().id()));
        Poll::Ready(None)
    }
}

struct Worker {
    incarnation: usize,
    reports: UnboundedSender<Report>,
}

impl Actor for Worker {
    type Message = Msg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _ = self
            .reports
            .send((self.incarnation, "started", Handle::current().id()));
        let incarnation = self.incarnation;
        let reports = self.reports.clone();
        ctx.every_with(Duration::from_millis(5), move || {
            let _ = reports.send((incarnation, "timer", Handle::current().id()));
            Msg::Tick
        })
        .await?;
        ctx.add_stream(ReportsItsPoll {
            incarnation: self.incarnation,
            reports: self.reports.clone(),
        });
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Crash => panic!("worker crash"),
            Msg::Tick | Msg::Stream(StreamEvent::Data(_) | StreamEvent::Finished) => Ok(()),
        }
    }
}

/// Starts one placed child when it starts.
struct Supervisor {
    placed: Handle,
    name: String,
    reports: UnboundedSender<Report>,
}

impl Actor for Supervisor {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let built = Arc::new(AtomicUsize::new(0));
        let reports = self.reports.clone();
        ctx.spawn_child(move || Worker {
            incarnation: built.fetch_add(1, Ordering::SeqCst),
            reports: reports.clone(),
        })
        .named(self.name.clone())
        .restart_type(RestartType::Permanent)
        .on_runtime(&self.placed)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Reads reports until the incarnation has reported both a timer tick and a
/// stream poll, and returns every report read so far.
async fn until_a_tick_and_a_poll(
    reports: &mut UnboundedReceiver<Report>,
    incarnation: usize,
    seen: &mut Vec<Report>,
) {
    let wanted = |seen: &[Report], kind: &str| {
        seen.iter()
            .any(|(n, tag, _)| *n == incarnation && *tag == kind)
    };
    while !(wanted(seen, "timer") && wanted(seen, "stream")) {
        let report = tokio::time::timeout(HANG_BOUND, reports.recv())
            .await
            .expect("the incarnation reports its timer and its stream")
            .expect("the reports stay open");
        seen.push(report);
    }
}

fn end_runtime(runtime: Runtime) {
    std::thread::spawn(move || drop(runtime)).join().unwrap();
}

#[test]
fn the_timers_and_streams_of_a_placed_child_run_on_the_placed_runtime_in_every_incarnation() {
    let placed = Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("placed")
        .enable_all()
        .build()
        .unwrap();
    let placed_id = placed.handle().id();
    let placed_handle = placed.handle().clone();
    common::run(2, async move {
        let supervisor_runtime = Handle::current().id();
        assert_ne!(placed_id, supervisor_runtime);
        let (tx, mut reports) = unbounded_channel::<Report>();
        let name = unique("child");
        let sup = Supervisor {
            placed: placed_handle,
            name: name.clone(),
            reports: tx,
        }
        .spawn()
        .named(unique("supervisor"))
        .with_supervision(SupervisionConfig::one_for_one())
        .await
        .unwrap();

        let mut seen = Vec::new();
        until_a_tick_and_a_poll(&mut reports, 0, &mut seen).await;

        // A crash restart starts the second incarnation on the placed runtime,
        // and its timer and stream run there as well.
        let _ = ActorSystem::default()
            .get::<Worker>(&name)
            .expect("the child is registered")
            .send(Msg::Crash)
            .await;
        until_a_tick_and_a_poll(&mut reports, 1, &mut seen).await;

        for (incarnation, kind, runtime) in &seen {
            assert_eq!(
                *runtime, placed_id,
                "the {kind} of incarnation {incarnation} ran on the placed runtime"
            );
        }
        let _ = sup.stop(StopReason::Graceful).await;
        tokio::time::timeout(HANG_BOUND, sup.wait_stopped())
            .await
            .expect("the supervisor ends");
    });
    end_runtime(placed);
}
