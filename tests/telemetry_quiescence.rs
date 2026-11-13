#![allow(dead_code)]
//! `ActorSystem::is_quiescent`: quiet now, read in two passes.
//!
//! The answer is `None` while turn tracking is off, `Some(true)` for a system
//! whose actors are idle with empty lanes, and `Some(false)` while an actor is
//! inside a turn, has a queued message or has a restart attempt in flight.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{sleep, timeout, Instant};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, ActorSystem, SystemConfig,
};

const BOUND: Duration = Duration::from_secs(30);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "telemetry-quiescence-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// A system with turn tracking as the row says. The watchdog threshold, far
/// above anything a test waits for, also times turns, so a turn in flight is
/// flagged whichever switch is asked to time it.
fn system(base: &str, tracking: bool) -> Arc<ActorSystem> {
    let mut config = SystemConfig::default();
    if tracking {
        config = config
            .with_turn_tracking()
            .with_turn_warning_threshold(Duration::from_secs(3600));
    }
    ActorSystem::create_with(uname(base), config).unwrap()
}

/// Polls until the system reads `Some(true)`, at most `BOUND`.
async fn settle(sys: &ActorSystem) {
    let deadline = Instant::now() + BOUND;
    while sys.is_quiescent() != Some(true) {
        assert!(
            Instant::now() < deadline,
            "the system never settled: {:?}",
            sys.is_quiescent()
        );
        sleep(Duration::from_millis(2)).await;
    }
}

#[derive(Clone, Default)]
struct Gate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

/// Lets go of its gate when dropped, so a failed assertion never leaves a
/// handler waiting.
struct Release(Arc<Notify>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

enum HeldMsg {
    Hold,
    Plain,
}

/// Holds its turn on a gate while it handles `Hold`.
struct Held(Gate);

impl Actor for Held {
    type Message = HeldMsg;
    type Response = ();

    async fn handle(&mut self, msg: HeldMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let HeldMsg::Hold = msg {
            self.0.entered.notify_one();
            self.0.release.notified().await;
        }
        Ok(())
    }
}

#[test]
fn quiescence_reads_the_system_as_it_is_now() {
    common::run(2, async {
        // Tracking off: no answer, even while a turn is in flight.
        let off = system("off", false);
        let gate = Gate::default();
        let _release = Release(gate.release.clone());
        let held = Held(gate.clone()).spawn().on_system(&off).await.unwrap();
        held.notify(HeldMsg::Hold).await.unwrap();
        timeout(BOUND, gate.entered.notified())
            .await
            .expect("the turn begins");
        assert_eq!(off.is_quiescent(), None);
        gate.release.notify_one();

        // Tracking on: the rows are read in this order on one actor.
        let on = system("on", true);
        let gate = Gate::default();
        let _release = Release(gate.release.clone());
        let held = Held(gate.clone()).spawn().on_system(&on).await.unwrap();
        settle(&on).await;

        held.notify(HeldMsg::Hold).await.unwrap();
        timeout(BOUND, gate.entered.notified())
            .await
            .expect("the turn begins");
        assert_eq!(on.is_quiescent(), Some(false), "an actor is inside a turn");

        held.notify(HeldMsg::Plain).await.unwrap();
        assert_eq!(
            on.is_quiescent(),
            Some(false),
            "a turn in flight with a message queued behind it"
        );

        gate.release.notify_one();
        settle(&on).await;
    });
}

// ---------------------------------------------------------------------------
// A restart attempt in flight
// ---------------------------------------------------------------------------

/// Opens once, for a factory that runs on a worker thread and cannot await.
#[derive(Default)]
struct ThreadGate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl ThreadGate {
    fn wait(&self) {
        let guard = self.open.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(guard, BOUND, |open| !*open)
            .unwrap();
    }

    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

struct Worker;

impl Actor for Worker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("crash on command"))
    }
}

/// A supervisor with one permanent worker whose factory, after its first
/// call, holds on a gate.
struct Boss {
    calls: Arc<AtomicUsize>,
    inside: Arc<AtomicBool>,
    gate: Arc<ThreadGate>,
    worker: String,
}

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (calls, inside, gate) = (self.calls.clone(), self.inside.clone(), self.gate.clone());
        ctx.spawn_child(move || {
            if calls.fetch_add(1, Ordering::SeqCst) > 0 {
                inside.store(true, Ordering::SeqCst);
                gate.wait();
            }
            Worker
        })
        .named(self.worker.clone())
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A worker that crashes on any message and, once told to hold, keeps its
/// task alive in its drop, after its registry entry has left.
struct Mortal {
    hold: Arc<AtomicBool>,
    gate: Arc<ThreadGate>,
    dropping: Arc<AtomicBool>,
}

impl Actor for Mortal {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("crash on command"))
    }
}

impl Drop for Mortal {
    fn drop(&mut self) {
        if self.hold.load(Ordering::SeqCst) {
            self.dropping.store(true, Ordering::SeqCst);
            self.gate.wait();
        }
    }
}

/// A supervisor with one permanent `Mortal`.
struct MortalBoss {
    hold: Arc<AtomicBool>,
    gate: Arc<ThreadGate>,
    dropping: Arc<AtomicBool>,
    worker: String,
}

impl Actor for MortalBoss {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (hold, gate, dropping) = (self.hold.clone(), self.gate.clone(), self.dropping.clone());
        ctx.spawn_child(move || Mortal {
            hold: hold.clone(),
            gate: gate.clone(),
            dropping: dropping.clone(),
        })
        .named(self.worker.clone())
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}
