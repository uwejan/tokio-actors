#![cfg(feature = "tracing")]
//! What the subtree observer does around the log lines of the `tracing`
//! feature. Each scenario runs in a child process of this test binary,
//! because it installs the one global subscriber.
//!
//! - The wait for the subtree starts when the observed actor has ended and
//!   let go of what it held. The end log line of the actor's own task runs
//!   user code (the subscriber) after `Stopped` is published, so a wait that
//!   counted from `Stopped` would give the verdict for an actor with nothing
//!   below it while a subscriber is slow.
//! - A wait that answers `Unresponsive` records no verdict on any supervisor
//!   and writes no log line.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, ActorSystem, StopReason, SubtreeEnd,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// The window of a wait that ran out.
#[derive(Debug)]
struct Elapsed;

/// `tokio::time::timeout` measured in real time and polled by hand: the
/// future is polled every few milliseconds on the calling thread, and no timer
/// of the runtime is in the window. A worker held inside the subscriber or a
/// `Drop` can leave a multi-thread runtime without a thread that drives its
/// timers, and a window measured by that timer then lasts as long as the hold.
async fn timeout<F: Future>(window: Duration, fut: F) -> Result<F::Output, Elapsed> {
    let mut fut = Box::pin(fut);
    let mut cx = Context::from_waker(Waker::noop());
    let started = Instant::now();
    loop {
        if let Poll::Ready(output) = fut.as_mut().poll(&mut cx) {
            return Ok(output);
        }
        if started.elapsed() >= window {
            return Err(Elapsed);
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

/// A gate a thread blocks on.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *(self.0).0.lock().unwrap() = true;
        (self.0).1.notify_all();
    }

    fn wait(&self) {
        let (lock, changed) = &*self.0;
        let started = Instant::now();
        let mut open = lock.lock().unwrap();
        while !*open {
            let left = HANG_BOUND.saturating_sub(started.elapsed());
            if left.is_zero() {
                return;
            }
            open = changed.wait_timeout(open, left).unwrap().0;
        }
    }
}

/// Opens the gate when dropped, so a failed assertion never leaves a thread
/// blocked.
struct OpenOnDrop(Gate);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// An actor that does nothing.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A child whose `Drop` waits on the gate and then sets the flag.
struct Slow {
    gate: Gate,
    dropped: Arc<AtomicBool>,
}

impl Drop for Slow {
    fn drop(&mut self) {
        self.gate.wait();
        self.dropped.store(true, Ordering::SeqCst);
    }
}

impl Actor for Slow {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A supervisor with one `Slow` child that is aborted as soon as it gets a
/// `Kill`.
struct Boss {
    gate: Gate,
    dropped: Arc<AtomicBool>,
}

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (gate, dropped) = (self.gate.clone(), self.dropped.clone());
        ctx.spawn_child(move || Slow {
            gate: gate.clone(),
            dropped: dropped.clone(),
        })
        .kill_grace(Duration::ZERO)
        .await?;
        Ok(())
    }
}

#[test]
fn the_wait_does_not_count_the_time_the_end_log_line_of_the_actor_takes() {
    const NAME: &str = "the_wait_does_not_count_the_time_the_end_log_line_of_the_actor_takes";
    if !common::in_a_child_process(NAME) {
        return;
    }
    let subscriber = subscriber::install();
    common::run_multi_thread_only(2, async move {
        let sys = ActorSystem::create(unique("tail")).unwrap();
        let actor = Idle
            .spawn()
            .named(unique("tail"))
            .on_system(&sys)
            .await
            .expect("spawns");
        let observer = actor.observe_subtree();

        // The subscriber holds the actor's end log line, which the actor's
        // task writes after it publishes `Stopped` and before it returns.
        let gate = Gate::default();
        let _open = OpenOnDrop(gate.clone());
        let held = subscriber.hold(
            &["Actor stopped", &format!("actor_id={} ", actor.id())],
            &gate,
        );
        actor.stop(StopReason::Graceful).await.expect("accepted");
        held.recv_timeout(HANG_BOUND)
            .expect("the subscriber holds the end log line");
        assert_eq!(actor.status(), ActorStatus::Stopped);

        let mut wait = Box::pin(observer.wait_ended());
        assert!(
            timeout(Duration::from_millis(400), &mut wait)
                .await
                .is_err(),
            "a wait that counted from `Stopped` would have given its verdict by now"
        );
        gate.open();
        assert_eq!(
            timeout(HANG_BOUND, &mut wait).await.expect("answers"),
            SubtreeEnd::Ended
        );
    });
}

#[test]
fn an_unresponsive_answer_records_no_verdict_and_writes_no_log_line() {
    const NAME: &str = "an_unresponsive_answer_records_no_verdict_and_writes_no_log_line";
    if !common::in_a_child_process(NAME) {
        return;
    }
    let subscriber = subscriber::install();
    common::run_multi_thread_only(2, async move {
        let sys = ActorSystem::create(unique("verdict")).unwrap();
        let gate = Gate::default();
        let _open = OpenOnDrop(gate.clone());
        let dropped = Arc::new(AtomicBool::new(false));
        let boss = Boss {
            gate: gate.clone(),
            dropped: dropped.clone(),
        }
        .spawn()
        .on_system(&sys)
        .supervisor()
        .await
        .expect("spawns");
        let observer = boss.observe_subtree();
        timeout(BOUND, sys.kill_by_id(boss.id()))
            .await
            .expect("returns")
            .expect("registered");

        assert_eq!(
            timeout(HANG_BOUND, observer.wait_ended())
                .await
                .expect("the verdict comes"),
            SubtreeEnd::Unresponsive
        );
        assert!(!dropped.load(Ordering::SeqCst));
        let lines = subscriber.lines();
        assert!(
            lines.iter().any(|line| line.contains("Actor stopped")),
            "the subscriber sees the crate's lines: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("left behind unresponsive")),
            "the observer writes no verdict line: {lines:?}"
        );

        gate.open();
        assert_eq!(
            timeout(HANG_BOUND, observer.wait_ended())
                .await
                .expect("a later call answers"),
            SubtreeEnd::Ended
        );
    });
}

mod subscriber {
    use std::fmt::Write as _;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    use super::Gate;

    /// An event to hold back: the first event whose line contains every
    /// needle blocks its thread until the gate opens, after saying so.
    struct Hold {
        needles: Vec<String>,
        entered: Sender<()>,
        gate: Gate,
    }

    struct Recording {
        lines: Arc<Mutex<Vec<String>>>,
        hold: Arc<Mutex<Option<Hold>>>,
    }

    /// The handle a test keeps of the installed subscriber.
    pub struct Installed {
        lines: Arc<Mutex<Vec<String>>>,
        hold: Arc<Mutex<Option<Hold>>>,
    }

    impl Installed {
        /// Holds back the next event whose line contains every needle. The
        /// receiver gets a message once the event is held.
        pub fn hold(&self, needles: &[&str], gate: &Gate) -> Receiver<()> {
            let (entered, held) = channel();
            *self.hold.lock().unwrap() = Some(Hold {
                needles: needles.iter().map(|n| (*n).to_string()).collect(),
                entered,
                gate: gate.clone(),
            });
            held
        }

        pub fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }
    }

    struct Text(String);

    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    /// Installs the one global subscriber of this process.
    pub fn install() -> Installed {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let hold = Arc::new(Mutex::new(None));
        tracing::subscriber::set_global_default(Recording {
            lines: lines.clone(),
            hold: hold.clone(),
        })
        .expect("no other global subscriber is installed in this process");
        Installed { lines, hold }
    }

    impl Subscriber for Recording {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            matches!(*metadata.level(), Level::INFO | Level::WARN | Level::ERROR)
        }

        fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut text = Text(String::new());
            event.record(&mut text);
            let line = text.0;
            let held = {
                let mut slot = self.hold.lock().unwrap();
                match slot.as_ref() {
                    Some(hold) if hold.needles.iter().all(|n| line.contains(n.as_str())) => {
                        slot.take()
                    }
                    _ => None,
                }
            };
            self.lines.lock().unwrap().push(line);
            if let Some(hold) = held {
                let _ = hold.entered.send(());
                hold.gate.wait();
            }
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }
}
