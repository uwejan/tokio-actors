//! Spawn latency while the registry maps grow.
//!
//! Registering an actor takes one shard's write lock in the id map and, for a
//! named actor, in the name map. A table growth on that shard rehashes it
//! under the lock, so the spawn that triggers it waits for the whole rehash.
//! The registry maps use a large, constant shard count so each rehash covers a
//! small slice of the entries, whatever CPUs the process may use.

use std::time::{Duration, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorSystem, StopReason,
};

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// The number of live named actors the scale test holds. A process limited to
/// one CPU gets 4 shards by default, and 4 shards of 114,688 entries are
/// exhausted at 458,752 entries, so this count passes that table growth with
/// room to spare.
const LIVE_ACTORS: usize = 500_000;

/// The longest single spawn the scale test accepts with the registry at
/// `LIVE_ACTORS` entries in each map: 5 ms.
const WORST_SPAWN: Duration = Duration::from_micros(BOUND_US);
const BOUND_US: u64 = 5_000;

/// Spawns `LIVE_ACTORS` named idle actors on a current-thread runtime and
/// checks that no single spawn waits on a table growth.
///
/// Memory: the test holds every actor alive until the end and peaks at about
/// 2.3 GiB resident (about 4.6 KiB per actor, plus the name registrations), so
/// keep 3 GiB free. It is `#[ignore]` because of that memory and because the
/// bound is a wall clock latency; run it in release on a quiet machine with
/// `cargo test --release --test registry_scale -- --ignored`, and under
/// `taskset -c 0` to check the single-CPU case.
#[test]
#[ignore = "needs about 3 GiB and a quiet machine; bounds a wall clock latency"]
fn no_spawn_waits_on_a_registry_table_growth() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let sys = ActorSystem::create(format!("scale-{}", uuid::Uuid::new_v4())).unwrap();
        let mut live: Vec<ActorHandle<Idle>> = Vec::with_capacity(LIVE_ACTORS);
        let mut worst = Duration::ZERO;
        for i in 0..LIVE_ACTORS {
            let name = format!("scale-{i}");
            let started = Instant::now();
            let handle = Idle.spawn().named(name).on_system(&sys).await.unwrap();
            worst = worst.max(started.elapsed());
            live.push(handle);
        }
        for handle in &live {
            handle.stop(StopReason::Graceful).await.unwrap();
        }
        for handle in &live {
            handle.wait_stopped().await;
        }
        assert!(
            worst <= WORST_SPAWN,
            "the worst single spawn took {worst:?}, over the {WORST_SPAWN:?} bound"
        );
    });
}
