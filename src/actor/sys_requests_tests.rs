//! The request path of the `sys` calls: a call is bounded whatever it waits
//! on, a request whose caller left is never run, an actor that ended answers
//! one way however the call met the end, a runtime without a time driver is
//! refused at once, and a pending call is served ahead of a flood of
//! messages. Every row runs on a paused clock and on a multi-thread runtime;
//! nothing is ordered by a sleep.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::Builder;
use tokio::sync::{Notify, Semaphore};

use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::runtime::ActorConfig;
use crate::actor::sys::{SysError, DEFAULT_SYS_TIMEOUT};
use crate::actor::{Actor, ActorExt};
use crate::error::ActorResult;
use crate::types::{StopReason, SysCommand};

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// The messages of the flood, and the one whose handler reads the counter.
const FLOOD: usize = 1999;
const CHECKED: usize = 1500;

#[derive(Clone, Copy)]
enum Flavor {
    /// A current-thread runtime with a paused clock.
    Paused,
    /// A multi-thread runtime with the real clock.
    Real,
}

enum Msg {
    /// Signals `entered`, then waits until the gate is closed.
    Hold,
    /// One message of a flood; the checked one reads whether a probe ran.
    Work(usize),
}

struct Worker {
    entered: Arc<Notify>,
    gate: Arc<Semaphore>,
    probes: Arc<AtomicUsize>,
    probe_ran_before_the_checked_message: Arc<AtomicBool>,
}

impl Actor for Worker {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Hold => {
                self.entered.notify_one();
                // Closing the semaphore ends the wait.
                let _ = self.gate.acquire().await;
            }
            Msg::Work(n) => {
                if n == CHECKED {
                    self.probe_ran_before_the_checked_message
                        .store(self.probes.load(Ordering::SeqCst) > 0, Ordering::SeqCst);
                }
            }
        }
        Ok(())
    }
}

/// An actor that can be held in a handler, and what the rows read of it.
struct Rig {
    handle: ActorHandle<Worker>,
    entered: Arc<Notify>,
    gate: Arc<Semaphore>,
    probes: Arc<AtomicUsize>,
    probe_ran_before_the_checked_message: Arc<AtomicBool>,
}

impl Rig {
    async fn new() -> Self {
        let entered = Arc::new(Notify::new());
        let gate = Arc::new(Semaphore::new(0));
        let probes = Arc::new(AtomicUsize::new(0));
        let flag = Arc::new(AtomicBool::new(false));
        let handle = Worker {
            entered: entered.clone(),
            gate: gate.clone(),
            probes: probes.clone(),
            probe_ran_before_the_checked_message: flag.clone(),
        }
        .spawn()
        .with_config(ActorConfig::default().with_mailbox_capacity(2100))
        .await
        .expect("the worker spawns");
        Self {
            handle,
            entered,
            gate,
            probes,
            probe_ran_before_the_checked_message: flag,
        }
    }

    /// Holds the actor in a handler.
    async fn hold(&self) {
        self.handle.notify(Msg::Hold).await.unwrap();
        self.entered.notified().await;
    }

    /// Lets the held handler go, now and for every later hold.
    fn release(&self) {
        self.gate.close();
    }

    /// How many commands wait on the system channel, and how many places
    /// are free; `None` before the channel exists.
    fn channel(&self) -> Option<(usize, usize)> {
        self.handle
            .sys_record()
            .system_tx_for_test()
            .map(|tx| (tx.max_capacity() - tx.capacity(), tx.capacity()))
    }

    /// A call that counts a run of the command, on a task of its own.
    fn probe(&self, bound: Duration) -> tokio::task::JoinHandle<Result<(), SysError>> {
        let handle = self.handle.clone();
        let probes = self.probes.clone();
        tokio::spawn(async move {
            handle
                .sys()
                .timeout(bound)
                .call(|reply| SysCommand::Probe(probes, reply))
                .await
        })
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.release();
    }
}

/// Waits, a yield at a time, until `condition` reads true.
async fn until(what: &str, condition: impl Fn() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(started.elapsed() < HANG_BOUND, "never saw: {what}");
        tokio::task::yield_now().await;
    }
}

async fn finished<T>(task: tokio::task::JoinHandle<T>) -> T {
    bounded(task).await.expect("the call's task ends")
}

/// Awaits `work`, failing instead of hanging when it never ends.
async fn bounded<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, work)
        .await
        .expect("the call answers")
}

type Row = fn(Flavor) -> Pin<Box<dyn Future<Output = ()>>>;

/// A request whose caller timed out is dropped unrun, and the actor goes on.
async fn stale_request(_: Flavor) {
    let rig = Rig::new().await;
    rig.hold().await;
    let outcome = bounded(
        rig.handle
            .sys()
            .timeout(Duration::from_millis(30))
            .call(|reply| SysCommand::Probe(rig.probes.clone(), reply)),
    )
    .await;
    assert_eq!(outcome, Err(SysError::Timeout));
    rig.release();
    rig.handle.send(Msg::Work(0)).await.unwrap();
    assert_eq!(rig.probes.load(Ordering::SeqCst), 0, "the stale probe ran");
}

/// With no bound set, a call to a stuck actor answers at five seconds.
async fn default_bound(flavor: Flavor) {
    // The clock is paused only on the current-thread runtime.
    let Flavor::Paused = flavor else { return };
    assert_eq!(DEFAULT_SYS_TIMEOUT, Duration::from_millis(5000));
    let rig = Rig::new().await;
    rig.hold().await;
    let handle = rig.handle.clone();
    let probes = rig.probes.clone();
    let call = tokio::spawn(async move {
        handle
            .sys()
            .call(|reply| SysCommand::Probe(probes, reply))
            .await
    });
    until("the request is queued", || {
        rig.channel().is_some_and(|(queued, _)| queued == 1)
    })
    .await;
    tokio::time::advance(Duration::from_millis(4999)).await;
    tokio::task::yield_now().await;
    assert!(!call.is_finished(), "answered before five seconds");
    tokio::time::advance(Duration::from_millis(2)).await;
    assert_eq!(finished(call).await, Err(SysError::Timeout));
}

/// A request queued behind a stop and one made after the end answer alike.
async fn both_end_shapes(_: Flavor) {
    let rig = Rig::new().await;
    rig.hold().await;
    let queued = rig.probe(Duration::from_secs(60));
    until("the request is queued", || {
        rig.channel().is_some_and(|(queued, _)| queued == 1)
    })
    .await;
    rig.handle.stop(StopReason::Graceful).await.unwrap();
    rig.release();
    let ended = Err(SysError::ActorEnded(StopReason::Graceful));
    assert_eq!(finished(queued).await, ended);
    let after = bounded(
        rig.handle
            .sys()
            .call(|reply| SysCommand::Probe(rig.probes.clone(), reply)),
    )
    .await;
    assert_eq!(after, ended);
}

/// A runtime without a time driver is refused before anything is queued.
async fn no_time_driver(_: Flavor) {
    let rig = Rig::new().await;
    let handle = rig.handle.clone();
    let probes = rig.probes.clone();
    let outcome = std::thread::spawn(move || {
        let runtime = Builder::new_current_thread().build().unwrap();
        runtime.block_on(async move {
            handle
                .sys()
                .call(|reply| SysCommand::Probe(probes, reply))
                .await
        })
    })
    .join()
    .expect("the call does not panic");
    assert_eq!(outcome, Err(SysError::NoTimeDriver));
    assert!(rig.channel().is_none(), "a command was queued");
}

/// The bound covers the wait for room on a full channel.
async fn full_channel(_: Flavor) {
    let rig = Rig::new().await;
    rig.hold().await;
    let waiting: Vec<_> = (0..69)
        .map(|_| rig.probe(Duration::from_secs(3600)))
        .collect();
    until("the channel is full", || rig.channel() == Some((64, 0))).await;
    let last = rig.probe(Duration::from_millis(30));
    assert_eq!(finished(last).await, Err(SysError::Timeout));
    rig.release();
    for call in waiting {
        assert_eq!(finished(call).await, Ok(()));
    }
    assert_eq!(rig.probes.load(Ordering::SeqCst), 69);
}

/// A pending call is served ahead of the messages queued behind the one the
/// actor is handling.
async fn call_ahead_of_a_flood(_: Flavor) {
    let rig = Rig::new().await;
    rig.hold().await;
    for n in 1..=FLOOD {
        rig.handle.notify(Msg::Work(n)).await.unwrap();
    }
    let call = rig.probe(Duration::from_secs(3600));
    until("the request is queued", || {
        rig.channel().is_some_and(|(queued, _)| queued == 1)
    })
    .await;
    rig.release();
    assert_eq!(finished(call).await, Ok(()));
    rig.handle.send(Msg::Work(FLOOD + 1)).await.unwrap();
    assert!(
        rig.probe_ran_before_the_checked_message
            .load(Ordering::SeqCst),
        "the flood was handled before the call was served"
    );
}

const ROWS: [(&str, Row); 6] = [
    ("stale request", |f| Box::pin(stale_request(f))),
    ("default bound", |f| Box::pin(default_bound(f))),
    ("both end shapes", |f| Box::pin(both_end_shapes(f))),
    ("no time driver", |f| Box::pin(no_time_driver(f))),
    ("full channel", |f| Box::pin(full_channel(f))),
    ("call ahead of a flood", |f| {
        Box::pin(call_ahead_of_a_flood(f))
    }),
];

async fn rows(flavor: Flavor) {
    for (name, row) in ROWS {
        eprintln!("row: {name}");
        row(flavor).await;
    }
}

#[tokio::test(start_paused = true)]
async fn sys_requests() {
    rows(Flavor::Paused).await;
}
