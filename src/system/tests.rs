use super::*;
use crate::actor::incarnation::StatusValue;
use std::sync::atomic::AtomicBool;
use tokio::sync::mpsc;

/// The outcomes of a report's entries, without their reasons.
fn outcomes_of(entries: &[(ActorId, StopReport)]) -> Vec<(ActorId, StopOutcome)> {
    entries
        .iter()
        .map(|(id, report)| (id.clone(), report.outcome))
        .collect()
}

use crate::actor::supervision::{ChildLinkGuard, DEFAULT_KILL_GRACE};

#[derive(Default)]
struct Dummy;

impl Actor for Dummy {
    type Message = ();
    type Response = ();

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        Ok(())
    }
}

fn dummy_handle(id: &ActorId, _system_name: &str) -> ActorHandle<Dummy> {
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (_status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, _stop_rx) = StopLane::new();
    ActorHandle::new(
        id.clone(),
        tx,
        stop_lane,
        1,
        status_rx,
        None,
        Some(Arc::new(std::sync::atomic::AtomicBool::new(false))),
    )
}

// The registry map is built with the same shard count whatever CPUs the
// process may use: a table growth then rehashes a small slice of the
// entries instead of a quarter or a sixteenth of them. The literal bound
// keeps the count itself from being lowered without this test noticing.
#[tokio::test]
async fn registry_maps_keep_the_shard_count_on_any_cpu_set() {
    let sys = ActorSystem::create(format!("shards-{}", uuid::Uuid::new_v4())).unwrap();
    let shards = sys.by_id.shards().len();
    assert_eq!(shards, REGISTRY_SHARDS, "by_id shard count");
    assert!(shards >= 256, "by_id has only {shards} shards");
    assert_eq!(registry_map::<u8, u8>().shards().len(), REGISTRY_SHARDS);
}

/// Holds its `on_stopped` until released, so a lookup can run while the
/// actor ends.
struct HeldEnd {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl Actor for HeldEnd {
    type Message = ();
    type Response = ();

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

// The longest single insert while a map takes `keys` one by one, each
// timed alone; the map is dropped before the caller builds the next one.
fn longest_insert<V>(map: DashMap<String, V>, keys: &[String], value: impl Fn() -> V) -> Duration {
    let mut longest = Duration::ZERO;
    for key in keys {
        let key = key.clone();
        let started = std::time::Instant::now();
        map.insert(key, value());
        longest = longest.max(started.elapsed());
    }
    longest
}

// A table growth moves at most the entries of one shard, and the shards
// are the registry's: the map has the registry's shard count, and with
// half a million entries no shard holds more than twice its even share.
// Those two bound the entries one growth step moves under one shard's
// write lock, with no clock in the test. A map of 4 or 16 shards fails the
// count, and a hash that piles keys into few shards fails the largest
// shard.
#[test]
fn a_table_growth_at_half_a_million_entries_moves_a_bounded_slice_of_the_map() {
    const ENTRIES: usize = 500_000;
    let map = registry_map::<String, u8>();
    let shards = map.shards().len();
    assert_eq!(shards, REGISTRY_SHARDS, "the registry map's shard count");
    assert!(shards >= 256, "the registry map has only {shards} shards");
    for i in 0..ENTRIES {
        map.insert(format!("actor-{i}"), 0);
    }
    let largest = map.shards().iter().map(|s| s.read().len()).max().unwrap();
    let even_share = ENTRIES.div_ceil(shards);
    assert!(
        largest <= 2 * even_share,
        "the largest of {shards} shards holds {largest} of {ENTRIES} entries, \
             more than twice the even share of {even_share}"
    );
}

// The stall is in the map, not in the actors: a map of the registry's
// shard count takes half a million entries with no insert waiting on the
// rehash of a large table, while a map of 4 shards (the count a process
// limited to one CPU gets by default) rehashes a quarter of its entries
// under one shard's write lock. 500,000 entries pass the 458,752 at which
// each of 4 shards doubles its largest table (4 * 114,688), so the
// baseline includes that rehash. The value is the size of a registry
// entry, so a rehash moves the bytes a real entry moves. A rehash stall
// shows in every repeat, a scheduling hiccup in one, so the registry map
// is filled again, up to six times, until the smallest longest insert of
// its repeats meets both bounds.
#[test]
#[ignore = "wall-time bound for a quiet CPU; the structure test covers the default run"]
fn a_table_growth_at_half_a_million_entries_never_stalls_an_insert() {
    type Entry = [u8; std::mem::size_of::<AnyActorHandle>()];
    const ENTRIES: usize = 500_000;
    let keys: Vec<String> = (0..ENTRIES).map(|i| format!("actor-{i}")).collect();
    let entry = || -> Entry { [0; std::mem::size_of::<AnyActorHandle>()] };
    let four_shards = (0..2)
        .map(|_| longest_insert(DashMap::with_capacity_and_shard_amount(0, 4), &keys, entry))
        .min()
        .unwrap();
    let holds = |worst: Duration| worst * 8 <= four_shards && worst <= Duration::from_millis(5);
    let mut shipped = longest_insert(registry_map(), &keys, entry);
    let mut repeats = 1;
    while repeats < 6 && !holds(shipped) {
        shipped = shipped.min(longest_insert(registry_map(), &keys, entry));
        repeats += 1;
    }
    assert!(
            shipped * 8 <= four_shards,
            "the registry map's worst insert {shipped:?} is not far below a 4-shard map's {four_shards:?}"
        );
    assert!(
        shipped <= Duration::from_millis(5),
        "the registry map's worst insert took {shipped:?}"
    );
}

// - Reaper ----------------------------------------------------------------

/// An entry that is dropped without having fired aborts its task; one that
/// fired, or whose task already ended, does nothing more.
#[tokio::test]
async fn reaper_entry_dropped_before_its_deadline_aborts_its_task() {
    // A task that is not aborted fails the test at this bound instead of
    // hanging it.
    async fn ended(child: tokio::task::JoinHandle<()>) -> Result<(), tokio::task::JoinError> {
        tokio::time::timeout(Duration::from_secs(5), child)
            .await
            .expect("the task was aborted when its entry was dropped")
    }

    let child = tokio::spawn(std::future::pending::<()>());
    let entry = ReaperEntry::new(
        Instant::now() + Duration::from_secs(30),
        child.abort_handle(),
        IncarnationRef::new(),
    );
    drop(entry);
    assert!(ended(child).await.unwrap_err().is_cancelled());

    let child = tokio::spawn(std::future::pending::<()>());
    let entry = ReaperEntry::new(Instant::now(), child.abort_handle(), IncarnationRef::new());
    entry.fire();
    assert!(ended(child).await.unwrap_err().is_cancelled());

    let done = tokio::spawn(async {});
    let abort = done.abort_handle();
    done.await.unwrap();
    drop(ReaperEntry::new(
        Instant::now(),
        abort,
        IncarnationRef::new(),
    ));
}

/// A task that never ends on its own, with a lane to raise on.
fn parked_with_a_lane() -> (
    tokio::task::JoinHandle<()>,
    StopLane,
    crate::actor::incarnation::LaneRx,
) {
    let (lane, lane_rx) = StopLane::new();
    (tokio::spawn(std::future::pending::<()>()), lane, lane_rx)
}

async fn turns() {
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
}

/// A plane that holds `status`, `reason` and the instant `final_at` (the
/// cell keeps its last value once its sender is gone).
fn plane(status: ActorStatus, reason: StopReason, final_at: Option<Instant>) -> StatusRx {
    StatusTx::channel(StatusValue {
        status,
        reason: Some(reason),
        final_at,
    })
    .1
}

// - The sweep runs only while something asks for it ------------------------

/// A system nobody asks anything of starts no task of its own, however
/// many actors register and however long it lives.
async fn nothing_asked_starts_nothing() {
    use crate::actor::ActorExt;

    let sys = ActorSystem::create(format!("sweep-none-{}", uuid::Uuid::new_v4())).unwrap();
    let mut kept = Vec::new();
    for _ in 0..4 {
        kept.push(Dummy.spawn().on_system(&sys).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(sys.live_helpers(), 0);
    assert!(!sys.sweep_spawned.load(Ordering::Acquire));
    drop(kept);
}

#[tokio::test(flavor = "current_thread")]
async fn nothing_asked_starts_nothing_current_thread() {
    nothing_asked_starts_nothing().await;
}

#[cfg(feature = "telemetry")]
/// The sweep ends by itself once nothing asks for it, and a want that
/// lands after it decided to end and before its latch clears still has a
/// sweep running afterwards.
async fn the_sweep_ends_unasked_and_a_want_in_its_exit_window_restarts_it() {
    use crate::helper::HelperEnd;
    use crate::telemetry::{EventKind, KindSet};

    let sys = ActorSystem::create(format!("sweep-exit-{}", uuid::Uuid::new_v4())).unwrap();
    let kinds = KindSet::of(&[EventKind::TurnStuck]);
    assert!(sys.attach_telemetry("first", kinds, |_| {}));
    assert!(
        sys.sweep_spawned.load(Ordering::Acquire),
        "a handler for a sampled kind starts the sweep"
    );

    let landed = Arc::new(AtomicBool::new(false));
    let weak = Arc::downgrade(&sys);
    let flag = landed.clone();
    sys.run_before_sweep_latch_clear(move || {
        let sys = weak.upgrade().expect("the system is alive");
        assert!(
            sys.sweep_spawned.load(Ordering::Acquire),
            "the latch is still set in the exit window"
        );
        assert!(sys.attach_telemetry("second", kinds, |_| {}));
        flag.store(true, Ordering::SeqCst);
    });
    assert!(sys.detach_telemetry("first"));

    let rt = Handle::current();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !sys
        .helper_ends(&rt)
        .iter()
        .any(|(kind, end)| *kind == HelperKind::Sweep && *end == HelperEnd::Finished)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the sweep did not end by itself"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        landed.load(Ordering::SeqCst),
        "the want landed in the exit window"
    );
    assert!(
        sys.sweep_spawned.load(Ordering::Acquire),
        "a sweep runs for the want that landed in the window"
    );
    assert_eq!(sys.live_helpers(), 1);
}

#[cfg(feature = "telemetry")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sweep_ends_unasked_and_a_want_in_its_exit_window_restarts_it_multi_thread() {
    the_sweep_ends_unasked_and_a_want_in_its_exit_window_restarts_it().await;
}

// - Turn-watchdog sweep latch ---------------------------------------------

#[cfg(feature = "telemetry")]
/// Builds a bare, unstarted single-threaded runtime for driving the
/// sweep by hand - not a `#[tokio::test]` function, since the whole
/// point is to control exactly when a runtime exists and when it is
/// dropped.
#[cfg(feature = "tracing")]
#[test]
fn sweep_latch_clears_when_its_runtime_drops_and_respawns_on_the_next_one() {
    let sys = ActorSystem::create(format!("sweep-latch-{}", uuid::Uuid::new_v4())).unwrap();

    let rt_a = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt_a.block_on(async {
        sys.set_turn_warning_threshold(Some(Duration::from_millis(50)));
        // Give the sweep task a chance to run at least one iteration
        // before its runtime disappears, so this phase exercises the
        // steady-state path: a task that is alive and has actually been
        // polled. The third phase below exercises the opposite case, a
        // task dropped before it is ever polled at all.
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    assert!(
        sys.sweep_spawned.load(Ordering::Acquire),
        "the sweep task should be alive after starting it inside runtime A"
    );

    drop(rt_a);
    assert!(
        !sys.sweep_spawned.load(Ordering::Acquire),
        "dropping the sweep's own runtime must clear the latch, not leave it stuck forever"
    );

    let rt_b = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt_b.block_on(async {
        // The threshold is already `Some` from runtime A; setting it
        // again inside runtime B must still respawn the sweep, since
        // the latch cleared when runtime A dropped.
        sys.set_turn_warning_threshold(Some(Duration::from_millis(50)));
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    assert!(
        sys.sweep_spawned.load(Ordering::Acquire),
        "a threshold set inside a fresh runtime must respawn the sweep"
    );

    rt_b.block_on(async {
        sys.set_turn_warning_threshold(None);
    });
    drop(rt_b);
    assert!(
        !sys.sweep_spawned.load(Ordering::Acquire),
        "dropping runtime B must clear the latch too, ready for the next phase"
    );

    // Phase 3: a sweep task spawned but never polled at all - its
    // runtime disappears before anything ever gives it a turn. The
    // compare-exchange in `ensure_sweep_spawned_with` sets the latch
    // synchronously, before the future is even spawned, so an unpolled
    // task still leaving it stuck would break the guarantee: the guard is
    // built and moved into the future BEFORE the spawn, not constructed
    // as the first line of the future's own body, so it drops - and
    // clears the latch - along with the future's other captures even
    // when the future body never runs.
    let rt_c = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    {
        let _enter = rt_c.enter();
        sys.set_turn_warning_threshold(Some(Duration::from_millis(50)));
    }
    assert!(
        sys.sweep_spawned.load(Ordering::Acquire),
        "the latch is set as soon as the sweep is spawned, whether or not it is ever polled"
    );
    drop(rt_c);
    assert!(
        !sys.sweep_spawned.load(Ordering::Acquire),
        "a sweep task dropped before its first poll must still clear the latch"
    );

    sys.set_turn_warning_threshold(None);
}

#[cfg(feature = "telemetry")]
/// A runtime without the time driver is never used for the sweep: a
/// threshold set while one is current starts nothing, and the next call on
/// a runtime with the driver starts it.
#[cfg(feature = "tracing")]
#[test]
fn the_sweep_is_not_started_on_a_runtime_without_the_time_driver() {
    let sys = ActorSystem::create(format!("sweep-bare-{}", uuid::Uuid::new_v4())).unwrap();

    let bare = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    {
        let _enter = bare.enter();
        sys.set_turn_warning_threshold(Some(Duration::from_millis(50)));
    }
    assert!(
        !sys.sweep_spawned.load(Ordering::Acquire),
        "no sweep task is spawned on a runtime without the time driver"
    );
    assert_eq!(sys.live_helpers(), 0);

    let timed = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    {
        let _enter = timed.enter();
        sys.set_turn_warning_threshold(Some(Duration::from_millis(50)));
    }
    assert!(
        sys.sweep_spawned.load(Ordering::Acquire),
        "the next call on a runtime with the driver starts the sweep"
    );
    assert_eq!(sys.live_helpers(), 1);

    drop(timed);
    assert_eq!(sys.live_helpers(), 0);
    sys.set_turn_warning_threshold(None);
}

/// The declared `Kill` deadline is one `u64` on a registry entry: a named
/// actor has two entries (by id and by name), so it costs 16 bytes, an
/// anonymous one 8. The entry is exactly the size its fields need, the
/// deadline included, rounded up to the 8-byte alignment; a second word,
/// a wider deadline or a stray word of padding fails here.
#[test]
fn a_registry_entry_costs_at_most_eight_bytes_for_its_kill_deadline() {
    use std::mem::size_of;
    let without_deadline = size_of::<TypeId>()
        + size_of::<bool>()
        + size_of::<Box<dyn Any + Send + Sync>>()
        + size_of::<StopLane>()
        + size_of::<StatusRx>()
        + size_of::<crate::actor::incarnation::TurnClock>()
        + size_of::<Option<AbortHandle>>()
        + size_of::<Option<(ActorId, u64)>>()
        + size_of::<u64>()
        + size_of::<Duration>()
        + size_of::<Handle>();
    assert_eq!(std::mem::align_of::<AnyActorHandle>(), 8);
    let exact = (without_deadline + size_of::<u64>()).div_ceil(8) * 8;
    assert_eq!(
        size_of::<AnyActorHandle>(),
        exact,
        "an entry is {} bytes; its fields and the one deadline word need {exact}",
        size_of::<AnyActorHandle>()
    );
}

// - Reaper survives a runtime change --------------------------------------

/// Mirrors the sweep-latch test's shape: two hand-built multi-thread
/// runtimes on one system, driven by hand (not `#[tokio::test]`) so the
/// test controls exactly when each runtime exists and when it drops.
///
/// Runtime A hosts the system's first reaper, then is dropped out from
/// under it: the stored sender's receiver goes away with it, so a
/// send against it comes back closed. A guard holding that now-
/// stale sender, dropped later inside runtime B, must still honor its
/// child's full grace via a freshly resolved reaper on B - not degrade
/// straight to an immediate abort. The child's future records the
/// instant it is torn down, so the check reads when the abort actually
/// happened: a late wake or a busy worker only makes that instant later,
/// never earlier, and the bound holds on any host.
#[test]
fn guard_drop_across_a_runtime_change_still_honors_the_grace() {
    let sys =
        ActorSystem::create(format!("reaper-runtime-change-{}", uuid::Uuid::new_v4())).unwrap();

    // Host the FIRST reaper on runtime A, then drop A out from under it.
    let rt_a = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let stale_tx = sys.reaper_handle(rt_a.handle());
    drop(rt_a);

    let rt_b = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let grace = Duration::from_millis(100);
    let torn_down = Arc::new(Mutex::new(None::<Instant>));

    /// Records the instant the future holding it is dropped.
    struct TornDown(Arc<Mutex<Option<Instant>>>);
    impl Drop for TornDown {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = Some(Instant::now());
        }
    }

    rt_b.block_on(async move {
        let stamp = TornDown(torn_down.clone());
        let join: tokio::task::JoinHandle<()> = tokio::spawn(async move {
            let _stamp = stamp;
            std::future::pending::<()>().await;
        });
        let abort = join.abort_handle();
        let (lane, _lane_rx) = StopLane::new();

        let guard = ChildLinkGuard::new(
            lane,
            abort,
            stale_tx,
            crate::actor::supervision::rules::InheritedStop::Kill { grace },
            sys.clone(),
            Handle::current(),
        );
        let dropped_at = Instant::now();
        drop(guard);

        // The fresh reaper on runtime B aborts the child once the
        // original grace elapses; the bound here only catches a hang.
        let result = tokio::time::timeout(Duration::from_secs(10), join).await;
        match result {
            Ok(Err(join_err)) => assert!(
                join_err.is_cancelled(),
                "the fresh reaper on the new runtime must still abort the child \
                     once its grace elapses"
            ),
            other => panic!("expected the child to be aborted by its grace, got {other:?}"),
        }
        let aborted_at = torn_down
            .lock()
            .unwrap()
            .expect("the abort dropped the child's future");
        let waited = aborted_at.saturating_duration_since(dropped_at);
        assert!(
            waited >= grace,
            "the child must live out its full {grace:?} grace, aborted after \
                 {waited:?}: an earlier abort means the runtime switch degraded \
                 instead of retrying on the current reaper"
        );
    });
}

// - The reaper follows the runtime -----------------------------------------

/// Parks in `handle` once messaged, and says so.
struct Parker {
    parked: Arc<AtomicBool>,
}

impl Parker {
    fn new() -> (Parker, Arc<AtomicBool>) {
        let parked = Arc::new(AtomicBool::new(false));
        (
            Parker {
                parked: parked.clone(),
            },
            parked,
        )
    }
}

impl Actor for Parker {
    type Message = ();
    type Response = ();

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        self.parked.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        Ok(())
    }
}

fn undriven_host() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

// - A Kill raised between an entry's lookup and its abort being stored ----

/// A registered entry whose task is spawned but whose abort handle is not
/// stored yet; the hook stores it at the point between a system-side
/// kill's lookup and its raise. The returned task owns the status sender
/// and the lane's receiver, so its end is observable.
fn pre_attach_target(
    grace: Duration,
) -> (
    Arc<ActorSystem>,
    String,
    ActorId,
    tokio::task::JoinHandle<()>,
) {
    let sys = ActorSystem::create(format!("pre-attach-hook-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("hook-target-{}", uuid::Uuid::new_v4());
    let id = ActorId::from(name.clone());
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, stop_rx) = StopLane::new();
    let handle = ActorHandle::<Dummy>::new(
        id.clone(),
        tx,
        stop_lane,
        1,
        status_rx,
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    let seq = sys
        .register_actor::<Dummy>(
            &id,
            true,
            &handle,
            true,
            TurnClock::default(),
            grace,
            Handle::current(),
        )
        .unwrap();
    let task = tokio::spawn(async move {
        let _owned = (status_tx, stop_rx);
        std::future::pending::<()>().await;
    });
    let abort = task.abort_handle();
    let hook_sys = sys.clone();
    let hook_id = id.clone();
    *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
        hook_sys.attach_abort(&hook_id, abort.clone(), seq);
    }));
    (sys, name, id, task)
}

async fn ends_by_the_backstop(task: tokio::task::JoinHandle<()>, grace: Duration, what: &str) {
    let ended = tokio::time::timeout(grace + ABORT_SETTLE + Duration::from_millis(500), task)
        .await
        .unwrap_or_else(|_| panic!("{what}: no backstop was armed"));
    assert!(ended.unwrap_err().is_cancelled());
}

/// Registers a root whose task owns its status sender and lane receiver,
/// with its abort handle stored, and returns the task.
fn registered_root(
    sys: &Arc<ActorSystem>,
    n: usize,
    grace: Duration,
) -> tokio::task::JoinHandle<()> {
    let id = ActorId::from(format!("sweep-{n}"));
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, stop_rx) = StopLane::new();
    let handle = ActorHandle::<Dummy>::new(
        id.clone(),
        tx,
        stop_lane,
        1,
        status_rx,
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    let seq = sys
        .register_actor::<Dummy>(
            &id,
            false,
            &handle,
            true,
            TurnClock::default(),
            grace,
            Handle::current(),
        )
        .unwrap();
    let task = tokio::spawn(async move {
        let _owned = (status_tx, stop_rx);
        std::future::pending::<()>().await;
    });
    sys.attach_abort(&id, task.abort_handle(), seq);
    task
}

const HOOK_GRACE: Duration = Duration::from_millis(100);

/// A grace far longer than the shutdown futures of the dropped-call tests
/// are polled for, so only the system's own backstop can end the target
/// before it.
const SLOW_GRACE: Duration = Duration::from_millis(1000);

async fn kill_by_name_arms_when_the_abort_is_stored_after_the_lookup() {
    let (sys, name, _id, task) = pre_attach_target(HOOK_GRACE);
    sys.kill(&name).await.unwrap();
    ends_by_the_backstop(task, HOOK_GRACE, "kill(name)").await;
}

async fn kill_by_id_arms_when_the_abort_is_stored_after_the_lookup() {
    let (sys, _name, id, task) = pre_attach_target(HOOK_GRACE);
    // The call is dropped at once: only the system's backstop is left.
    let _ = tokio::time::timeout(Duration::ZERO, sys.kill_by_id(&id)).await;
    ends_by_the_backstop(task, HOOK_GRACE, "kill_by_id").await;
}

async fn shutdown_kill_step_arms_when_the_abort_is_stored_after_the_lookup() {
    let (sys, _name, _id, task) = pre_attach_target(SLOW_GRACE);
    let policy = ShutdownPolicy {
        timeout: Duration::from_secs(30),
        per_actor_timeout: Duration::from_millis(10),
    };
    let _ = tokio::time::timeout(Duration::from_millis(60), sys.shutdown_with(policy)).await;
    ends_by_the_backstop(task, SLOW_GRACE, "the shutdown's Kill step").await;
}

async fn force_sweep_arms_when_the_abort_is_stored_after_the_lookup() {
    let (sys, _name, _id, task) = pre_attach_target(SLOW_GRACE);
    let policy = ShutdownPolicy {
        timeout: Duration::from_millis(10),
        per_actor_timeout: Duration::from_secs(5),
    };
    let _ = tokio::time::timeout(Duration::from_millis(60), sys.shutdown_with(policy)).await;
    ends_by_the_backstop(task, SLOW_GRACE, "the concurrent force sweep").await;
}

// - A Kill arms the incarnation it was raised on, and only it -------------

/// Registers an incarnation under `id` (as its name when `named`): a live lane and
/// status plane owned by a pending task that also holds the registration
/// guard, so the task's end unregisters it. Its abort handle is stored.
fn register_incarnation(
    sys: &Arc<ActorSystem>,
    id: &ActorId,
    named: bool,
    grace: Duration,
) -> (u64, tokio::task::JoinHandle<()>) {
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, stop_rx) = StopLane::new();
    let handle = ActorHandle::<Dummy>::new(
        id.clone(),
        tx,
        stop_lane,
        1,
        status_rx,
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    let seq = sys
        .register_actor::<Dummy>(
            id,
            named,
            &handle,
            true,
            TurnClock::default(),
            grace,
            Handle::current(),
        )
        .unwrap();
    let guard = RegistryGuard::new(sys.clone(), id.clone(), seq);
    let task = tokio::spawn(async move {
        let _owned = (status_tx, stop_rx, guard);
        std::future::pending::<()>().await;
    });
    sys.attach_abort(id, task.abort_handle(), seq);
    (seq, task)
}

#[derive(Clone, Copy, Debug)]
enum KillSite {
    ByName,
    ById,
    ShutdownKillStep,
    ForceSweep,
}

/// Between a kill's raise and its re-read of the registry, the incarnation
/// it raised on is replaced by a successor under the same id and name:
/// the successor's abort is never armed.
async fn a_kill_arms_only_the_incarnation_it_was_raised_on(site: KillSite) {
    let sys = ActorSystem::create(format!("successor-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("successor-target-{}", uuid::Uuid::new_v4());
    let id = ActorId::from(name.clone());
    let (seq, old) = register_incarnation(&sys, &id, true, SLOW_GRACE);
    let successor: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    {
        let (hook_sys, hook_id, slot) = (sys.clone(), id.clone(), successor.clone());
        *sys.after_kill_raise.lock().unwrap() = Some(Arc::new(move || {
            hook_sys.unregister_by_id(&hook_id, seq);
            // A shutdown refuses registrations; the successor stands for
            // the next incarnation of the same id.
            let phase = hook_sys
                .phase
                .swap(SystemPhase::Active.as_u8(), Ordering::SeqCst);
            let (_, task) = register_incarnation(&hook_sys, &hook_id, true, HOOK_GRACE);
            hook_sys.phase.store(phase, Ordering::SeqCst);
            *slot.lock().unwrap() = Some(task);
        }));
    }
    match site {
        KillSite::ByName => {
            sys.kill(&name).await.unwrap();
        }
        KillSite::ById => {
            let _ = tokio::time::timeout(Duration::ZERO, sys.kill_by_id(&id)).await;
        }
        KillSite::ShutdownKillStep | KillSite::ForceSweep => {
            let policy = match site {
                KillSite::ShutdownKillStep => ShutdownPolicy {
                    timeout: Duration::from_secs(30),
                    per_actor_timeout: Duration::from_millis(10),
                },
                _ => ShutdownPolicy {
                    timeout: Duration::from_millis(10),
                    per_actor_timeout: Duration::from_secs(5),
                },
            };
            // The shutdown is driven until the hook has put the successor
            // in place, however long the host takes to get there. The
            // arm decision follows the hook inside the same poll, so the
            // next look at the slot sees a decided arm.
            let mut shutdown = Box::pin(sys.shutdown_with(policy));
            let replaced = async {
                while successor.lock().unwrap().is_none() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            };
            let driven = async {
                tokio::select! {
                    _ = &mut shutdown => {}
                    _ = replaced => {}
                }
            };
            tokio::time::timeout(Duration::from_secs(20), driven)
                .await
                .expect("the shutdown reached its Kill");
        }
    }
    let successor = successor
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| panic!("{site:?}: the incarnation was never replaced"));
    // Well past the successor's grace: nothing aborts it, because the
    // Kill was raised on the incarnation before it.
    tokio::time::sleep(HOOK_GRACE * 4).await;
    assert!(
        !successor.is_finished(),
        "{site:?}: the Kill armed the abort of the successor incarnation"
    );
    successor.abort();
    old.abort();
}

// - The force sweep reports a root gone before it looked ------------------

/// Two roots; the sequential ladder is cut at the first (global timeout
/// spent) so both reach the sweep. The second was aborted by another
/// caller's Kill and left the registry before the sweep looked: its
/// verdict comes from the status receiver its snapshot kept, as the
/// sequential ladder's would.
async fn the_sweep_classifies_a_root_gone_before_it_looked() {
    let sys = ActorSystem::create(format!("sweep-gone-{}", uuid::Uuid::new_v4())).unwrap();
    let (aborted_id, parked_id) = (
        ActorId::from("sweep-aborted"),
        ActorId::from("sweep-parked"),
    );
    // Registered first, so it is stopped last and swept as a straggler.
    let (_seq, aborted_task) =
        register_incarnation(&sys, &aborted_id, true, Duration::from_millis(40));
    let (_seq, parked_task) =
        register_incarnation(&sys, &parked_id, true, Duration::from_millis(100));
    let shutdown = {
        let sys = sys.clone();
        tokio::spawn(async move {
            sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_millis(2400),
                per_actor_timeout: Duration::from_secs(5),
            })
            .await
        })
    };
    // Another caller kills the first-registered root while the ladder is
    // still waiting on the second.
    tokio::time::sleep(Duration::from_millis(20)).await;
    sys.kill("sweep-aborted").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), aborted_task)
        .await
        .expect("the kill's backstop aborts the root")
        .unwrap_err();
    assert!(sys.get_by_id::<Dummy>(&aborted_id).is_none(), "gone");
    let report = tokio::time::timeout(Duration::from_secs(10), shutdown)
        .await
        .expect("the shutdown returns")
        .unwrap();
    let outcome = |id: &ActorId| {
        report
            .outcomes
            .iter()
            .find(|(reported, _)| reported == id)
            .map(|(_, report)| report.outcome)
    };
    assert_eq!(
        outcome(&aborted_id),
        Some(StopOutcome::Aborted),
        "a root aborted before the sweep looked is Aborted, as in the sequential ladder"
    );
    assert!(outcome(&parked_id).is_some());
    parked_task.abort();
}

/// An attach that comes after its incarnation's registration was
/// replaced (the name and the id freed, and a later spawn registered
/// them) leaves the new entries' abort handle as it is, and arms no
/// backstop through them, even when the new entry's lane holds a `Kill`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_attach_leaves_the_new_registration_s_abort_its_own() {
    let sys = ActorSystem::create(format!("stale-attach-{}", uuid::Uuid::new_v4())).unwrap();
    let name = "stale-attach-name";
    let id = ActorId::from(name);
    let register = |sys: &Arc<ActorSystem>| {
        let (tx, _rx) = crate::actor::mailbox::test_channel(1);
        let (_status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, _stop_rx) = StopLane::new();
        let handle = ActorHandle::<Dummy>::new(
            id.clone(),
            tx,
            stop_lane.clone(),
            1,
            status_rx,
            None,
            Some(Arc::new(AtomicBool::new(false))),
        );
        let seq = sys
            .register_actor::<Dummy>(
                &id,
                true,
                &handle,
                true,
                TurnClock::default(),
                Duration::from_secs(60),
                Handle::current(),
            )
            .unwrap();
        (seq, stop_lane)
    };
    // The first incarnation registers, and its entry is freed before its
    // attach runs.
    let (old_seq, _old_lane) = register(&sys);
    sys.unregister_by_id(&id, old_seq);
    // A later spawn registers the same name and attaches its own abort;
    // its lane then holds a `Kill`.
    let (new_seq, new_lane) = register(&sys);
    let new_task = tokio::spawn(std::future::pending::<()>());
    sys.attach_abort(&id, new_task.abort_handle(), new_seq);
    new_lane.raise(StopReason::Kill);
    let arms = sys.backstop_arms.load(Ordering::SeqCst);
    let old_task = tokio::spawn(std::future::pending::<()>());
    sys.attach_abort(&id, old_task.abort_handle(), old_seq);
    let by_id = sys
        .by_id
        .get(&id)
        .and_then(|entry| entry.abort.as_ref().map(|a| a.id()));
    let by_name = sys
        .by_name(name)
        .and_then(|entry| entry.abort.as_ref().map(|a| a.id()));
    assert_eq!(
        by_id,
        Some(new_task.id()),
        "the id's entry keeps the new abort"
    );
    assert_eq!(
        by_name,
        Some(new_task.id()),
        "the name's entry keeps the new abort"
    );
    assert_eq!(
        sys.backstop_arms.load(Ordering::SeqCst),
        arms,
        "the stale attach armed no backstop"
    );
    new_task.abort();
    old_task.abort();
}

/// Supervises one child that parks in `handle`, stopped under
/// `Shutdown::Kill` on command.
struct Boss {
    child: &'static str,
    parked: Arc<AtomicBool>,
}

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn on_started(
        &mut self,
        ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        let parked = self.parked.clone();
        ctx.spawn_child(move || Parker {
            parked: parked.clone(),
        })
        .named(self.child)
        .shutdown(crate::types::Shutdown::Kill)
        .await?;
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: (),
        ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        ctx.terminate_child(self.child).await?;
        Ok(())
    }
}

/// Supervises one parked child and parks itself in `handle` on command,
/// so a `Kill` raised on it is held by its lane and never observed: its
/// registry entry outlives every lookup a kill path makes.
struct ParkingBoss {
    parked: Arc<AtomicBool>,
    child_parked: Arc<AtomicBool>,
}

impl Actor for ParkingBoss {
    type Message = ();
    type Response = ();

    async fn on_started(
        &mut self,
        ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        let parked = self.child_parked.clone();
        ctx.spawn_child(move || Parker {
            parked: parked.clone(),
        })
        .named("guard-child")
        .await?;
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        self.parked.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// A supervised child that crashes on command; every incarnation after
/// the first hangs in its initialization, so a restart attempt's start
/// timeout kills it.
struct HangsOnRestart {
    builds: Arc<AtomicU64>,
}

impl Actor for HangsOnRestart {
    type Message = ();
    type Response = ();

    async fn on_started(
        &mut self,
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        if self.builds.load(Ordering::SeqCst) > 1 {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        Err(crate::error::ActorError::user("crash on command"))
    }
}

struct RestartBoss {
    builds: Arc<AtomicU64>,
}

impl Actor for RestartBoss {
    type Message = ();
    type Response = ();

    async fn on_started(
        &mut self,
        ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        let builds = self.builds.clone();
        ctx.spawn_child(move || {
            builds.fetch_add(1, Ordering::SeqCst);
            HangsOnRestart {
                builds: builds.clone(),
            }
        })
        .named("restart-child")
        .start_timeout(Duration::from_millis(50))
        .await?;
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        Ok(())
    }
}

/// Counts the entries every kill path hands to this runtime's reaper.
fn counting_reaper(sys: &ActorSystem) -> mpsc::UnboundedReceiver<ReaperEntry> {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(std::future::pending::<()>()).abort_handle();
    sys.reaper
        .lock()
        .unwrap()
        .insert(Handle::current().id(), ReaperSlot { tx, task });
    rx
}

/// Polls until `ready` holds. The bound only limits how long a real hang takes to report.
async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..12_000 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn drained(entries: &mut mpsc::UnboundedReceiver<ReaperEntry>) -> usize {
    let mut n = 0;
    while entries.try_recv().is_ok() {
        n += 1;
    }
    n
}

async fn backstop_entries_by_kill_path() {
    use crate::actor::ActorExt;

    let sys = ActorSystem::create(format!("reaper-count-{}", uuid::Uuid::new_v4())).unwrap();
    let mut entries = counting_reaper(&sys);
    let arms = || sys.backstop_arms.load(Ordering::SeqCst);

    // One `kill_by_id` of a parked root arms exactly once and queues
    // exactly one entry.
    let (parker, parked) = Parker::new();
    let root = parker
        .spawn()
        .named("count-root")
        .on_system(&sys)
        .await
        .unwrap();
    root.notify(()).await.unwrap();
    until("the root to park", || parked.load(Ordering::SeqCst)).await;
    assert!(sys.kill_by_id(root.id()).await.is_some());
    assert_eq!(arms(), 1, "one kill, one arming");
    assert_eq!(drained(&mut entries), 1, "one kill, one backstop entry");

    // The supervisor's own stop of a child under `Shutdown::Kill` aborts
    // through the runner's ladder, which arms nothing.
    let parked = Arc::new(AtomicBool::new(false));
    let boss = Boss {
        child: "ladder-child",
        parked: parked.clone(),
    }
    .spawn()
    .named("count-boss")
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();
    let child = loop {
        if let Some(child) = sys.get::<Parker>("ladder-child") {
            break child;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    child.notify(()).await.unwrap();
    until("the child to park", || parked.load(Ordering::SeqCst)).await;
    boss.notify(()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), child.wait_stopped())
        .await
        .expect("the child is stopped by its supervisor's ladder");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(arms(), 1, "the runner's ladder arms nothing");
    assert_eq!(drained(&mut entries), 0);

    // A killed supervisor arms its own backstop once (its handle's
    // `stop(Kill)`); its child's link guard schedules an entry of its own
    // and arms nothing through the system. The supervisor is parked in a
    // turn, so the `Kill` is held by its lane and the lookup `stop(Kill)`
    // makes always finds it registered: an idle one can observe the
    // `Kill` and leave the registry before that lookup, and a target
    // already out of the registry is past its last await.
    let parked = Arc::new(AtomicBool::new(false));
    let child_parked = Arc::new(AtomicBool::new(false));
    let boss = ParkingBoss {
        parked: parked.clone(),
        child_parked: child_parked.clone(),
    }
    .spawn()
    .named("count-guard-boss")
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();
    let child = loop {
        if let Some(child) = sys.get::<Parker>("guard-child") {
            break child;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    child.notify(()).await.unwrap();
    until("the child to park", || child_parked.load(Ordering::SeqCst)).await;
    boss.notify(()).await.unwrap();
    until("the supervisor to park", || parked.load(Ordering::SeqCst)).await;
    boss.stop(crate::types::StopReason::Kill).await.unwrap();
    assert_eq!(arms(), 2, "the supervisor's stop(Kill) arms once");
    // The reaper of this runtime is the counting one. The supervisor's
    // entry is queued by the time `stop` returns; dropping it aborts the
    // parked supervisor, whose dropped death plane fires the child's link
    // guard, which queues the second entry.
    let supervisor_entry = entries
        .try_recv()
        .expect("the supervisor's entry is queued");
    assert_eq!(drained(&mut entries), 0, "one entry per arming");
    drop(supervisor_entry);
    let guard_entry = tokio::time::timeout(Duration::from_secs(5), entries.recv())
        .await
        .expect("the guard's entry is scheduled");
    assert!(guard_entry.is_some());
    let _ = &child;
    assert_eq!(arms(), 2, "the guard arms nothing through the system");
    assert_eq!(drained(&mut entries), 0, "and nothing else was queued");

    // A restart attempt's start-timeout Kill arms nothing either.
    let builds = Arc::new(AtomicU64::new(0));
    let _restarter = RestartBoss {
        builds: builds.clone(),
    }
    .spawn()
    .named("count-restart-boss")
    .on_system(&sys)
    .supervisor()
    .await
    .unwrap();
    until("the child to be built", || {
        builds.load(Ordering::SeqCst) == 1
    })
    .await;
    let restart_child = loop {
        if let Some(child) = sys.get::<HangsOnRestart>("restart-child") {
            break child;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    restart_child.notify(()).await.unwrap();
    until("a restart attempt to be built", || {
        builds.load(Ordering::SeqCst) >= 2
    })
    .await;
    // The first attempt's incarnation hangs past its start timeout and is
    // killed, and the supervisor builds the next one: the Kill this case
    // counts has happened.
    until("the hung attempt to be killed and replaced", || {
        builds.load(Ordering::SeqCst) >= 3
    })
    .await;
    assert_eq!(
        arms(),
        2,
        "a restart attempt arms nothing through the system"
    );

    // The reaper's own Kill stage raises the Kill and queues nothing: its
    // entry already holds the abort.
    let (staged, lane, lane_rx) = parked_with_a_lane();
    let entry = ReaperEntry::two_stage(
        Instant::now(),
        lane,
        Instant::now() + Duration::from_secs(60),
        staged.abort_handle(),
    );
    drained(&mut entries);
    let rest = entry.fire().expect("the Kill stage hands its entry back");
    assert!(matches!(
        lane_rx.borrow().reason,
        Some(crate::types::StopReason::Kill)
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        entries.try_recv().is_err(),
        "the reaper's Kill stage queues no second entry"
    );
    assert_eq!(arms(), 2, "and arms nothing through the system");
    drop(rest);
}

// - Roots whose end a test drives step by step ----------------------------

/// A root incarnation whose end a test drives in the runtime's order:
/// `leave` frees its registry entry, and only afterwards does
/// `publish_stopped` write `Stopped` and close its status plane. Its lane
/// receiver belongs to a parked task whose abort handle is the entry's,
/// so a raise on its lane is recorded and an abort of it is observable.
struct Scripted {
    id: ActorId,
    handle: ActorHandle<Dummy>,
    lane: StopLane,
    guard: Mutex<Option<RegistryGuard>>,
    status_tx: Mutex<Option<StatusTx>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Scripted {
    /// Registers a root incarnation under `id` (also while a shutdown
    /// refuses registrations, so a test can stand a later incarnation of
    /// the same id in for the one a shutdown snapshotted).
    fn register(sys: &Arc<ActorSystem>, id: &ActorId, grace: Duration) -> Arc<Scripted> {
        Scripted::register_as(sys, id, grace, true)
    }

    /// As `register`, as a root or as a non-root (a supervised child's
    /// registration).
    fn register_as(
        sys: &Arc<ActorSystem>,
        id: &ActorId,
        grace: Duration,
        is_root: bool,
    ) -> Arc<Scripted> {
        let (tx, _rx) = crate::actor::mailbox::test_channel(1);
        let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, stop_rx) = StopLane::new();
        let handle = ActorHandle::<Dummy>::new(
            id.clone(),
            tx,
            stop_lane.clone(),
            1,
            status_rx,
            None,
            Some(Arc::new(AtomicBool::new(false))),
        );
        let phase = sys
            .phase
            .swap(SystemPhase::Active.as_u8(), Ordering::SeqCst);
        let seq = sys
            .register_actor::<Dummy>(
                id,
                false,
                &handle,
                is_root,
                TurnClock::default(),
                grace,
                Handle::current(),
            )
            .unwrap();
        sys.phase.store(phase, Ordering::SeqCst);
        let guard = RegistryGuard::new(sys.clone(), id.clone(), seq);
        let task = tokio::spawn(async move {
            let _owned = stop_rx;
            std::future::pending::<()>().await;
        });
        sys.attach_abort(id, task.abort_handle(), seq);
        Arc::new(Scripted {
            id: id.clone(),
            handle,
            lane: stop_lane,
            guard: Mutex::new(Some(guard)),
            status_tx: Mutex::new(Some(status_tx)),
            task: Mutex::new(Some(task)),
        })
    }

    /// This incarnation leaves the registry and a later one is
    /// registered under the same id.
    fn replace(&self, sys: &Arc<ActorSystem>, grace: Duration) -> Arc<Scripted> {
        self.leave();
        Scripted::register(sys, &self.id, grace)
    }

    fn leave(&self) {
        drop(self.guard.lock().unwrap().take());
    }

    fn publish_stopped(&self) {
        if let Some(tx) = self.status_tx.lock().unwrap().take() {
            StatusValue::write_reason(&tx, crate::types::StopReason::Graceful, true);
        }
    }

    /// Ends as the runtime does: leaves the registry, seals the lane,
    /// then publishes `Stopped`.
    fn end(&self) {
        self.leave();
        let _ = self.lane.seal();
        self.publish_stopped();
    }

    fn task_ended(&self) -> bool {
        self.task
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|task| task.is_finished())
    }

    fn task(&self) -> tokio::task::JoinHandle<()> {
        self.task
            .lock()
            .unwrap()
            .take()
            .expect("the task is taken once")
    }
}

fn set_ladder_hook(
    sys: &ActorSystem,
    hook: impl Fn(LadderPoint, &ActorId) + Send + Sync + 'static,
) {
    *sys.ladder_hook.lock().unwrap() = Some(Arc::new(hook));
}

fn scripted_system(what: &str) -> Arc<ActorSystem> {
    ActorSystem::create(format!("{what}-{}", uuid::Uuid::new_v4())).unwrap()
}

async fn shut_down(
    sys: &Arc<ActorSystem>,
    timeout: Duration,
    per_actor: Duration,
) -> ShutdownReport {
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        sys.shutdown_with(ShutdownPolicy {
            timeout,
            per_actor_timeout: per_actor,
        }),
    )
    .await
    .expect("the shutdown returns");
    // The hooks hold the system; clearing them ends the cycle.
    *sys.ladder_hook.lock().unwrap() = None;
    report
}

fn outcome_of(report: &ShutdownReport, id: &ActorId) -> StopOutcome {
    report
        .outcomes
        .iter()
        .find(|(reported, _)| reported == id)
        .map(|(_, report)| report.outcome)
        .unwrap_or_else(|| panic!("{id} is reported: {report:?}"))
}

/// A global timeout that cuts the ladder in its first step: every root
/// still waiting goes to the force sweep, unasked for `Kill`.
const CUT_IN_STEP_ONE: (Duration, Duration) = (Duration::from_millis(100), Duration::from_secs(30));

/// A step 1 that ends at once and a global timeout that then cuts the
/// ladder in its `Kill` step (the root's grace being longer).
const CUT_IN_STEP_TWO: (Duration, Duration) =
    (Duration::from_millis(1000), Duration::from_millis(1));

/// A grace no test here waits out.
const LONG_GRACE: Duration = Duration::from_secs(30);

/// Two roots cut at the first one's step 1. The first left the registry
/// before the sweep looked and had not published `Stopped`; it publishes
/// it only once the sweep has moved on to the second root. The sweep
/// waits for it and reports what it reached by itself.
async fn the_sweep_waits_for_a_root_gone_before_it_published_stopped() {
    let sys = scripted_system("sweep-window");
    let second = Scripted::register(&sys, &ActorId::from("window-second"), LONG_GRACE);
    // Registered last, so the ladder reaches it first and is cut there.
    // Its grace is short of the test's bounds: a sweep that waited for it
    // before looking at the next root would give it `Unresponsive`.
    let first = Scripted::register(
        &sys,
        &ActorId::from("window-first"),
        Duration::from_millis(600),
    );
    {
        let (first, second) = (first.clone(), second.clone());
        set_ladder_hook(&sys, move |point, id| {
            if point != LadderPoint::SweepLookup {
                return;
            }
            if *id == first.id {
                first.leave();
            } else if *id == second.id {
                first.publish_stopped();
                second.end();
            }
        });
    }
    let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
    assert_eq!(
        outcome_of(&report, &first.id),
        StopOutcome::Graceful,
        "a root that left the registry and then published Stopped itself"
    );
    assert_eq!(outcome_of(&report, &second.id), StopOutcome::Graceful);
    first.task().abort();
    second.task().abort();
}

/// The same window, held past the bound: a root that left the registry
/// and has not published `Stopped` within its kill grace and the wait
/// after an abort is `Unresponsive`, as on every other path.
async fn a_root_gone_and_not_stopped_within_its_bound_is_unresponsive() {
    let sys = scripted_system("sweep-window-held");
    // Longer than the default grace, so the bound read is the root's own.
    let grace = Duration::from_millis(600);
    let second = Scripted::register(&sys, &ActorId::from("held-second"), LONG_GRACE);
    let first = Scripted::register(&sys, &ActorId::from("held-first"), grace);
    {
        let (first, second) = (first.clone(), second.clone());
        set_ladder_hook(&sys, move |point, id| {
            if point != LadderPoint::SweepLookup {
                return;
            }
            if *id == first.id {
                first.leave();
            } else if *id == second.id {
                second.end();
            }
        });
    }
    let started = Instant::now();
    let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
    assert_eq!(outcome_of(&report, &first.id), StopOutcome::Unresponsive);
    assert!(
        started.elapsed() >= CUT_IN_STEP_ONE.0 + grace + ABORT_SETTLE,
        "the sweep waited the root's grace and the wait after an abort"
    );
    first.publish_stopped();
    first.task().abort();
    second.task().abort();
}

/// The ladder is cut in step 2 after its `Kill` step found the root
/// already gone from the registry: it raised nothing, so the root,
/// publishing `Stopped` itself, is `Graceful`.
async fn a_root_gone_at_the_kill_step_reports_no_raised_kill() {
    let sys = scripted_system("gone-at-kill");
    let root = Scripted::register(&sys, &ActorId::from("gone-at-kill"), LONG_GRACE);
    let reached_kill = Arc::new(AtomicBool::new(false));
    {
        let (root, reached_kill) = (root.clone(), reached_kill.clone());
        set_ladder_hook(&sys, move |point, _id| match point {
            LadderPoint::Kill => {
                reached_kill.store(true, Ordering::SeqCst);
                root.leave();
            }
            LadderPoint::SweepLookup => root.end(),
            _ => {}
        });
    }
    let report = shut_down(&sys, CUT_IN_STEP_TWO.0, CUT_IN_STEP_TWO.1).await;
    assert!(
        reached_kill.load(Ordering::SeqCst),
        "the ladder reached step 2"
    );
    assert!(
        matches!(root.lane.reason(), Some(StopReason::ParentRequest)),
        "no Kill was raised"
    );
    assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
    root.task().abort();
}

/// Two non-root leftovers leave the registry when the final sweep's force
/// looks them up, after the global deadline: one publishes `Stopped` with
/// its reason, the other's status sender closes without it. Each is
/// reported from its own plane, with the reason it holds: the first as a
/// kill, since its end became final after the deadline, the second as
/// aborted.
async fn a_leftover_gone_at_the_sweep_lookup_reports_the_reason_its_plane_holds() {
    let sys = scripted_system("leftover-gone");
    let stopped = Scripted::register_as(&sys, &ActorId::from("stopped"), LONG_GRACE, false);
    let closed = Scripted::register_as(&sys, &ActorId::from("closed"), LONG_GRACE, false);
    {
        let (stopped, closed) = (stopped.clone(), closed.clone());
        set_ladder_hook(&sys, move |point, id| {
            if point != LadderPoint::SweepLookup {
                return;
            }
            if *id == stopped.id {
                stopped.end();
            } else if *id == closed.id {
                closed.leave();
                drop(closed.status_tx.lock().unwrap().take());
            }
        });
    }
    let report = shut_down(&sys, Duration::from_millis(300), Duration::from_secs(5)).await;
    let swept = |id: &ActorId| {
        report
            .swept
            .iter()
            .find(|(reported, _)| reported == id)
            .map(|(_, report)| report.clone())
            .unwrap_or_else(|| panic!("{id} is swept: {report:?}"))
    };
    assert_eq!(
        swept(&stopped.id),
        StopReport {
            outcome: StopOutcome::Killed,
            reason: Some(StopReason::Graceful),
        }
    );
    assert_eq!(
        swept(&closed.id),
        StopReport {
            outcome: StopOutcome::Aborted,
            reason: Some(StopReason::Kill),
        }
    );
    stopped.task().abort();
    closed.task().abort();
}

/// What a shutdown did to a later incarnation `S` standing in for a root
/// under the same id: `S` is registered at `swap_at` in place of the
/// snapshotted root (which then publishes `Stopped` when
/// `root_publishes`). When the shutdown takes `S` on as a late root,
/// before its own first ladder step, its lane, whether its task had ended
/// and the system's arm count are recorded, and `S` ends there.
struct SuccessorWatch {
    successor: Mutex<Option<Arc<Scripted>>>,
    before_its_ladder: Mutex<Option<(Option<StopReason>, bool, u64)>>,
}

impl SuccessorWatch {
    fn successor(&self) -> Arc<Scripted> {
        self.successor
            .lock()
            .unwrap()
            .clone()
            .expect("the successor was registered")
    }

    fn before_its_ladder(&self) -> (Option<StopReason>, bool, u64) {
        self.before_its_ladder
            .lock()
            .unwrap()
            .clone()
            .expect("the shutdown took the successor on as a late root")
    }
}

fn watch_successor(
    sys: &Arc<ActorSystem>,
    root: &Arc<Scripted>,
    swap_at: LadderPoint,
    root_publishes: bool,
) -> Arc<SuccessorWatch> {
    let watch = Arc::new(SuccessorWatch {
        successor: Mutex::new(None),
        before_its_ladder: Mutex::new(None),
    });
    let (hook_sys, root, hook_watch) = (sys.clone(), root.clone(), watch.clone());
    set_ladder_hook(sys, move |point, _id| {
        let successor = hook_watch.successor.lock().unwrap().clone();
        match successor {
            None if point == swap_at => {
                let successor = root.replace(&hook_sys, HOOK_GRACE);
                *hook_watch.successor.lock().unwrap() = Some(successor);
                if root_publishes {
                    root.publish_stopped();
                }
            }
            Some(successor)
                if point == LadderPoint::Stop
                    && hook_watch.before_its_ladder.lock().unwrap().is_none() =>
            {
                *hook_watch.before_its_ladder.lock().unwrap() = Some((
                    successor.lane.reason(),
                    successor.task_ended(),
                    hook_sys.backstop_arms.load(Ordering::SeqCst),
                ));
                successor.end();
            }
            _ => {}
        }
    });
    watch
}

/// Both incarnations under the id are reported, the snapshotted one first
/// with `first`, the successor `Graceful` (it ended at its first step).
fn reported_twice(report: &ShutdownReport, id: &ActorId, first: StopOutcome) {
    let under_id: Vec<StopOutcome> = report
        .outcomes
        .iter()
        .filter(|(reported, _)| reported == id)
        .map(|(_, report)| report.outcome)
        .collect();
    assert_eq!(under_id, vec![first, StopOutcome::Graceful], "{report:?}");
}

/// Before the ladder's `Kill` step, the incarnation it is stopping is
/// replaced by a later one under the same id, and never publishes
/// `Stopped`. The `Kill` step neither raises on nor arms the successor,
/// the replaced incarnation is `Unresponsive`, and the shutdown then
/// stops the successor as a late root, reported after it.
async fn a_successor_registered_before_the_kill_step_is_never_raised_on() {
    let sys = scripted_system("successor-at-kill");
    let grace = Duration::from_millis(20);
    let root = Scripted::register(&sys, &ActorId::from("successor-at-kill"), grace);
    let watch = watch_successor(&sys, &root, LadderPoint::Kill, false);
    let report = shut_down(&sys, Duration::from_secs(30), Duration::from_millis(1)).await;
    let (lane, ended, arms) = watch.before_its_ladder();
    assert!(lane.is_none(), "nothing was raised on the successor");
    assert!(!ended);
    assert_eq!(arms, 0, "nothing was armed");
    reported_twice(&report, &root.id, StopOutcome::Unresponsive);
    watch.successor().task().abort();
    root.publish_stopped();
    root.task().abort();
}

/// Before the ladder begins, the incarnation it snapshotted is replaced
/// by a later one under the same id: no step of that ladder stops, kills
/// or aborts the successor, the snapshotted incarnation, which never
/// publishes `Stopped`, is `Unresponsive`, and the successor is stopped
/// as a late root.
async fn the_ladder_never_acts_on_a_successor_registered_before_it_began() {
    let sys = scripted_system("successor-at-stop");
    let grace = Duration::from_millis(20);
    let root = Scripted::register(&sys, &ActorId::from("successor-at-stop"), grace);
    let watch = watch_successor(&sys, &root, LadderPoint::Stop, false);
    let report = shut_down(&sys, Duration::from_secs(30), grace).await;
    let (lane, ended, arms) = watch.before_its_ladder();
    assert!(lane.is_none(), "nothing was raised on the successor");
    assert!(!ended, "the successor was not aborted");
    assert_eq!(arms, 0, "nothing was armed");
    reported_twice(&report, &root.id, StopOutcome::Unresponsive);
    watch.successor().task().abort();
    root.publish_stopped();
    root.task().abort();
}

/// Before the ladder's abort step, the incarnation it raised `Kill` on is
/// replaced by a later one under the same id: the abort step does not
/// abort the successor, and the successor is stopped as a late root.
async fn the_abort_step_never_aborts_a_successor_registered_before_it() {
    let sys = scripted_system("successor-at-abort");
    let grace = Duration::from_millis(20);
    let root = Scripted::register(&sys, &ActorId::from("successor-at-abort"), grace);
    let watch = watch_successor(&sys, &root, LadderPoint::Abort, false);
    let report = shut_down(&sys, Duration::from_secs(30), grace).await;
    assert!(matches!(root.lane.reason(), Some(StopReason::Kill)));
    let (lane, ended, _) = watch.before_its_ladder();
    assert!(lane.is_none(), "nothing was raised on the successor");
    assert!(!ended, "the successor was not aborted");
    reported_twice(&report, &root.id, StopOutcome::Unresponsive);
    watch.successor().task().abort();
    root.publish_stopped();
}

/// The ladder is cut in step 1; before the sweep looks, the root is
/// replaced by a later incarnation under the same id and never publishes
/// `Stopped`. The sweep reports the snapshotted root `Unresponsive` after
/// its grace and never kills the successor, and the shutdown then stops
/// the successor as a late root, reported after it.
async fn the_sweep_never_kills_a_successor_registered_before_it_looked() {
    let sys = scripted_system("successor-at-sweep");
    let grace = Duration::from_millis(20);
    let root = Scripted::register(&sys, &ActorId::from("successor-at-sweep"), grace);
    let watch = watch_successor(&sys, &root, LadderPoint::SweepLookup, false);
    let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
    let (lane, ended, arms) = watch.before_its_ladder();
    assert!(lane.is_none(), "nothing was raised on the successor");
    assert!(!ended);
    assert_eq!(arms, 0, "nothing was armed");
    reported_twice(&report, &root.id, StopOutcome::Unresponsive);
    watch.successor().task().abort();
    root.publish_stopped();
    root.task().abort();
}

/// The ladder's `Kill` step finds the root, but between its lookup and
/// its raise the root leaves, seals its lane and publishes `Stopped`: the
/// lane refuses the `Kill`, nothing is armed, and the root, which ended
/// with its own reason, is `Graceful`.
async fn a_root_that_refuses_the_kill_step_keeps_its_own_outcome() {
    let sys = scripted_system("kill-step-refused");
    let root = Scripted::register(&sys, &ActorId::from("kill-step-refused"), LONG_GRACE);
    {
        let root = root.clone();
        *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
            root.leave();
            let _ = root.lane.seal();
            root.publish_stopped();
        }));
    }
    let report = shut_down(&sys, Duration::from_secs(30), Duration::from_millis(1)).await;
    assert!(
        sys.after_kill_lookup.lock().unwrap().is_none(),
        "the Kill step found the root"
    );
    assert!(matches!(
        root.lane.reason(),
        Some(StopReason::ParentRequest)
    ));
    assert_eq!(
        sys.backstop_arms.load(Ordering::SeqCst),
        0,
        "nothing was armed"
    );
    assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
    root.task().abort();
}

/// The ladder is cut in step 1; before the sweep looks, the root
/// publishes `Stopped` and leaves: `Graceful`, no `Kill` having been
/// raised on it.
async fn a_root_cut_in_step_one_that_ends_before_the_sweep_is_graceful() {
    let sys = scripted_system("cut-step-one");
    let root = Scripted::register(&sys, &ActorId::from("cut-step-one"), LONG_GRACE);
    {
        let root = root.clone();
        set_ladder_hook(&sys, move |point, _id| {
            if point == LadderPoint::SweepLookup {
                root.end();
            }
        });
    }
    let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
    assert!(matches!(
        root.lane.reason(),
        Some(StopReason::ParentRequest)
    ));
    assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
    root.task().abort();
}

/// The ladder is cut in step 2, after it raised `Kill`; before the sweep
/// looks, the root publishes `Stopped` and leaves: `Killed`.
async fn a_root_cut_in_step_two_that_ends_before_the_sweep_is_killed() {
    let sys = scripted_system("cut-step-two");
    let root = Scripted::register(&sys, &ActorId::from("cut-step-two"), LONG_GRACE);
    {
        let root = root.clone();
        set_ladder_hook(&sys, move |point, _id| {
            if point == LadderPoint::SweepLookup {
                root.end();
            }
        });
    }
    let report = shut_down(&sys, CUT_IN_STEP_TWO.0, CUT_IN_STEP_TWO.1).await;
    assert!(
        matches!(root.lane.reason(), Some(StopReason::Kill)),
        "the ladder raised Kill before it was cut"
    );
    assert_eq!(outcome_of(&report, &root.id), StopOutcome::Killed);
    root.task().abort();
}

/// The ladder is cut in step 1 and the sweep finds the root, but between
/// its lookup and its raise the root leaves, seals its lane and publishes
/// `Stopped`: the lane refuses the sweep's `Kill`, and the root, which
/// ended with its own reason, is `Graceful`.
async fn a_sweep_target_that_refuses_the_kill_keeps_its_own_outcome() {
    let sys = scripted_system("sweep-refused");
    let root = Scripted::register(&sys, &ActorId::from("sweep-refused"), LONG_GRACE);
    {
        let root = root.clone();
        *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
            root.leave();
            let _ = root.lane.seal();
            root.publish_stopped();
        }));
    }
    let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
    assert!(
        sys.after_kill_lookup.lock().unwrap().is_none(),
        "the sweep found the root"
    );
    assert!(matches!(
        root.lane.reason(),
        Some(StopReason::ParentRequest)
    ));
    assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
    root.task().abort();
}

/// `stop_within(Graceful)` outlives its window; before the system's stop
/// task raises its `Kill`, the actor leaves the registry, seals its lane
/// and publishes `Stopped`. The lane refuses the `Kill`, nothing is armed,
/// and the call reports `Graceful`, the outcome the actor reached by
/// itself.
async fn stop_within_reports_its_kill_only_when_the_lane_took_it() {
    let sys = scripted_system("stop-within-refused");
    let root = Scripted::register(&sys, &ActorId::from("stop-within-refused"), LONG_GRACE);
    let reached = Arc::new(AtomicBool::new(false));
    {
        let (root, reached) = (root.clone(), reached.clone());
        set_ladder_hook(&sys, move |point, _id| {
            if point == LadderPoint::StopWithinKill {
                reached.store(true, Ordering::SeqCst);
                root.leave();
                let _ = root.lane.seal();
                root.publish_stopped();
            }
        });
    }
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        root.handle
            .stop_within(StopReason::Graceful, Duration::from_millis(20)),
    )
    .await
    .expect("stop_within returns")
    .expect("the graceful request is accepted");
    *sys.ladder_hook.lock().unwrap() = None;
    assert!(
        reached.load(Ordering::SeqCst),
        "the call reached its Kill step"
    );
    assert!(matches!(root.lane.reason(), Some(StopReason::Graceful)));
    assert_eq!(
        sys.backstop_arms.load(Ordering::SeqCst),
        0,
        "nothing was armed"
    );
    assert_eq!(outcome, StopOutcome::Graceful);
    let rt = tokio::runtime::Handle::current();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !sys
            .helper_ends(&rt)
            .contains(&(HelperKind::StopLadder, crate::helper::HelperEnd::Finished))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Kill step ran in a stop task of the system, which ended");
    root.task().abort();
}

// - Every wait bounded by a kill grace is the incarnation's own ----------

/// Two placements of one condition, a root with its own 600 ms grace that
/// leaves the registry and never publishes `Stopped` while the shutdown
/// runs: gone before the sweep's lookup, or gone between the lookup and
/// the raise. Both are waited the root's grace and the wait after an
/// abort, and both are `Unresponsive`.
async fn a_root_leaving_around_the_sweep_s_lookup_is_waited_its_own_grace() {
    let grace = Duration::from_millis(600);
    for after_the_lookup in [false, true] {
        let sys = scripted_system("own-grace-sweep");
        let root = Scripted::register(&sys, &ActorId::from("own-grace-sweep"), grace);
        if after_the_lookup {
            let root = root.clone();
            *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || root.leave()));
        } else {
            let root = root.clone();
            set_ladder_hook(&sys, move |point, _id| {
                if point == LadderPoint::SweepLookup {
                    root.leave();
                }
            });
        }
        let started = Instant::now();
        let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
        let took = started.elapsed();
        assert_eq!(
            outcome_of(&report, &root.id),
            StopOutcome::Unresponsive,
            "after_the_lookup={after_the_lookup}"
        );
        assert!(
            took >= CUT_IN_STEP_ONE.0 + grace + ABORT_SETTLE,
            "after_the_lookup={after_the_lookup}: the root's own grace, took {took:?}"
        );
        root.publish_stopped();
        root.task().abort();
    }
}

/// `kill_by_id` on a root that leaves the registry between the lookup and
/// the raise, and never publishes `Stopped` during the call: the call
/// waits the root's own grace, not a default.
async fn kill_by_id_waits_the_incarnation_s_own_grace_after_its_entry_is_gone() {
    let sys = scripted_system("own-grace-kill-by-id");
    let grace = Duration::from_millis(600);
    let root = Scripted::register(&sys, &ActorId::from("own-grace-kill-by-id"), grace);
    {
        let root = root.clone();
        *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || root.leave()));
    }
    let started = Instant::now();
    assert!(sys.kill_by_id(&root.id).await.is_some());
    let took = started.elapsed();
    assert!(
        took >= grace + ABORT_SETTLE,
        "the root's own grace, took {took:?}"
    );
    root.publish_stopped();
    root.task().abort();
}

/// `stop_within(Graceful)` outlives its window; before its `Kill` step the
/// actor leaves the registry and never publishes `Stopped` during the
/// call. The `Kill` step waits the actor's own grace, read while the
/// entry held it.
async fn stop_within_waits_the_incarnation_s_own_grace_after_its_entry_is_gone() {
    let sys = scripted_system("own-grace-stop-within");
    let grace = Duration::from_millis(600);
    let root = Scripted::register(&sys, &ActorId::from("own-grace-stop-within"), grace);
    {
        let root = root.clone();
        set_ladder_hook(&sys, move |point, _id| {
            if point == LadderPoint::StopWithinKill {
                root.leave();
            }
        });
    }
    let within = Duration::from_millis(20);
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        root.handle.stop_within(StopReason::Graceful, within),
    )
    .await
    .expect("stop_within returns")
    .expect("the graceful request is accepted");
    let took = started.elapsed();
    *sys.ladder_hook.lock().unwrap() = None;
    assert_eq!(outcome, StopOutcome::Unresponsive);
    assert!(
        took >= within + grace + ABORT_SETTLE,
        "the actor's own grace, took {took:?}"
    );
    root.publish_stopped();
    root.task().abort();
}

// - A refused Kill arms nothing ------------------------------------------

/// A registered root whose lane is sealed between a lookup and the raise
/// (`kill_by_id`, or the sweep's found branch): the lane refuses the
/// `Kill` and nothing is armed, though the entry still holds its abort;
/// `kill_by_id` answers `false` and aborts nothing.
async fn a_refused_kill_arms_nothing_while_the_entry_is_registered() {
    for through_the_sweep in [false, true] {
        let sys = scripted_system("refused-registered");
        let root = Scripted::register(&sys, &ActorId::from("refused-registered"), LONG_GRACE);
        {
            let root = root.clone();
            *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
                let _ = root.lane.seal();
                root.publish_stopped();
            }));
        }
        if through_the_sweep {
            let report = shut_down(&sys, CUT_IN_STEP_ONE.0, CUT_IN_STEP_ONE.1).await;
            assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
        } else {
            // Answered as `kill(name)` answers a sealed lane, and nothing
            // is aborted.
            assert!(sys.kill_by_id(&root.id).await.is_none());
            let mut task = root.task();
            assert!(
                tokio::time::timeout(ABORT_SETTLE, &mut task).await.is_err(),
                "the refused Kill aborted the task"
            );
            task.abort();
        }
        assert!(
            sys.after_kill_lookup.lock().unwrap().is_none(),
            "through_the_sweep={through_the_sweep}: the lookup found the root"
        );
        assert_eq!(
            sys.backstop_arms.load(Ordering::SeqCst),
            0,
            "through_the_sweep={through_the_sweep}: nothing was armed"
        );
        root.leave();
        if through_the_sweep {
            root.task().abort();
        }
    }
}

// - Late roots ------------------------------------------------------------

/// Two roots registered while the first root's ladder runs, which then
/// ends; the global timeout cuts the first late root's ladder, so both go
/// to one sweep. The first left the registry before the sweep looked and
/// publishes `Stopped` only once the sweep looks at the second: it is
/// waited for, concurrently, and reported `Graceful`.
async fn late_roots_are_swept_together() {
    let sys = scripted_system("late-together");
    let grace = Duration::from_millis(600);
    let root = Scripted::register(&sys, &ActorId::from("late-root"), LONG_GRACE);
    let late: Arc<Mutex<Vec<Arc<Scripted>>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let (hook_sys, root, late) = (sys.clone(), root.clone(), late.clone());
        set_ladder_hook(&sys, move |point, id| {
            let registered = late.lock().unwrap().clone();
            match (point, registered.as_slice()) {
                (LadderPoint::Stop, []) if *id == root.id => {
                    let second =
                        Scripted::register(&hook_sys, &ActorId::from("late-second"), grace);
                    // Registered last: laddered first, and cut there.
                    let first = Scripted::register(&hook_sys, &ActorId::from("late-first"), grace);
                    *late.lock().unwrap() = vec![first, second];
                    root.end();
                }
                (LadderPoint::SweepLookup, [first, second]) => {
                    if *id == first.id {
                        first.leave();
                    } else if *id == second.id {
                        first.publish_stopped();
                        second.end();
                    }
                }
                _ => {}
            }
        });
    }
    // Step 1 of every late root waits until the deadline.
    let report = shut_down(&sys, Duration::from_millis(300), Duration::from_secs(30)).await;
    let late = late.lock().unwrap().clone();
    assert_eq!(late.len(), 2, "the late roots were registered");
    assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
    assert_eq!(outcome_of(&report, &late[0].id), StopOutcome::Graceful);
    assert_eq!(outcome_of(&report, &late[1].id), StopOutcome::Graceful);
    for scripted in [&root, &late[0], &late[1]] {
        scripted.task().abort();
    }
}

/// A root registered while the first root's ladder runs, which then ends;
/// the late root's ladder raises `Kill` and is cut in step 2, and before
/// the sweep looks it ends: `Killed`.
async fn a_late_root_cut_in_step_two_that_ends_before_the_sweep_is_killed() {
    let sys = scripted_system("late-killed");
    let root = Scripted::register(&sys, &ActorId::from("late-killed-root"), LONG_GRACE);
    let late: Arc<Mutex<Option<Arc<Scripted>>>> = Arc::new(Mutex::new(None));
    {
        let (hook_sys, root, late) = (sys.clone(), root.clone(), late.clone());
        set_ladder_hook(&sys, move |point, id| {
            let registered = late.lock().unwrap().clone();
            match (point, registered) {
                (LadderPoint::Stop, None) if *id == root.id => {
                    let late_root =
                        Scripted::register(&hook_sys, &ActorId::from("late-killed"), LONG_GRACE);
                    *late.lock().unwrap() = Some(late_root);
                    root.end();
                }
                (LadderPoint::SweepLookup, Some(late_root)) if *id == late_root.id => {
                    late_root.end();
                }
                _ => {}
            }
        });
    }
    let report = shut_down(&sys, CUT_IN_STEP_TWO.0, CUT_IN_STEP_TWO.1).await;
    let late_root = late
        .lock()
        .unwrap()
        .clone()
        .expect("the late root was registered");
    assert!(
        matches!(late_root.lane.reason(), Some(StopReason::Kill)),
        "the late root's ladder raised Kill before it was cut"
    );
    assert_eq!(outcome_of(&report, &root.id), StopOutcome::Graceful);
    assert_eq!(outcome_of(&report, &late_root.id), StopOutcome::Killed);
    root.task().abort();
    late_root.task().abort();
}

// - The verdict follows the Kill the call's lane took --------------------

/// The same end, an actor that left the registry and then published
/// `Stopped` by itself, read by two calls. `stop_within` raises its `Kill`
/// on the actor's own lane, which takes it: `Killed`. The shutdown's
/// ladder finds the actor out of the registry at its `Kill` step and
/// raises nothing: `Graceful`.
async fn a_kill_the_lane_took_is_killed_and_one_never_raised_is_graceful() {
    // stop_within: the actor leaves before the Kill step and publishes
    // `Stopped` once its lane holds the `Kill`.
    let sys = scripted_system("verdict-split-handle");
    let root = Scripted::register(&sys, &ActorId::from("verdict-split-handle"), LONG_GRACE);
    {
        let root = root.clone();
        set_ladder_hook(&sys, move |point, _id| {
            if point == LadderPoint::StopWithinKill {
                root.leave();
            }
        });
    }
    let ends = {
        let root = root.clone();
        tokio::spawn(async move {
            let bound = Instant::now() + Duration::from_secs(10);
            while !matches!(root.lane.reason(), Some(StopReason::Kill)) && Instant::now() < bound {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            root.publish_stopped();
        })
    };
    let through_the_handle = tokio::time::timeout(
        Duration::from_secs(10),
        root.handle
            .stop_within(StopReason::Graceful, Duration::from_millis(20)),
    )
    .await
    .expect("stop_within returns")
    .expect("the graceful request is accepted");
    *sys.ladder_hook.lock().unwrap() = None;
    ends.await.unwrap();
    root.task().abort();

    // The shutdown's ladder: the actor leaves and publishes `Stopped`
    // before the Kill step, which finds no entry.
    let sys = scripted_system("verdict-split-ladder");
    let root = Scripted::register(&sys, &ActorId::from("verdict-split-ladder"), LONG_GRACE);
    {
        let root = root.clone();
        set_ladder_hook(&sys, move |point, _id| {
            if point == LadderPoint::Kill {
                root.leave();
                root.publish_stopped();
            }
        });
    }
    let report = shut_down(&sys, Duration::from_secs(30), Duration::from_millis(1)).await;
    root.task().abort();

    assert_eq!(
        (through_the_handle, outcome_of(&report, &root.id)),
        (StopOutcome::Killed, StopOutcome::Graceful)
    );
}

// - What the shutdown has taken on ----------------------------------------

/// A root and a late root that both stay registered after their ladders
/// (neither publishes `Stopped`): each is reported once, the late pass
/// ends, and the leftover sweep takes on neither again.
async fn a_root_left_registered_after_its_ladder_is_reported_once() {
    let sys = scripted_system("left-registered");
    let grace = Duration::from_millis(20);
    let root = Scripted::register(&sys, &ActorId::from("left-registered-root"), grace);
    let late: Arc<Mutex<Option<Arc<Scripted>>>> = Arc::new(Mutex::new(None));
    {
        let (hook_sys, root, late) = (sys.clone(), root.clone(), late.clone());
        set_ladder_hook(&sys, move |point, id| {
            if point == LadderPoint::Stop && *id == root.id && late.lock().unwrap().is_none() {
                let late_root =
                    Scripted::register(&hook_sys, &ActorId::from("left-registered-late"), grace);
                *late.lock().unwrap() = Some(late_root);
            }
        });
    }
    let report = shut_down(&sys, Duration::from_secs(30), grace).await;
    let late_root = late
        .lock()
        .unwrap()
        .clone()
        .expect("the late root was registered");
    assert_eq!(
        outcomes_of(&report.outcomes),
        vec![
            (root.id.clone(), StopOutcome::Unresponsive),
            (late_root.id.clone(), StopOutcome::Unresponsive),
        ]
    );
    assert!(report.swept.is_empty(), "{report:?}");
    for scripted in [&root, &late_root] {
        scripted.end();
        scripted.task().abort();
    }
}

/// The ladder raised `Kill` on a root and was cut in step 2; before the
/// sweep looks, the root seals its lane and publishes `Stopped` while
/// still registered. The sweep finds it and its own `Kill` is refused,
/// but this shutdown had raised `Kill` on it: `Killed`.
async fn a_root_the_ladder_raised_kill_on_is_killed_though_the_sweep_s_kill_is_refused() {
    let sys = scripted_system("carried-kill");
    let root = Scripted::register(&sys, &ActorId::from("carried-kill"), LONG_GRACE);
    {
        let root = root.clone();
        set_ladder_hook(&sys, move |point, _id| {
            if point == LadderPoint::SweepLookup {
                let _ = root.lane.seal();
                root.publish_stopped();
            }
        });
    }
    let report = shut_down(&sys, CUT_IN_STEP_TWO.0, CUT_IN_STEP_TWO.1).await;
    assert!(matches!(root.lane.reason(), Some(StopReason::Kill)));
    assert_eq!(outcome_of(&report, &root.id), StopOutcome::Killed);
    root.leave();
    root.task().abort();
}

// - Registration against a shutdown -------------------------------------

/// Registers a root under `id` from a thread of its own. Once registered,
/// the root's task, spawned on `rt`, ends on the first stop request its
/// lane receives: it frees its registry entry, then publishes `Stopped`.
fn register_a_root_that_ends_when_asked(
    sys: &Arc<ActorSystem>,
    id: &ActorId,
    rt: &Handle,
) -> Result<(), SpawnError> {
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, mut stop_rx) = StopLane::new();
    let handle = ActorHandle::<Dummy>::new(
        id.clone(),
        tx,
        stop_lane,
        1,
        status_rx,
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    let seq = sys.register_actor::<Dummy>(
        id,
        false,
        &handle,
        true,
        TurnClock::default(),
        LONG_GRACE,
        rt.clone(),
    )?;
    let guard = RegistryGuard::new(sys.clone(), id.clone(), seq);
    let task = rt.spawn(async move {
        let _ = stop_rx.changed().await;
        drop(guard);
        StatusValue::write_reason(&status_tx, crate::types::StopReason::Graceful, true);
    });
    sys.attach_abort(id, task.abort_handle(), seq);
    Ok(())
}

/// A registration has passed its phase check when the shutdown begins,
/// and holds there until the shutdown has reached the registration gate
/// and then until it has taken its last root snapshot (or a second has
/// passed). The shutdown's phase change waits for the registration to
/// complete, so the root is in its first snapshot: it is stopped and
/// reported, and nothing is left registered or swept.
async fn a_registration_in_progress_when_the_shutdown_begins_is_stopped_by_it() {
    let sys = scripted_system("registration-race");
    let id = ActorId::from("racing-root");
    let in_window = Arc::new(AtomicBool::new(false));
    let saw_the_shutdown = Arc::new(AtomicBool::new(false));
    {
        let (hook_sys, in_window, saw) = (sys.clone(), in_window.clone(), saw_the_shutdown.clone());
        *sys.after_phase_check.lock().unwrap() = Some(Arc::new(move || {
            in_window.store(true, Ordering::SeqCst);
            let bound = std::time::Instant::now() + Duration::from_secs(10);
            while !hook_sys.shutdown_at_the_gate.load(Ordering::SeqCst)
                && std::time::Instant::now() < bound
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            saw.store(
                hook_sys.shutdown_at_the_gate.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
            let bound = std::time::Instant::now() + Duration::from_secs(1);
            while !hook_sys.root_snapshots_done.load(Ordering::SeqCst)
                && std::time::Instant::now() < bound
            {
                std::thread::sleep(Duration::from_millis(1));
            }
        }));
    }
    let registering = {
        let (sys, id, rt) = (sys.clone(), id.clone(), Handle::current());
        std::thread::spawn(move || register_a_root_that_ends_when_asked(&sys, &id, &rt))
    };
    until("the registration to pass its phase check", || {
        in_window.load(Ordering::SeqCst)
    })
    .await;
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_secs(5),
            per_actor_timeout: Duration::from_secs(2),
        }),
    )
    .await
    .expect("the shutdown returns");
    let registered = tokio::task::spawn_blocking(move || registering.join())
        .await
        .unwrap()
        .expect("the registering thread ends");
    assert!(
        registered.is_ok(),
        "the registration passed its phase check"
    );
    assert!(
        saw_the_shutdown.load(Ordering::SeqCst),
        "the registration was in progress when the shutdown reached the gate"
    );
    assert_eq!(
        outcomes_of(&report.outcomes),
        vec![(id.clone(), StopOutcome::Graceful)]
    );
    assert!(report.swept.is_empty(), "{report:?}");
    assert!(sys.actor_ids().is_empty(), "nothing is left registered");
}

/// Two shutdowns overlap. The second is waiting on a non-root leftover
/// that has already left the registry when the first, finding nothing,
/// completes. While the second is in flight, `reactivate` is refused and
/// a registration is refused; once the second completes, the system is
/// `Defunct` with nothing registered. The leftover ends before the
/// deadline with nothing raised on it, so it reads `Graceful`.
async fn reactivate_waits_for_every_shutdown_in_flight() {
    let sys = scripted_system("overlapping-shutdowns");
    let mut entries = counting_reaper(&sys);
    let leftover =
        Scripted::register_as(&sys, &ActorId::from("overlap-leftover"), LONG_GRACE, false);
    let policy = ShutdownPolicy {
        timeout: Duration::from_secs(30),
        per_actor_timeout: Duration::from_secs(30),
    };
    let second = {
        let (sys, policy) = (sys.clone(), policy.clone());
        tokio::spawn(async move { sys.shutdown_with(policy).await })
    };
    // The second shutdown's sweep took the leftover on (it scheduled the
    // deadline entry for it) and waits for it; the leftover then leaves
    // the registry.
    until(
        "the second shutdown's deadline entry for the leftover",
        || entries.try_recv().is_ok(),
    )
    .await;
    assert!(
        leftover.lane.reason().is_none(),
        "nothing is raised before the deadline"
    );
    leftover.leave();
    let first = tokio::time::timeout(Duration::from_secs(10), sys.shutdown_with(policy))
        .await
        .expect("the first shutdown returns");
    assert!(
        first.outcomes.is_empty() && first.swept.is_empty(),
        "{first:?}"
    );
    assert!(!second.is_finished(), "the second shutdown is in flight");
    assert!(
        !sys.reactivate(),
        "reactivate is refused while a shutdown is in flight"
    );
    let root_id = ActorId::from("overlap-root");
    assert!(
        matches!(
            register_a_root_that_ends_when_asked(&sys, &root_id, &Handle::current()),
            Err(SpawnError::SystemShuttingDown(_))
        ),
        "a registration is refused while a shutdown is in flight"
    );
    let _ = leftover.lane.seal();
    leftover.publish_stopped();
    let second = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .expect("the second shutdown returns")
        .unwrap();
    assert_eq!(
        outcomes_of(&second.swept),
        vec![(leftover.id.clone(), StopOutcome::Graceful)]
    );
    assert!(sys.actor_ids().is_empty(), "nothing is left registered");
    assert!(
        sys.reactivate(),
        "the last shutdown to complete leaves it Defunct"
    );
    leftover.task().abort();
}

/// Three shutdowns: B waits on a non-root leftover; A starts with an
/// earlier deadline, lowers the system's to it, finds nothing and
/// completes while B waits; C starts after A with a later deadline. The
/// system holds A's deadline until the last shutdown in flight (B)
/// completes: A's completion leaves it, C folds into it, and only B's
/// completion clears it.
async fn the_sweep_deadline_is_held_until_the_last_shutdown_completes() {
    let sys = scripted_system("three-shutdowns");
    let mut entries = counting_reaper(&sys);
    let leftover = Scripted::register_as(&sys, &ActorId::from("held-leftover"), LONG_GRACE, false);
    let policy = |secs| ShutdownPolicy {
        timeout: Duration::from_secs(secs),
        per_actor_timeout: Duration::from_secs(secs),
    };
    let b = {
        let sys = sys.clone();
        tokio::spawn(async move { sys.shutdown_with(policy(60)).await })
    };
    until("B's deadline entry for the leftover", || {
        entries.try_recv().is_ok()
    })
    .await;
    let b_deadline = sys
        .sweep_deadline
        .borrow()
        .expect("B's sweep holds its deadline");
    leftover.leave();
    let a = tokio::time::timeout(Duration::from_secs(10), sys.shutdown_with(policy(30)))
        .await
        .expect("A returns");
    assert!(a.outcomes.is_empty() && a.swept.is_empty(), "{a:?}");
    let held = sys
        .sweep_deadline
        .borrow()
        .expect("A's completion leaves the deadline while B is in flight");
    assert!(
        held < b_deadline,
        "A lowered the system's deadline to its own"
    );
    let c = tokio::time::timeout(Duration::from_secs(10), sys.shutdown_with(policy(90)))
        .await
        .expect("C returns");
    assert!(c.outcomes.is_empty() && c.swept.is_empty(), "{c:?}");
    assert_eq!(
        *sys.sweep_deadline.borrow(),
        Some(held),
        "C folded into the held deadline and its completion left it"
    );
    assert!(!b.is_finished(), "B is in flight");
    let _ = leftover.lane.seal();
    leftover.publish_stopped();
    let b = tokio::time::timeout(Duration::from_secs(10), b)
        .await
        .expect("B returns")
        .unwrap();
    assert_eq!(
        outcomes_of(&b.swept),
        vec![(leftover.id.clone(), StopOutcome::Graceful)]
    );
    assert_eq!(
        *sys.sweep_deadline.borrow(),
        None,
        "the last shutdown to complete clears it"
    );
    assert!(sys.reactivate());
    leftover.task().abort();
}

/// A leftover whose end became final before the deadline (its lane
/// sealed, its reason written) publishes `Stopped` only between the
/// force's lookup and its raise. The force's `Kill` is refused, so this
/// shutdown raised nothing on it, and it reads `Graceful` with its own
/// reason.
async fn a_leftover_final_before_the_deadline_and_stopped_at_the_force_reads_graceful() {
    let sys = scripted_system("final-before-force");
    let leftover = Scripted::register_as(&sys, &ActorId::from("final-early"), LONG_GRACE, false);
    let _ = leftover.lane.seal();
    let tx = leftover
        .status_tx
        .lock()
        .unwrap()
        .take()
        .expect("the plane's sender");
    assert!(StatusValue::write_reason(&tx, StopReason::Graceful, false));
    *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
        tx.send_modify(|value| value.status = ActorStatus::Stopped);
    }));
    let report = shut_down(&sys, Duration::from_millis(200), Duration::from_secs(5)).await;
    assert_eq!(
        report.swept,
        vec![(
            leftover.id.clone(),
            StopReport {
                outcome: StopOutcome::Graceful,
                reason: Some(StopReason::Graceful),
            }
        )]
    );
    leftover.task().abort();
}

/// A sweep that lowers the system's deadline at the very instant an end
/// became final (the normal case on a paused clock) lands just after
/// that instant, so the end reads `Graceful` before the lowering and
/// after it. An own deadline already past is raised to the same floor.
#[tokio::test(start_paused = true)]
async fn a_lowering_at_the_instant_of_an_end_lands_after_it() {
    let sys = scripted_system("lowering-tie");
    let first = sys.fold_sweep_deadline(Instant::now() + Duration::from_secs(10));
    let (tx, rx) = StatusTx::channel(StatusValue::new(ActorStatus::Stopping));
    assert!(StatusValue::write_reason(&tx, StopReason::Graceful, true));
    let before = leftover_outcome(&rx, false, sys.current_sweep_deadline(first));
    let lowered = sys.fold_sweep_deadline(Instant::now());
    let final_at = rx.borrow().final_at.expect("the write stamped it");
    assert!(lowered > final_at, "the lowering lands after the end");
    let after = leftover_outcome(&rx, false, sys.current_sweep_deadline(lowered));
    assert_eq!(
        (before, after),
        (StopOutcome::Graceful, StopOutcome::Graceful)
    );
    let past = sys.fold_sweep_deadline(Instant::now() - Duration::from_secs(1));
    assert_eq!(
        past, lowered,
        "an own deadline already past takes the floor"
    );
}

/// Two shutdown calls share one sweep deadline and both report one
/// leftover whose end becomes final at offsets from 60 us before that
/// deadline to 100 us after it: every round, both calls report it the
/// same way, that way is the one its recorded final instant gives
/// (`Graceful` before the deadline, `Killed` at or after it), and the
/// rounds reach both sides of the deadline. The ending thread starts
/// well ahead of its instant and spins to it on the monotonic clock, so
/// a coarse or late timer only shortens the spin, and each round aims
/// by the shortest time an end has taken so far. Multi-thread only: the
/// two calls and the end must run in parallel for an end to fall
/// between one call's look at the leftover and the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_shutdowns_report_one_end_the_same_way_around_the_deadline() {
    // Ahead of the end's instant by more than a coarse host's timer
    // granularity (about 15.6 ms on Windows).
    const LEAD: Duration = Duration::from_millis(30);
    let mut sides: Vec<StopOutcome> = Vec::new();
    let mut end_cost: Option<Duration> = None;
    let mut round = 0u64;
    while round < 41 || (sides.len() < 2 && round < 400) {
        let offset = Duration::from_micros((round % 41) * 4);
        let sys = scripted_system("one-end-two-calls");
        let mut entries = counting_reaper(&sys);
        let leftover =
            Scripted::register_as(&sys, &ActorId::from("shared-leftover"), LONG_GRACE, false);
        let policy = ShutdownPolicy {
            timeout: Duration::from_millis(100),
            per_actor_timeout: Duration::from_millis(100),
        };
        let calls: Vec<_> = (0..2)
            .map(|_| {
                let (sys, policy) = (sys.clone(), policy.clone());
                tokio::spawn(async move { sys.shutdown_with(policy).await })
            })
            .collect();
        let mut queued = 0;
        until("both calls' deadline entries", || {
            queued += drained(&mut entries);
            queued >= 2
        })
        .await;
        let deadline = sys
            .sweep_deadline
            .borrow()
            .expect("the sweeps hold one deadline");
        let at = deadline - Duration::from_micros(60) + offset - end_cost.unwrap_or_default();
        tokio::time::sleep_until(at - LEAD).await;
        let ending = leftover.clone();
        tokio::task::spawn_blocking(move || {
            while Instant::now() < at {
                std::hint::spin_loop();
            }
            ending.end();
        })
        .await
        .unwrap();
        let final_at = leftover
            .handle
            .status_rx()
            .borrow()
            .final_at
            .expect("the end stamped its final instant");
        let cost = final_at.saturating_duration_since(at);
        end_cost = Some(end_cost.map_or(cost, |least| least.min(cost)));
        let mut seen = Vec::new();
        for call in calls {
            let report = tokio::time::timeout(Duration::from_secs(10), call)
                .await
                .expect("the call returns")
                .unwrap();
            seen.push(outcomes_of(&report.swept));
        }
        assert_eq!(seen[0], seen[1], "round {round}: one end, one outcome");
        assert_eq!(seen[0].len(), 1, "round {round}: {seen:?}");
        let want = if final_at < deadline {
            StopOutcome::Graceful
        } else {
            StopOutcome::Killed
        };
        assert_eq!(
            seen[0][0].1,
            want,
            "round {round}: the end became final {:?} before the deadline, {:?} after it",
            deadline.saturating_duration_since(final_at),
            final_at.saturating_duration_since(deadline)
        );
        if !sides.contains(&want) {
            sides.push(want);
        }
        leftover.task().abort();
        round += 1;
    }
    assert!(
        sides.contains(&StopOutcome::Graceful) && sides.contains(&StopOutcome::Killed),
        "the rounds reach both sides of the deadline in {round} rounds: {sides:?}"
    );
}

macro_rules! both_flavors {
        ($($body:ident => $multi:ident, $single:ident;)*) => {$(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $multi() {
                $body().await;
            }

            #[tokio::test(flavor = "current_thread")]
            async fn $single() {
                $body().await;
            }
        )*};
    }

both_flavors! {
    reactivate_waits_for_every_shutdown_in_flight =>
        reactivate_waits_for_every_shutdown_in_flight_multi_thread,
        reactivate_waits_for_every_shutdown_in_flight_current_thread;
}

// - The shutdown count and the phase it leaves behind --------------------

fn phase_of(sys: &ActorSystem) -> SystemPhase {
    SystemPhase::from_u8(sys.phase.load(Ordering::Acquire))
}

fn ends(guard: ShutdownInFlight<Arc<ActorSystem>>, completed: bool) {
    let mut guard = guard;
    guard.completed = completed;
    drop(guard);
}

/// A shutdown that completes while a cut-short one is still in flight
/// leaves the move to `Defunct` to whichever ends last.
#[test]
fn a_completed_shutdown_that_ends_first_leaves_defunct_when_the_cut_short_one_ends() {
    let sys = scripted_system("episode-completed-first");
    let completed = ShutdownInFlight::enter(sys.clone());
    let cut_short = ShutdownInFlight::enter(sys.clone());
    ends(completed, true);
    assert_eq!(phase_of(&sys), SystemPhase::ShuttingDown);
    assert!(!sys.reactivate(), "a shutdown is still in flight");
    ends(cut_short, false);
    assert_eq!(phase_of(&sys), SystemPhase::Defunct);
    assert!(sys.reactivate());
}

// - A kill never arms the incarnation a supervisor restarted under its id -

/// Counts its starts; does nothing else.
struct Idle {
    builds: Arc<AtomicU64>,
}

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn on_started(
        &mut self,
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        Ok(())
    }
}

/// Supervises one permanent `Idle` child under `child`.
struct IdleBoss {
    child: String,
    builds: Arc<AtomicU64>,
}

impl Actor for IdleBoss {
    type Message = ();
    type Response = ();

    async fn on_started(
        &mut self,
        ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        let builds = self.builds.clone();
        ctx.spawn_child(move || Idle {
            builds: builds.clone(),
        })
        .named(self.child.clone())
        .restart_type(crate::types::RestartType::Permanent)
        .await?;
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum KillCall {
    ByName,
    ById,
}

/// A supervised permanent child is killed; between the kill's raise and
/// its re-read of the registry, the caller waits until the supervisor has
/// restarted the child under the same id and name. The kill arms nothing:
/// the incarnation it raised on is gone, and the restarted one is not it.
/// `actors` is the runtime the supervisor and its child live on (the
/// caller's when `None`).
async fn a_kill_never_arms_the_restarted_incarnation(call: KillCall, actors: Option<Handle>) {
    use crate::actor::ActorExt;

    let sys = ActorSystem::create(format!("restarted-{}", uuid::Uuid::new_v4())).unwrap();
    let builds = Arc::new(AtomicU64::new(0));
    let child_name = format!("restarted-child-{}", uuid::Uuid::new_v4());
    let spawn_boss = {
        let (sys, child, builds) = (sys.clone(), child_name.clone(), builds.clone());
        async move {
            IdleBoss { child, builds }
                .spawn()
                .named(format!("restarted-boss-{}", uuid::Uuid::new_v4()))
                .on_system(&sys)
                .supervisor()
                .await
                .unwrap();
        }
    };
    match &actors {
        Some(rt) => rt.spawn(spawn_boss).await.unwrap(),
        None => spawn_boss.await,
    }
    until("the first incarnation to start", || {
        builds.load(Ordering::SeqCst) == 1
    })
    .await;
    let child = loop {
        if let Some(child) = sys.get::<Idle>(&child_name) {
            break child;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    {
        // The caller is the test's own thread (never a worker of the
        // actors' runtime), so holding it here lets the supervisor run.
        let builds = builds.clone();
        *sys.after_kill_raise.lock().unwrap() = Some(Arc::new(move || {
            let bound = std::time::Instant::now() + Duration::from_secs(10);
            while builds.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < bound {
                std::thread::sleep(Duration::from_millis(1));
            }
        }));
    }
    match call {
        KillCall::ByName => sys.kill(&child_name).await.unwrap(),
        KillCall::ById => assert!(sys.kill_by_id(child.id()).await.is_some()),
    }
    assert_eq!(
        builds.load(Ordering::SeqCst),
        2,
        "{call:?}: the supervisor restarted the killed incarnation before the re-read"
    );
    assert_eq!(
        sys.backstop_arms.load(Ordering::SeqCst),
        0,
        "{call:?}: the kill armed the restarted incarnation"
    );
    // Past the restarted incarnation's grace: it was not aborted, so the
    // supervisor built nothing more.
    tokio::time::sleep(DEFAULT_KILL_GRACE + ABORT_SETTLE + Duration::from_millis(300)).await;
    assert_eq!(builds.load(Ordering::SeqCst), 2, "{call:?}: it was aborted");
}

/// The caller on a `current_thread` runtime, the supervisor and its child
/// on a second runtime that keeps running while the caller is held.
fn a_kill_from_a_current_thread_caller_never_arms_the_restarted_incarnation(call: KillCall) {
    let actors = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let on = actors.handle().clone();
    caller.block_on(a_kill_never_arms_the_restarted_incarnation(call, Some(on)));
    caller.shutdown_background();
    actors.shutdown_background();
}

/// A root that parks in `on_started`: a `Kill` raised on it is never
/// observed, and only an abort ends its task.
struct Parked;

impl Actor for Parked {
    type Message = ();
    type Response = ();

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        Ok(())
    }

    async fn on_started(
        &mut self,
        _ctx: &mut crate::actor::context::ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// Spawns a parked root on `sys`. The returned status sender outlives the
/// task: publishing `Stopped` through it after the abort stands for an
/// actor that, still inside a synchronous `Drop` when its task was
/// aborted, reached its end and published `Stopped` itself; dropping it
/// stands for a task the abort ended.
fn spawn_parked(
    sys: &Arc<ActorSystem>,
) -> (
    ActorHandle<Parked>,
    tokio::task::JoinHandle<crate::types::StopReason>,
    StatusTx,
) {
    let name = format!("parked-{}", uuid::Uuid::new_v4());
    let (handle, join, status_tx, _subtree) = crate::actor::runtime::spawn_actor_parts(
        ActorId::from(name.clone()),
        Parked,
        crate::actor::runtime::ActorConfig::default(),
        Some(Arc::from(name)),
        Some(sys.clone()),
        None,
        true,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the actor spawns");
    (handle, join, status_tx)
}

/// Moves the paused clock forward until the aborted task has ended; the
/// caller's settle wait has just begun.
async fn advance_until_finished(join: &tokio::task::JoinHandle<crate::types::StopReason>) {
    while !join.is_finished() {
        tokio::time::advance(Duration::from_millis(1)).await;
    }
}

/// The force of a final-sweep leftover whose registry entry is freed:
/// the sweep raises nothing on it and has no abort of its own, so the
/// only `Kill` and abort it gets are the two-stage entry the sweep fed at
/// its snapshot. The reaper is held back by a gate: the entry is fed only
/// `late` after its `Kill` instant, which is how an entry behaves whose
/// `Kill` stage fires late. The force is judged by the reaper's record:
/// `Aborted` when that abort ends the leftover, `Unresponsive` only the
/// settle time after it.
mod late_kill_sweep {
    use super::*;

    const GRACE: Duration = Duration::from_millis(100);
    const LATE: Duration = Duration::from_millis(150);

    /// A leftover in its teardown: no request reaches it, only an abort
    /// ends its task. With `stuck`, its plane stays open past its task.
    async fn force_leftover(late: Duration, stuck: bool) -> (StopReport, Instant, Instant) {
        let (lane, _lane_rx) = StopLane::new();
        let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let plane = stuck.then(|| status_tx.clone());
        let task = tokio::spawn(async move {
            let _status = status_tx;
            std::future::pending::<()>().await;
        });
        let deadline = Instant::now();
        let entry = ReaperEntry::two_stage(
            deadline,
            lane.clone(),
            deadline + GRACE,
            task.abort_handle(),
        );
        let system = ActorSystem::create(format!("late-kill-{}", uuid::Uuid::new_v4())).unwrap();
        let feeder = system.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline + late).await;
            feeder.feed_reaper(&Handle::current(), entry);
        });
        let report = force_stop(
            status_rx,
            None,
            lane.abort_record().clone(),
            GRACE,
            StopOutcome::Graceful,
        )
        .await;
        let reported = Instant::now();
        drop(plane);
        (report, deadline, reported)
    }

    #[tokio::test(start_paused = true)]
    async fn a_stuck_leftover_is_unresponsive_the_settle_time_after_a_late_abort() {
        let (report, deadline, at) = force_leftover(LATE, true).await;
        assert_eq!(report.outcome, StopOutcome::Unresponsive);
        assert_eq!(at, deadline + LATE + GRACE + ABORT_SETTLE);
    }
}

/// A mark never outlives its task. A hold of the actor can: a forwarder
/// still ending holds it past the task's end. Each of these releases the
/// mark instead of skipping it when it sees the task finished: an abort
/// that finds it so, the join arm of a ladder, an observer of a lost
/// runner, the reaper's entry, and the drop of the hold. The release
/// counts as the subtree's latest abort, so a wait on the subtree gives
/// its verdict after the settle time, never at the cap. An entry built over a task that already
/// finished takes no mark at all.
mod finished_task {
    use super::*;
    use crate::actor::incarnation::StopLane;
    use crate::actor::incarnation::SubtreeLink;
    use tokio::task::JoinHandle;

    const WINDOW: Duration = Duration::from_millis(400);
    const GRACE: Duration = Duration::from_millis(100);

    /// A root and one child below it, the child's lane record linked to
    /// its token, the child's task spawned and not yet run: it finishes
    /// at its first poll, once the test awaits it.
    fn child_task() -> (Arc<SubtreeHold>, Arc<SubtreeHold>, StopLane, JoinHandle<()>) {
        let root = SubtreeHold::new(None);
        let (lane, _lane_rx) = StopLane::new();
        let link = SubtreeLink {
            parent: root.token(),
            own: WINDOW + GRACE,
            infinity: false,
            trap_extra: std::time::Duration::ZERO,
            trap_infinity: false,
        };
        let child = SubtreeHold::with_record(Some(link), lane.abort_record().clone());
        let task = tokio::spawn(async {});
        (root, child, lane, task)
    }

    /// A root and one child below it, the child's lane record linked to
    /// its token, the child's task finished while the hold lives on.
    async fn finished_child() -> (Arc<SubtreeHold>, Arc<SubtreeHold>, StopLane, AbortHandle) {
        let (root, child, lane, task) = child_task();
        let abort = task.abort_handle();
        task.await.unwrap();
        (root, child, lane, abort)
    }

    /// The mark is released, the end recorded on the subtree, nothing
    /// recorded on the lane, and no mark is taken again.
    fn released(root: &SubtreeHold, lane: &StopLane, at: Instant) {
        assert_eq!(root.watch().abort_view(), (Some(at), false));
        assert_eq!(lane.abort_record().view(), (None, false));
        lane.abort_record().owe();
        assert_eq!(
            root.watch().abort_view(),
            (Some(at), false),
            "nothing is owed to a task that finished"
        );
    }
}

/// Every abort the system issues against an actor records its instant on
/// the actor's lane. The reaper here is a stand-in that keeps every entry
/// it is fed and fires none, so the only abort an actor gets is the one
/// the path under test issues, and its record is read on the paused
/// clock.
mod abort_writers {
    use super::*;

    const GRACE: Duration = Duration::from_millis(100);

    /// An actor whose `pre_start` never completes: it hands out its stop
    /// lane first.
    struct HangsInPreStart {
        lane: Option<tokio::sync::oneshot::Sender<StopLane>>,
    }

    impl Actor for HangsInPreStart {
        type Message = ();
        type Response = ();

        async fn pre_start(
            &mut self,
            ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            if let Some(lane) = self.lane.take() {
                let _ = lane.send(ctx.self_handle().stop_lane());
            }
            std::future::pending::<()>().await;
            Ok(())
        }

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }
}

/// The helper task that walks a shutdown.
mod walk_helper {
    use super::*;
    use crate::actor::ActorExt;

    fn walk_ends(sys: &ActorSystem) -> Vec<crate::helper::HelperEnd> {
        sys.helper_ends(&Handle::current())
            .into_iter()
            .filter(|(kind, _)| *kind == HelperKind::ShutdownWalk)
            .map(|(_, end)| end)
            .collect()
    }

    /// Waits until `count` walk ends are recorded. The bound only limits how long a real hang takes to report.
    async fn until_walk_ends(sys: &ActorSystem, count: usize) {
        let waited = tokio::time::timeout(Duration::from_secs(60), async {
            while walk_ends(sys).len() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "the walk helper's end is recorded");
    }

    /// A walk that runs to its end is recorded once, as finished, and
    /// leaves the system defunct.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_finished_walk_is_recorded_once() {
        let sys = scripted_system("walk-finishes");
        let root = Dummy
            .spawn()
            .named("walk-finishes-root")
            .on_system(&sys)
            .await
            .unwrap();
        let report = tokio::time::timeout(Duration::from_secs(10), sys.shutdown())
            .await
            .expect("the shutdown returns");
        assert_eq!(outcome_of(&report, root.id()), StopOutcome::Graceful);
        until_walk_ends(&sys, 1).await;
        assert_eq!(walk_ends(&sys), vec![crate::helper::HelperEnd::Finished]);
        assert!(sys.reactivate());
    }
}
