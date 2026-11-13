#![allow(dead_code)]
//! A system that has been shut down and dropped by its user leaves nothing
//! behind: its name leaves the process map of named systems and is free
//! again, the system itself is freed, and the tasks it ran for the runtime
//! (its reaper among them) end. Systems created, used and shut down in a loop
//! therefore hold a bounded footprint.
//!
//! Observed through the public surface only: the names `ActorSystem::all`
//! lists, `ActorSystem::get_named` and `ActorSystem::create` for a name taken
//! or free, a `Weak` handle to each system, and the runtime's count of alive
//! tasks. A system its user still holds is shut down and stays findable, and
//! its tasks end all the same, since a shut-down system has no work for them.

use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, StopOutcome, StopReason,
};

/// Systems created, used and shut down by each row.
const SYSTEMS: usize = 20;

/// The longest the runtime may take to end the helper tasks of a system that
/// was dropped. A pending abort deadline of the reaper (the stop's window
/// below) is the longest wait a dropped system's reaper has.
const SETTLE: Duration = Duration::from_secs(5);

/// The window of the bounded stop that feeds the reaper.
const WINDOW: Duration = Duration::from_millis(50);

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// What one row left behind.
#[derive(Debug, Default, PartialEq)]
struct Left {
    /// Names still listed by `ActorSystem::all`.
    listed: usize,
    /// Names still found by `ActorSystem::get_named`.
    found: usize,
    /// Systems whose `Weak` still upgrades.
    alive: usize,
    /// Names `ActorSystem::create` still refuses.
    taken: usize,
    /// Tasks alive on the runtime beyond the row's starting count once the
    /// settle time has passed.
    tasks: usize,
}

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

/// Creates, uses and shuts down `SYSTEMS` systems, then drops them unless
/// `hold` keeps them. With `feed_reaper` the actor is stopped by a bounded
/// `Kill`, which schedules its abort with the system's reaper; without it the
/// actor is stopped gracefully and the reaper is never asked for.
async fn run_row(feed_reaper: bool, hold: bool) -> Left {
    let baseline = alive_tasks();
    let mut names = Vec::new();
    let mut weaks: Vec<Weak<ActorSystem>> = Vec::new();
    let mut held = Vec::new();

    for _ in 0..SYSTEMS {
        let name = format!("footprint-{}", uuid::Uuid::new_v4());
        let sys = ActorSystem::create(name.clone()).unwrap();
        let actor = Idle.spawn().on_system(&sys).await.unwrap();
        let (reason, outcome) = if feed_reaper {
            (StopReason::Kill, StopOutcome::Killed)
        } else {
            (StopReason::Graceful, StopOutcome::Graceful)
        };
        let got = actor.stop_within(reason, WINDOW).await.unwrap();
        assert_eq!(got, outcome);
        sys.shutdown().await;
        weaks.push(Arc::downgrade(&sys));
        names.push(name);
        drop(actor);
        if hold {
            held.push(sys);
        }
    }

    // The helper tasks of a dropped system end by themselves; wait for them.
    let ended = Instant::now() + SETTLE;
    while alive_tasks() > baseline && Instant::now() < ended {
        sleep(Duration::from_millis(20)).await;
    }

    let listed = ActorSystem::all();
    let mut left = Left {
        tasks: alive_tasks().saturating_sub(baseline),
        alive: weaks.iter().filter(|w| w.strong_count() > 0).count(),
        ..Left::default()
    };
    for name in &names {
        left.listed += usize::from(listed.contains(name));
        left.found += usize::from(ActorSystem::get_named(name).is_some());
        match ActorSystem::create(name.clone()) {
            Ok(again) => {
                again.shutdown().await;
            }
            Err(_) => left.taken += 1,
        }
    }
    drop(held);
    left
}

/// Runs one row on a runtime of its own, so the count of alive tasks starts
/// from nothing and a row cannot leave tasks behind for the next one.
fn row_on_its_own_runtime(feed_reaper: bool, hold: bool) -> Left {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        timeout(Duration::from_secs(60), run_row(feed_reaper, hold))
            .await
            .expect("the row did not finish")
    })
}

/// The turn-watchdog sweep of a dropped system ends, and does not carry on as
/// the sweep of a later system that claimed the same name: each pair below
/// leaves exactly one sweep, the later system's.
#[cfg(feature = "tracing")]
#[test]
fn a_dropped_systems_sweep_does_not_become_the_sweep_of_a_later_system_of_its_name() {
    use tokio_actors::SystemConfig;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (tasks, expected) = rt.block_on(async {
        let config =
            || SystemConfig::default().with_turn_warning_threshold(Duration::from_millis(200));
        let mut later = Vec::new();
        for _ in 0..SYSTEMS {
            let name = format!("footprint-sweep-{}", uuid::Uuid::new_v4());
            let first = ActorSystem::create_with(name.clone(), config()).unwrap();
            let actor = Idle.spawn().on_system(&first).await.unwrap();
            actor
                .stop_within(StopReason::Graceful, WINDOW)
                .await
                .unwrap();
            first.shutdown().await;
            drop(actor);
            let weak = Arc::downgrade(&first);
            drop(first);
            // A shutdown's walk holds the system for a moment after the call
            // returns; the claim follows the last holder at once, so the first
            // system's sweep (a tick every 50 ms) has not looked for it since.
            while weak.strong_count() > 0 {
                tokio::task::yield_now().await;
            }
            let second = ActorSystem::create_with(name, config()).unwrap();
            let actor = Idle.spawn().on_system(&second).await.unwrap();
            actor
                .stop_within(StopReason::Graceful, WINDOW)
                .await
                .unwrap();
            later.push((second, actor));
        }
        let ended = Instant::now() + SETTLE;
        while alive_tasks() > SYSTEMS && Instant::now() < ended {
            sleep(Duration::from_millis(20)).await;
        }
        sleep(Duration::from_millis(300)).await;
        let tasks = alive_tasks();
        drop(later);
        (tasks, SYSTEMS)
    });
    assert_eq!(
        tasks, expected,
        "one sweep per later system, none left over from a dropped one"
    );
}
