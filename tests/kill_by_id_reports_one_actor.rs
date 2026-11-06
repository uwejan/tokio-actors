//! `kill_by_id` reports the actor it killed and returns once that actor has
//! ended: the actors below a killed supervisor end after it, so a child of the
//! killed actor can still be in its `Drop` when the call returns. The report
//! names one actor and the call does not wait for the rest.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);

/// The longest a blocked thread waits for the gate. It only limits how long a
/// real hang takes to report.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// A gate a child's `Drop` waits on.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *(self.0).0.lock().unwrap() = true;
        (self.0).1.notify_all();
    }

    /// Blocks the calling thread until the gate opens, or until the hang
    /// bound passes.
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

/// Opens the gate when dropped, so a failed assertion never leaves a worker
/// blocked.
struct OpenOnDrop(Gate);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
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

/// A supervisor with one `Slow` child whose kill grace is far beyond the test.
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
        .kill_grace(Duration::from_secs(30))
        .await?;
        Ok(())
    }
}

#[test]
fn kill_by_id_returns_while_a_child_is_still_in_its_drop() {
    common::run_multi_thread_only(2, async {
        let sys = ActorSystem::create(format!("kill-one-{}", uuid::Uuid::new_v4())).unwrap();
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
        .expect("the boss spawns");

        let report = timeout(BOUND, sys.kill_by_id(boss.id()))
            .await
            .expect("kill_by_id returns while the child below the boss is blocked");
        assert!(report.is_some(), "the boss was registered");
        assert!(
            !dropped.load(Ordering::SeqCst),
            "the child is still in its Drop: the report names the killed actor only"
        );
        gate.open();
    });
}
