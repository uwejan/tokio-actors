//! The parts of the snapshot and of quiescence that need crate-private seams:
//! the coupling counts the block holds, the restart attempts a supervisor
//! counts, and the hook between the two reads of quiescence.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::actor::{context::ActorContext, Actor, ActorExt};
use crate::error::ActorResult;
use crate::system::{ActorSystem, SystemConfig};

const BOUND: Duration = Duration::from_secs(30);

/// Runs `body` once on a current-thread runtime and once on a multi-thread
/// runtime.
fn both_flavors<F: Future<Output = ()>>(body: impl Fn() -> F) {
    for multi in [false, true] {
        let mut builder = if multi {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(2);
            builder
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        builder
            .enable_all()
            .build()
            .unwrap()
            .block_on(async { tokio::time::timeout(BOUND, body()).await.unwrap() });
    }
}

fn system(tracking: bool) -> Arc<ActorSystem> {
    let mut config = SystemConfig::default();
    if tracking {
        // The watchdog threshold also times turns, so a turn in flight is
        // flagged whichever switch is asked to time it.
        config = config
            .with_turn_tracking()
            .with_turn_warning_threshold(Duration::from_secs(3600));
    }
    ActorSystem::create_with(format!("snapshot-unit-{}", uuid::Uuid::new_v4()), config).unwrap()
}

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Polls until the system reads quiescent, at most `BOUND`.
async fn settle(sys: &ActorSystem) {
    let deadline = Instant::now() + BOUND;
    while sys.is_quiescent() != Some(true) {
        assert!(Instant::now() < deadline, "the system never settled");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Monitors the actor it is given from its start.
struct Watcher(crate::actor::handle::ActorHandle<Idle>);

impl Actor for Watcher {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.monitor(&self.0, |_| ())
            .expect("the watcher is running");
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A snapshot row reads the five coupling counts from the incarnation's
/// block, one row of the table per count: the watchers and the group
/// memberships as the status reports them, and the lengths of the actor's
/// own monitor, link and group monitor tables as the actor last recorded
/// them; each count follows a change.
#[test]
fn a_snapshot_row_shows_the_coupling_counts_of_the_block() {
    use crate::actor::subtree::Coupling;
    use crate::telemetry::ActorRecord;

    both_flavors(|| async {
        let sys = system(false);
        let target = Idle.spawn().on_system(&sys).await.unwrap();
        let _watcher = Watcher(target.clone())
            .spawn()
            .on_system(&sys)
            .await
            .unwrap();
        let deadline = Instant::now() + BOUND;
        while target.get_status().await.unwrap().monitored_by != 1 {
            assert!(
                Instant::now() < deadline,
                "the watcher's monitor is never made"
            );
            tokio::task::yield_now().await;
        }
        let scope = sys.pg();
        scope.join("a", &target).unwrap();
        scope.join("b", &target).unwrap();
        let record = target.stop_lane_ref().abort_record();
        record.set_coupling(Coupling::Monitor, 2);
        record.set_coupling(Coupling::Link, 3);
        record.set_coupling(Coupling::GroupMonitor, 4);
        let row = || {
            sys.snapshot()
                .into_iter()
                .find(|row| row.actor.id() == target.id())
                .expect("the target is listed")
        };

        type Read = fn(&ActorRecord) -> usize;
        let status = target.get_status().await.unwrap();
        let table: [(&str, Read, usize); 5] = [
            ("monitors", ActorRecord::monitors, 2),
            (
                "monitored_by",
                ActorRecord::monitored_by,
                status.monitored_by,
            ),
            ("links", ActorRecord::links, 3),
            ("pg_groups", ActorRecord::pg_groups, status.pg_groups),
            ("group_monitors", ActorRecord::group_monitors, 4),
        ];
        assert_eq!((status.monitored_by, status.pg_groups), (1, 2));
        let listed = row();
        for (name, read, want) in table {
            assert_eq!(read(&listed), want, "{name}");
        }

        scope.leave("a", &target).unwrap();
        record.set_coupling(Coupling::Link, 0);
        let listed = row();
        assert_eq!(
            listed.pg_groups(),
            target.get_status().await.unwrap().pg_groups
        );
        assert_eq!((listed.pg_groups(), listed.links()), (1, 0));
    });
}

/// Counts the turns it handles and records that the last one ended.
struct Counted {
    handled: Arc<AtomicUsize>,
}

impl Actor for Counted {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.handled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn a_switch_that_moved_between_the_two_reads_gives_no_answer() {
    both_flavors(|| async {
        let sys = system(true);
        let _actor = Idle.spawn().on_system(&sys).await.unwrap();
        settle(&sys).await;

        // Off and on again leaves the switch as it was, and turns that ran
        // in between left no counts.
        let hook_sys = sys.clone();
        sys.run_between_quiet_passes(move || {
            hook_sys.set_turn_tracking(false);
            hook_sys.set_turn_tracking(true);
        });
        let reader = sys.clone();
        let moved = tokio::task::spawn_blocking(move || reader.is_quiescent())
            .await
            .unwrap();
        assert_eq!(moved, None, "the switch moved during the call");
        assert_eq!(
            sys.is_quiescent(),
            Some(true),
            "a call with no move answers"
        );
    });
}
