//! A forwarder's end is counted before its task reads as finished when a
//! runtime shutdown cancels it while idle. The order holds because tokio drops
//! a task's future before it completes the task; a tokio that reversed it
//! would let a finished task read as uncounted.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;

fn idle_forwarder(done: Arc<AtomicU64>) -> Forwarder<std::future::Pending<Released>> {
    let (status_tx, _status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    Forwarder {
        done,
        counted: false,
        future: Some(Box::pin(std::future::pending::<Released>())),
        slot: PanicSlot::default(),
        owner: ActorId::from("forwarder-count"),
        _status: status_tx,
        _subtree: SubtreeHold::new(None),
    }
}

/// Idle forwarders on a multi-thread runtime that another thread shuts
/// down: at every sample an observer thread takes, a task whose abort handle
/// reads finished has its end counted.
#[test]
fn a_shutdown_counts_each_forwarder_before_its_task_reads_finished() {
    const PASSES: usize = 20;
    const TASKS: usize = 64;
    let mut samples_total = 0u64;
    let mut finished_seen = 0u64;
    for pass in 0..PASSES {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let mut set = tokio::task::JoinSet::new();
        let mut entries = Vec::with_capacity(TASKS);
        for _ in 0..TASKS {
            let done = Arc::new(AtomicU64::new(0));
            let abort = set.spawn_on(idle_forwarder(done.clone()), rt.handle());
            entries.push((abort, done));
        }
        // Each task is polled once and parks.
        std::thread::sleep(Duration::from_millis(20));
        assert!(entries
            .iter()
            .all(|(abort, done)| !abort.is_finished() && done.load(Ordering::Acquire) == 0));
        let go = Arc::new(AtomicBool::new(false));
        let observer = {
            let go = go.clone();
            std::thread::spawn(move || {
                let mut violations = Vec::new();
                let mut samples = 0u64;
                let mut finished = 0u64;
                let deadline = Instant::now() + Duration::from_secs(20);
                while !go.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                loop {
                    samples += 1;
                    let mut all = true;
                    for (i, (abort, done)) in entries.iter().enumerate() {
                        if abort.is_finished() {
                            finished += 1;
                            if done.load(Ordering::Acquire) == 0 {
                                violations.push(i);
                            }
                        } else {
                            all = false;
                        }
                    }
                    if all || Instant::now() > deadline {
                        let counted = entries
                            .iter()
                            .filter(|(_, done)| done.load(Ordering::Acquire) == 1)
                            .count();
                        return (violations, samples, finished, all, counted);
                    }
                }
            })
        };
        let shutter = std::thread::spawn(move || {
            go.store(true, Ordering::Release);
            drop(rt);
        });
        shutter.join().unwrap();
        let (violations, samples, finished, all, counted) = observer.join().unwrap();
        drop(set);
        samples_total += samples;
        finished_seen += finished;
        assert!(
            all,
            "pass {pass}: not every task finished after the shutdown"
        );
        assert!(
            violations.is_empty(),
            "pass {pass}: {} samples read a finished task whose end was not counted",
            violations.len()
        );
        assert_eq!(
            counted, TASKS,
            "pass {pass}: ends counted after the shutdown"
        );
    }
    eprintln!(
        "order pin: {PASSES} passes, {samples_total} samples, {finished_seen} finished reads"
    );
}
