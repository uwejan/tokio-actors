#![allow(dead_code)]
//! A monitor's forwarder task holds a slot of the watcher's priority lane,
//! and drops it when the monitor is revoked or fires. Freeing the slot wakes
//! the sender parked behind it, and that waker belongs to another task: a
//! panic of it stays in the drop, the watcher goes on, and with the `tracing`
//! feature the panic is logged under the watcher's id. A closure of the
//! watcher's `map` that panics when it is dropped has one outcome too,
//! whichever of the entry and the forwarder task drops it last. The same holds
//! for the public priority sends: a `notify_priority`, `send_priority` or
//! `send_priority_timeout` that is cancelled while it holds a granted slot
//! wakes the sender parked behind it inside the catch, and one that times out
//! while parked leaves the sender behind it its place.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorHandle, ActorResult, Down, MonitorId, SendError, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("mwake-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

/// A closure's captured value that panics when it is dropped while armed.
struct Loud(Arc<AtomicBool>);

impl Drop for Loud {
    fn drop(&mut self) {
        if self.0.load(Ordering::SeqCst) {
            panic!("a captured value of the map panicked when dropped");
        }
    }
}

/// Parks the closure of a monitor inside the forwarder's map call until the
/// test lets it go.
struct MapGate {
    entered: Mutex<std::sync::mpsc::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

enum Cmd {
    /// A priority monitor on the target.
    Watch(ActorHandle<Victim>),
    /// A priority monitor whose map call waits on the gate.
    WatchGated(ActorHandle<Victim>, Arc<MapGate>),
    /// A priority monitor whose closure captures a value that panics when
    /// dropped once armed.
    WatchLoud(ActorHandle<Victim>, Arc<AtomicBool>),
    Revoke(MonitorId),
    /// Signals that the handler began, then waits for the gate.
    Hold(Arc<Semaphore>),
    Ping,
    /// Whether a forwarder task of the watcher ended in a panic.
    ForwarderPanicked,
    Gone(Down),
}

#[derive(Debug, PartialEq)]
enum Out {
    Id(MonitorId),
    Flag(bool),
    Unit,
}

struct Watcher {
    began: mpsc::UnboundedSender<()>,
}

impl Actor for Watcher {
    type Message = Cmd;
    type Response = Out;

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Out> {
        Ok(match msg {
            Cmd::Watch(target) => {
                Out::Id(ctx.monitor_priority(&target, Cmd::Gone).expect("running"))
            }
            Cmd::WatchGated(target, gate) => Out::Id(
                ctx.monitor_priority(&target, move |down| {
                    let _ = gate.entered.lock().unwrap().send(());
                    let _ = gate.release.lock().unwrap().recv();
                    Cmd::Gone(down)
                })
                .expect("running"),
            ),
            Cmd::WatchLoud(target, armed) => {
                let loud = Loud(armed);
                Out::Id(
                    ctx.monitor_priority(&target, move |down| {
                        let _keep = &loud;
                        Cmd::Gone(down)
                    })
                    .expect("running"),
                )
            }
            Cmd::Revoke(id) => Out::Flag(ctx.demonitor(id)),
            Cmd::Hold(gate) => {
                let _ = self.began.send(());
                let _ = gate.acquire().await;
                Out::Unit
            }
            Cmd::ForwarderPanicked => Out::Flag(ctx.last_forwarder_error().is_some()),
            Cmd::Ping | Cmd::Gone(_) => Out::Unit,
        })
    }
}

struct Victim;

impl Actor for Victim {
    type Message = ();
    type Response = ();
    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: AtomicUsize,
}

impl Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a parked sender's waker panics");
        }
    }
}

type Pending = Pin<Box<dyn Future<Output = Result<(), SendError>> + Send>>;

async fn spawn_watcher() -> (
    ActorHandle<Watcher>,
    mpsc::UnboundedReceiver<()>,
    ActorHandle<Victim>,
    String,
) {
    let (began, rx) = mpsc::unbounded_channel();
    let name = uname("watcher");
    let w = Watcher { began }
        .spawn()
        .named(name.clone())
        .with_config(ActorConfig::default().with_priority_lane(1))
        .await
        .expect("the watcher spawns");
    let v = Victim
        .spawn()
        .named(uname("victim"))
        .await
        .expect("the victim spawns");
    (w, rx, v, name)
}

async fn id_of(w: &ActorHandle<Watcher>, cmd: Cmd) -> MonitorId {
    match within("the monitor", w.send(cmd)).await.unwrap() {
        Out::Id(id) => id,
        other => panic!("{other:?}"),
    }
}

/// Parks a `notify_priority` on the full lane with a waker that panics once
/// armed.
fn park_a_sender(w: &ActorHandle<Watcher>) -> (Pending, Arc<LoudWake>) {
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    let w = w.clone();
    let mut parked: Pending = Box::pin(async move { w.notify_priority(Cmd::Ping).await });
    assert!(
        matches!(
            parked.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        ),
        "a priority send into a full lane parks"
    );
    (parked, loud)
}

async fn woken(loud: &LoudWake) {
    within("the parked sender's waker", async {
        while loud.woken.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// The watcher is still running and none of its forwarder tasks panicked: a
/// panic of a parked sender's waker must not unwind into the monitor's task.
async fn alive(w: &ActorHandle<Watcher>) {
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        within("the watcher's answer", w.send(Cmd::Ping))
            .await
            .expect("the watcher runs on"),
        Out::Unit
    );
    assert_eq!(
        within("the forwarder check", w.send(Cmd::ForwarderPanicked))
            .await
            .expect("the watcher runs on"),
        Out::Flag(false),
        "no forwarder task of the watcher ended in a panic"
    );
}

/// Whether a warning naming `site` and the watcher was logged.
#[cfg(feature = "tracing")]
fn logged(site: &str, watcher: &str) -> bool {
    recorder::lines()
        .iter()
        .any(|line| line.contains(site) && line.contains(watcher))
}

#[cfg(feature = "tracing")]
mod recorder {
    use std::fmt::Write as _;
    use std::sync::{Mutex, OnceLock};

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    pub fn lines() -> Vec<String> {
        LINES.lock().unwrap().clone()
    }

    pub fn install() {
        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| {
            tracing::subscriber::set_global_default(Recording)
                .expect("no other global subscriber is installed in this process");
        });
    }

    struct Text(String);

    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    struct Recording;

    impl Subscriber for Recording {
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
            LINES.lock().unwrap().push(text.0);
        }
        fn enter(&self, _span: &span::Id) {}
        fn exit(&self, _span: &span::Id) {}
    }
}

fn record_logs() {
    #[cfg(feature = "tracing")]
    recorder::install();
}

/// A forwarder waiting for room on a full priority lane holds a reserve that
/// the channel grants a slot to when the watcher takes a message. A revoke
/// made in the turn that took it ends the forwarder before it uses the slot,
/// and the slot goes on to the sender parked behind it, whose waker panics.
/// The panic is contained: the watcher goes on, and the sender gets its slot.
#[test]
fn a_revoked_forwarders_granted_slot_wakes_a_sender_whose_waker_panics_inside_a_catch() {
    record_logs();
    common::run_current_thread_only(async {
        let (w, mut began, v, name) = spawn_watcher().await;
        let id = id_of(&w, Cmd::Watch(v.clone())).await;

        // The handler is held on a gate; the one slot of the lane then
        // holds the revoke, which the watcher takes after the gate.
        let gate = Arc::new(Semaphore::new(0));
        w.notify(Cmd::Hold(gate.clone())).await.unwrap();
        within("the handler to begin", began.recv()).await.unwrap();
        w.try_notify_priority(Cmd::Revoke(id)).unwrap();

        // The target ends; the forwarder waits for room on the full lane,
        // ahead of the sender that parks next.
        v.stop(StopReason::Graceful).await.unwrap();
        v.wait_stopped().await;
        sleep(Duration::from_millis(100)).await;
        let (parked, loud) = park_a_sender(&w);

        // The watcher takes the revoke: the freed slot is granted to the
        // forwarder, the same turn revokes its monitor, and the forwarder
        // drops its granted slot unused, which wakes the parked sender.
        loud.armed.store(true, Ordering::SeqCst);
        gate.add_permits(1);
        woken(&loud).await;
        loud.armed.store(false, Ordering::SeqCst);

        within("the sender's slot", parked).await.unwrap();
        alive(&w).await;
        #[cfg(feature = "tracing")]
        assert!(
            logged("priority lane", &name),
            "the contained panic is logged under the watcher's id"
        );
        let _ = name;
    });
}

/// The closure of a monitor's map has two owners, the entry and the
/// forwarder task. When the monitor is revoked the entry goes first and the
/// task drops the closure last; a captured value whose drop panics is logged
/// and the watcher goes on, as it does when the entry drops it last.
#[test]
fn a_map_closure_whose_drop_panics_is_contained_when_the_forwarder_drops_it_last() {
    record_logs();
    common::run(2, async {
        let (w, _began, v, name) = spawn_watcher().await;
        let armed = Arc::new(AtomicBool::new(false));
        let id = id_of(&w, Cmd::WatchLoud(v.clone(), armed.clone())).await;
        sleep(Duration::from_millis(50)).await;

        armed.store(true, Ordering::SeqCst);
        assert_eq!(
            within("the revoke", w.send(Cmd::Revoke(id))).await.unwrap(),
            Out::Flag(true)
        );
        sleep(Duration::from_millis(200)).await;
        armed.store(false, Ordering::SeqCst);

        alive(&w).await;
        #[cfg(feature = "tracing")]
        assert!(
            logged("arrived after its queue closed", &name),
            "the drop's panic is logged under the watcher's id"
        );
        let _ = name;
    });
}

/// How a sender that the table parks on the full lane is made.
#[derive(Clone, Copy, Debug)]
enum Site {
    NotifyPriority,
    SendPriority,
    /// With the deadline it is given.
    SendPriorityTimeout(Duration),
}

/// How the parked sender ends.
#[derive(Clone, Copy, Debug)]
enum End {
    /// Dropped after the lane granted it a slot it never took.
    Cancel,
    /// Its deadline passes while it is parked.
    Expire,
}

type Parked = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// Parks the sender under test on the full lane, polled once with a waker
/// that only counts.
fn park_site(w: &ActorHandle<Watcher>, site: Site) -> (Parked, Arc<LoudWake>) {
    let counting = Arc::new(LoudWake::default());
    let waker = Waker::from(counting.clone());
    let w = w.clone();
    let mut parked: Parked = match site {
        Site::NotifyPriority => Box::pin(async move {
            w.notify_priority(Cmd::Ping)
                .await
                .map_err(|err| format!("{err:?}"))
        }),
        Site::SendPriority => Box::pin(async move {
            w.send_priority(Cmd::Ping)
                .await
                .map(|_| ())
                .map_err(|err| format!("{err:?}"))
        }),
        Site::SendPriorityTimeout(deadline) => Box::pin(async move {
            w.send_priority_timeout(Cmd::Ping, deadline)
                .await
                .map(|_| ())
                .map_err(|err| format!("{err:?}"))
        }),
    };
    assert!(
        matches!(
            parked.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        ),
        "{site:?} into a full lane parks"
    );
    (parked, counting)
}
