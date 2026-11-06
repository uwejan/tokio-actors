//! Whether a Tokio runtime has the time driver.
//!
//! A timer, the kill grace, every bound on a wait and the stop ladder's
//! deadlines all sleep on the time driver. Tokio has no query for it, and a
//! sleep created on a runtime without it panics, so the crate asks once per
//! runtime, the first time a call needs the answer, by creating a zero-length
//! sleep under `catch_unwind` and polling it once. `start_timeout`,
//! `stop_within` and `kill_by_id` panic, with a message that names how to
//! enable the driver, when the calling runtime was built without it
//! (`stop_within` and `kill_by_id` also when no runtime is current), before
//! they change anything, unless a rule that needs no timer answers the call
//! first; `SubtreeObserver::wait_ended` makes the same check at the first
//! moment it needs a timer, which is once the observed actor has ended and let
//! go of everything it held and an actor below it has not ended, and a call
//! that finds none needs none; the sweep of long turns never starts on a
//! runtime without it.
//! Spawning a plain actor (an actor with no timer and no supervised child,
//! spawned without `start_timeout`) does not panic for lack of the driver and
//! runs on a runtime without it. A spawn asks about the runtime only to decide
//! whether the sweep of long turns can start (with the `tracing` feature and a
//! turn threshold set), because it names `start_timeout`, or, for a supervised
//! child, because it names `on_runtime`.
//!
//! Under `panic = "abort"` there is nothing to probe with: the functions
//! here answer yes, and the first timer of the crate on a runtime without the
//! driver ends the process, as the crate docs say.
//!
//! Every answer is kept, the two negative ones included: a live runtime never
//! gains or loses its driver, and a runtime that has been shut down never
//! runs again, so a runtime is asked about once. Answers are kept until the
//! table holds 1,024 runtimes; then all of them are forgotten and asked
//! again, on every thread together.
//! A repeated check of a runtime without the driver that nobody drives would
//! otherwise queue a probe task on it at every call.
//! The cache is keyed by `Handle::id()`: a thread-local "last seen" entry
//! serves a call that repeats on one runtime with a single comparison, so no
//! hot path takes a lock, and a small process-wide table is read only when
//! that entry misses. The cache rests on one assumption: Tokio 1.53 through 1.54
//! draw runtime ids from one process-wide counter (checked at 1.53.x and
//! 1.54.0), `NEXT_OWNED_TASKS_ID` in
//! `tokio/src/runtime/task/list.rs` (lines 31 to 40, with a 32-bit twin at
//! lines 46 to 55 on targets without 64-bit atomics), so an id is not reused
//! while the process lives. Tokio documents reuse of an id after its runtime
//! ended as allowed, so the assumption is
//! checked again whenever the Tokio floor changes, and a test builds and drops
//! many runtimes and asserts that no id repeats.
//!
//! With a negative answer, Tokio's own diagnostic for the failed sleep is
//! printed by the panic hook once, at the probe. A panic hook that ends the process
//! also ends it at a call that finds no time driver: the crate catches the
//! probe's panic, but the hook runs before the catch.
//!
//! A runtime that has been shut down fails the probe too, but later than one
//! built without the driver: the sleep is created, and the first poll panics.
//! [`runtime_timer_state`] tells the two apart, for a caller that places an
//! actor on a runtime the user hands over and must not treat an ended runtime
//! as one that lacks the driver. It asks the runtime whether it is shut down
//! before it probes, without a panic: a task spawned on a runtime that has
//! ended is cancelled before `spawn` returns, so the first poll of its
//! `JoinHandle` reports the cancellation, whereas a task spawned on a live
//! runtime does not. The panic hook therefore never runs for a runtime that
//! has ended, and a runtime that has ended is `ShutDown` whether or not it
//! had the driver. Every entry goes through [`runtime_timer_state`], so
//! [`runtime_has_timers`] and [`probe_runtime_for_timers`] answer no for a
//! runtime that is ending or has ended, without a panic. The check runs only
//! after a miss of the cache, so a runtime that is already known is never
//! asked again, and the one task it spawns on any other runtime shows in that
//! runtime's task counts and task metrics until the runtime runs it.

#[cfg(panic = "unwind")]
use std::cell::Cell;
#[cfg(panic = "unwind")]
use std::collections::HashMap;
#[cfg(panic = "unwind")]
use std::future::Future;
#[cfg(panic = "unwind")]
use std::panic::{catch_unwind, AssertUnwindSafe};
#[cfg(panic = "unwind")]
use std::pin::pin;
#[cfg(panic = "unwind")]
use std::sync::atomic::Ordering;
#[cfg(panic = "unwind")]
use std::sync::{Mutex, OnceLock};
#[cfg(panic = "unwind")]
use std::task::{Context, Poll, Waker};
#[cfg(panic = "unwind")]
use std::time::Duration;

use tokio::runtime::Handle;
#[cfg(panic = "unwind")]
use tokio::runtime::Id;

/// How many runtimes the process-wide table keeps before it starts over. A
/// table that starts over forgets every answer, positive ones too, and asks
/// again: a runtime still in use gets the same answer, and one that has ended
/// since reads as shut down, on every thread alike. It only bounds the memory
/// of a process that builds runtimes without end.
#[cfg(panic = "unwind")]
const TABLE_LIMIT: usize = 1024;

/// How many times the table has started over. A thread's last-seen entry
/// counts only while its generation is this one, so a start over forgets the
/// entries of every thread together with the table's.
#[cfg(panic = "unwind")]
static TABLE_GENERATION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(panic = "unwind")]
thread_local! {
    /// The last runtime this thread asked about that has the driver, and the
    /// generation of the table when it was kept.
    static LAST: Cell<Option<(Id, usize)>> = const { Cell::new(None) };
}

#[cfg(all(test, panic = "unwind"))]
thread_local! {
    /// How many times this thread has locked the process-wide table.
    static TABLE_LOCKS: Cell<usize> = const { Cell::new(0) };
}

/// Locks the process-wide table of answers, one per runtime.
#[cfg(panic = "unwind")]
fn lock_table() -> std::sync::MutexGuard<'static, HashMap<Id, TimerState>> {
    static TABLE: OnceLock<Mutex<HashMap<Id, TimerState>>> = OnceLock::new();
    #[cfg(test)]
    TABLE_LOCKS.with(|locks| locks.set(locks.get() + 1));
    TABLE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Held for the whole of a test that reads, counts or clears the
/// process-wide table, so that a test which fills the table past its limit
/// and starts it over never runs beside another that expects its answers.
#[cfg(all(test, panic = "unwind"))]
pub(crate) fn serial_table() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The probes run so far for each runtime, for the tests that pin that a
/// runtime is asked about once.
#[cfg(all(test, panic = "unwind"))]
fn probe_counts() -> &'static Mutex<HashMap<Id, usize>> {
    static COUNTS: OnceLock<Mutex<HashMap<Id, usize>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How many times the runtime `rt` has been probed.
#[cfg(all(test, panic = "unwind"))]
pub(crate) fn probes_run(rt: &Handle) -> usize {
    probe_counts()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&rt.id())
        .copied()
        .unwrap_or(0)
}

/// Records one more probe of `rt`, for the tests that count them.
#[cfg(panic = "unwind")]
fn count_probe(rt: &Handle) {
    #[cfg(test)]
    {
        *probe_counts()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(rt.id())
            .or_insert(0) += 1;
    }
    #[cfg(not(test))]
    let _ = rt;
}

/// What the time driver looks like from outside a runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimerState {
    /// A sleep on the runtime is created and polled.
    Has,
    /// The runtime was built without the driver: creating a sleep panics.
    Missing,
    /// The runtime has been shut down, with or without the driver: work
    /// placed on it ends at once.
    #[cfg(panic = "unwind")]
    ShutDown,
}

/// Whether the runtime `rt` has been shut down, asked without a panic. It
/// spawns a task that does nothing on `rt` and polls its `JoinHandle` once:
/// a runtime that has ended cancels the task before `spawn` returns, so the
/// first poll is `Ready` with a cancellation, and a live runtime never
/// answers so. The poll runs outside the caller's cooperative budget, so a
/// caller that has spent its budget in the current poll still reads the
/// answer instead of a `Pending` that the budget forced. The task of a live
/// runtime is left to run and its handle is dropped.
#[cfg(panic = "unwind")]
fn runtime_is_shut_down(rt: &Handle) -> bool {
    let mut join = pin!(tokio::task::unconstrained(rt.spawn(async {})));
    let mut cx = Context::from_waker(Waker::noop());
    matches!(
        join.as_mut().poll(&mut cx),
        Poll::Ready(Err(error)) if error.is_cancelled()
    )
}

/// Creates a zero-length sleep on `rt` and polls it once, telling a panic at
/// the creation from a panic at the first poll. A runtime that ended after
/// the shutdown check reads as shut down by the panic of the poll.
#[cfg(panic = "unwind")]
fn probe_state(rt: &Handle) -> TimerState {
    count_probe(rt);
    let _entered = rt.enter();
    let Ok(sleep) = catch_unwind(AssertUnwindSafe(|| tokio::time::sleep(Duration::ZERO))) else {
        return TimerState::Missing;
    };
    let polled = catch_unwind(AssertUnwindSafe(|| {
        let mut sleep = pin!(sleep);
        let mut cx = Context::from_waker(Waker::noop());
        let _ = sleep.as_mut().poll(&mut cx);
    }));
    if polled.is_ok() {
        TimerState::Has
    } else {
        TimerState::ShutDown
    }
}

/// The answer already kept for `rt`: the thread's last positive answer if it
/// is from the table's current generation, then the process-wide table. It
/// never probes.
#[cfg(panic = "unwind")]
fn cached_state(rt: &Handle) -> Option<TimerState> {
    let id = rt.id();
    if let Some((last, generation)) = LAST.with(Cell::get) {
        if last == id && generation == TABLE_GENERATION.load(Ordering::Acquire) {
            return Some(TimerState::Has);
        }
    }
    let table = lock_table();
    let state = table.get(&id).copied()?;
    if state == TimerState::Has {
        let generation = TABLE_GENERATION.load(Ordering::Acquire);
        LAST.with(|last| last.set(Some((id, generation))));
    }
    Some(state)
}

/// Keeps `state` as the answer for `rt` unless an answer is kept already,
/// and returns the answer that is kept: the first one stored stays, so a
/// positive answer that another thread stored before the runtime ended is
/// never turned into a negative one.
#[cfg(panic = "unwind")]
fn remember(rt: &Handle, state: TimerState) -> TimerState {
    let id = rt.id();
    let mut table = lock_table();
    if table.len() >= TABLE_LIMIT {
        table.clear();
        TABLE_GENERATION.fetch_add(1, Ordering::AcqRel);
    }
    let kept = *table.entry(id).or_insert(state);
    let generation = TABLE_GENERATION.load(Ordering::Acquire);
    drop(table);
    if kept == TimerState::Has {
        LAST.with(|last| last.set(Some((id, generation))));
    }
    kept
}

/// Whether the runtime `rt` is already known to have the time driver: the
/// thread's last answer, then the process-wide table. It never probes and
/// never answers for a runtime without the driver. Always `true` when the
/// build cannot unwind, where no probe exists.
#[cfg(any(test, feature = "tracing"))]
pub(crate) fn runtime_known_to_have_timers(rt: &Handle) -> bool {
    #[cfg(panic = "unwind")]
    {
        cached_state(rt) == Some(TimerState::Has)
    }
    #[cfg(not(panic = "unwind"))]
    {
        let _ = rt;
        true
    }
}

/// Whether the runtime `rt` has the time driver, probing it when no answer is
/// kept. Any kept answer serves, a negative one included: no caller of this
/// function can tell a runtime without the driver from one that is ending or
/// has ended, so a kept negative answer never spawns or probes again. A
/// runtime that is ending or has ended answers no. Always `true` when the
/// build cannot unwind, where no probe exists.
pub(crate) fn probe_runtime_for_timers(rt: &Handle) -> bool {
    #[cfg(panic = "unwind")]
    {
        match cached_state(rt) {
            Some(state) => state == TimerState::Has,
            None => runtime_timer_state(rt) == TimerState::Has,
        }
    }
    #[cfg(not(panic = "unwind"))]
    {
        let _ = rt;
        true
    }
}

/// Whether the runtime `rt` has the time driver. The first call for a
/// runtime probes it, and every answer is kept: every later call reads the
/// cache. A runtime that is ending or has ended answers no, as one without
/// the driver does; [`runtime_timer_state`] tells them apart. Always `true`
/// when the build cannot unwind, where no probe exists.
pub(crate) fn runtime_has_timers(rt: &Handle) -> bool {
    probe_runtime_for_timers(rt)
}

/// Whether the runtime `rt` has the time driver, lacks it, or is ending or
/// has ended. Every answer is kept for the life of the process: a positive
/// answer wins, so a runtime that was probed before it ended keeps answering
/// [`TimerState::Has`], and a kept `ShutDown` is final. A kept `Missing` is
/// not trusted here, because a runtime built without the driver can end
/// later and then reads `ShutDown`: it asks again whether the runtime has
/// ended, without a timer probe. That ask spawns one task on a live runtime,
/// so the callers on a path that can repeat (the bool functions above) read
/// the kept answer instead, and this function serves the entry checks, where
/// `Missing` ends in a panic. On a miss, a runtime that is ending or has
/// ended is [`TimerState::ShutDown`], found without a timer probe and
/// without a panic, so the panic hook does not run for it; the timer probe
/// follows only for a live runtime. The probe runs outside the table's lock:
/// the panic hook of a runtime without the driver runs inside it, and a hook
/// may call anything. Two threads that ask about a new runtime at the same
/// moment both probe, and the first answer stored stays. Always
/// [`TimerState::Has`] when the build cannot unwind, where no probe exists.
pub(crate) fn runtime_timer_state(rt: &Handle) -> TimerState {
    #[cfg(panic = "unwind")]
    {
        match cached_state(rt) {
            Some(TimerState::Missing) => {
                return if runtime_is_shut_down(rt) {
                    TimerState::ShutDown
                } else {
                    TimerState::Missing
                };
            }
            Some(state) => return state,
            None => {}
        }
        let state = if runtime_is_shut_down(rt) {
            TimerState::ShutDown
        } else {
            probe_state(rt)
        };
        match remember(rt, state) {
            TimerState::Missing => state,
            kept => kept,
        }
    }
    #[cfg(not(panic = "unwind"))]
    {
        let _ = rt;
        TimerState::Has
    }
}

/// Panics, naming `op`, when `rt` was built without the time driver. A
/// runtime that has been shut down passes, whether or not it was built with
/// the driver: the work placed on it ends at once by the rules that cover a
/// runtime that goes away, whereas a live runtime without the driver would
/// drop every wait the crate bounds on it.
/// Call it first, before the call raises, registers or spawns anything.
///
/// A kept `Missing` is not trusted, because the runtime can end later and
/// then passes; the check asks the runtime again whether it has ended. That
/// ask spawns one task on a live runtime, so a caller that catches the panic
/// and repeats the call on a live runtime without the driver queues one probe
/// task on it per call. The panic is the cold path, and the cost is accepted.
pub(crate) fn require_runtime_timers(op: &'static str, rt: &Handle) {
    if runtime_timer_state(rt) == TimerState::Missing {
        panic!(
            "tokio-actors: {op} needs the Tokio time driver on the runtime it places the \
             actor on (Builder::enable_time or enable_all)"
        );
    }
}

/// Panics, naming `op`, when the calling thread has no Tokio runtime or the
/// runtime current on it was built without the time driver. A calling runtime
/// that is shutting down or has ended passes, whatever the cache holds: the
/// check refuses a runtime without the driver and nothing else. A call that bounds its wait
/// sleeps on the calling runtime, so without the driver it would panic inside
/// the wait instead, after it had already changed state. Call it first, before
/// the call raises, registers or spawns anything. Does nothing when the build
/// cannot unwind and a runtime is current.
pub(crate) fn require_caller_timers(op: &'static str) {
    caller_check(op, false);
}

/// The same check for a call that fails by its own rule when the calling
/// thread has no runtime: it panics only when a runtime is current and lacks
/// the time driver.
pub(crate) fn require_timers_if_in_runtime(op: &'static str) {
    caller_check(op, true);
}

fn caller_check(op: &'static str, none_is_ok: bool) {
    match Handle::try_current() {
        Ok(rt) => {
            if runtime_timer_state(&rt) == TimerState::Missing {
                panic!(
                    "tokio-actors: {op} needs the Tokio time driver on the calling runtime \
                     (Builder::enable_time or enable_all)"
                );
            }
        }
        Err(_) => {
            if !none_is_ok {
                panic!(
                    "tokio-actors: {op} needs a Tokio runtime with the time driver on the \
                     calling thread (Builder::enable_time or enable_all)"
                );
            }
        }
    }
}

#[cfg(all(test, panic = "unwind"))]
mod tests {
    use super::*;
    use tokio::runtime::Builder;

    fn without_timers() -> tokio::runtime::Runtime {
        Builder::new_current_thread().build().unwrap()
    }

    fn with_timers() -> tokio::runtime::Runtime {
        Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn a_runtime_with_the_driver_answers_yes_and_a_runtime_without_it_no() {
        let _serial = serial_table();
        let yes = with_timers();
        let no = without_timers();
        assert!(runtime_has_timers(yes.handle()));
        assert!(!runtime_has_timers(no.handle()));
    }

    /// A runtime that had the driver and was shut down, probed for the first
    /// time after it ended.
    fn shut_down_with_timers() -> Handle {
        let rt = with_timers();
        let handle = rt.handle().clone();
        drop(rt);
        handle
    }

    #[test]
    fn the_timer_state_tells_a_shut_down_runtime_from_one_without_the_driver() {
        let _serial = serial_table();
        let yes = with_timers();
        let no = without_timers();
        let ended = shut_down_with_timers();
        let ended_bare = {
            let rt = without_timers();
            let handle = rt.handle().clone();
            drop(rt);
            handle
        };
        assert_eq!(runtime_timer_state(yes.handle()), TimerState::Has);
        assert_eq!(runtime_timer_state(no.handle()), TimerState::Missing);
        assert_eq!(runtime_timer_state(&ended), TimerState::ShutDown);
        assert_eq!(
            runtime_timer_state(&ended_bare),
            TimerState::ShutDown,
            "an ended runtime is shut down whether or not it had the driver"
        );
        assert!(
            !runtime_has_timers(&ended),
            "the plain probe reads a shut-down runtime as one without the driver"
        );
    }

    /// The probe itself, called on a runtime that has ended: creating the
    /// sleep succeeds when the runtime had the driver and polling it panics,
    /// which reads as a shut-down runtime; without the driver the creation
    /// panics, which reads as `Missing`.
    #[cfg(panic = "unwind")]
    #[test]
    fn the_timer_probe_reads_a_poll_that_panics_as_a_shut_down_runtime() {
        let _serial = serial_table();
        let ended = shut_down_with_timers();
        assert_eq!(probe_state(&ended), TimerState::ShutDown);
        let ended_bare = {
            let rt = without_timers();
            let handle = rt.handle().clone();
            drop(rt);
            handle
        };
        assert_eq!(probe_state(&ended_bare), TimerState::Missing);
    }

    #[test]
    fn the_shut_down_check_answers_for_each_kind_of_runtime() {
        let yes = with_timers();
        let no = without_timers();
        let ended = shut_down_with_timers();
        assert!(!runtime_is_shut_down(yes.handle()));
        assert!(!runtime_is_shut_down(no.handle()));
        assert!(runtime_is_shut_down(&ended));
    }

    #[test]
    fn the_shut_down_check_is_not_cut_short_by_a_spent_budget() {
        let _serial = serial_table();
        let host = with_timers();
        let ended = shut_down_with_timers();
        host.block_on(async {
            // Spend the budget of this poll: Tokio answers `Pending` for a
            // channel that holds values.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u8>();
            for _ in 0..1000 {
                tx.send(0).unwrap();
            }
            std::future::poll_fn(|cx| {
                while rx.poll_recv(cx).is_ready() {}
                Poll::Ready(())
            })
            .await;
            let mut cx = Context::from_waker(Waker::noop());
            assert!(rx.poll_recv(&mut cx).is_pending(), "the budget is spent");
            // The cancelled task of an ended runtime is held back by it too.
            let mut plain = pin!(ended.spawn(async {}));
            assert!(plain.as_mut().poll(&mut cx).is_pending());
            assert!(runtime_is_shut_down(&ended));
            assert!(!runtime_is_shut_down(&Handle::current()));
        });
    }

    thread_local! {
        /// How many times the panic hook has run on this thread.
        static HOOK_RUNS: Cell<usize> = const { Cell::new(0) };
    }

    /// Chains a hook that counts the panics of the thread it runs on, once.
    fn count_hook_runs() {
        static HOOK: std::sync::Once = std::sync::Once::new();
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                HOOK_RUNS.with(|runs| runs.set(runs.get() + 1));
                previous(info);
            }));
        });
    }

    fn hook_runs() -> usize {
        HOOK_RUNS.with(Cell::get)
    }

    #[test]
    fn an_ended_runtime_never_reaches_the_panic_hook_through_any_entry() {
        let _serial = serial_table();
        count_hook_runs();
        let ended = shut_down_with_timers();
        let ended_bare = {
            let rt = without_timers();
            let handle = rt.handle().clone();
            drop(rt);
            handle
        };
        let before = hook_runs();
        for rt in [&ended, &ended_bare] {
            assert!(!runtime_has_timers(rt));
            assert!(!probe_runtime_for_timers(rt));
            assert_eq!(runtime_timer_state(rt), TimerState::ShutDown);
            require_runtime_timers("Example::op", rt);
        }
        assert_eq!(hook_runs(), before, "no panic reached the hook");
        // The counter sees the panic of a live runtime without the driver.
        let no = without_timers();
        assert!(!runtime_has_timers(no.handle()));
        assert!(hook_runs() > before, "the probe of a live runtime panics");
    }

    #[test]
    fn repeated_checks_of_an_undriven_runtime_without_the_driver_queue_at_most_one_task() {
        let _serial = serial_table();
        let undriven = without_timers();
        for _ in 0..1000 {
            assert!(!runtime_has_timers(undriven.handle()));
            assert!(!probe_runtime_for_timers(undriven.handle()));
        }
        let queued = undriven.handle().metrics().num_alive_tasks();
        assert!(
            queued <= 1,
            "{queued} tasks queued on a runtime nobody runs"
        );
        assert_eq!(probes_run(undriven.handle()), 1);
    }

    #[test]
    fn a_kept_answer_for_a_runtime_without_the_driver_goes_stale_when_it_ends() {
        let _serial = serial_table();
        let rt = without_timers();
        let handle = rt.handle().clone();
        assert!(!runtime_has_timers(&handle));
        assert_eq!(lock_table().get(&handle.id()), Some(&TimerState::Missing));
        assert_eq!(runtime_timer_state(&handle), TimerState::Missing);
        drop(rt);
        // The entry check does not panic for it, as for a runtime that
        // was never probed; the bool answer stays no.
        assert_eq!(runtime_timer_state(&handle), TimerState::ShutDown);
        require_runtime_timers("Example::op", &handle);
        assert!(!runtime_has_timers(&handle));
    }

    #[test]
    fn the_first_answer_stored_for_a_runtime_stays() {
        let _serial = serial_table();
        let rt = with_timers();
        let handle = rt.handle().clone();
        assert_eq!(remember(&handle, TimerState::Has), TimerState::Has);
        assert_eq!(remember(&handle, TimerState::ShutDown), TimerState::Has);
        drop(rt);
        assert!(runtime_has_timers(&handle));
        assert_eq!(runtime_timer_state(&handle), TimerState::Has);

        let other = without_timers();
        let handle = other.handle().clone();
        assert_eq!(
            remember(&handle, TimerState::ShutDown),
            TimerState::ShutDown
        );
        assert_eq!(remember(&handle, TimerState::Has), TimerState::ShutDown);
        assert!(!runtime_has_timers(&handle));
    }

    /// Runs `require_caller_timers` from the drop of a task, which happens
    /// while the runtime shuts down, and returns what it did.
    fn caller_check_while_shutting_down(rt: tokio::runtime::Runtime) -> String {
        struct Guard(std::sync::Arc<Mutex<Option<String>>>);
        impl Drop for Guard {
            fn drop(&mut self) {
                let caught = catch_unwind(|| require_caller_timers("Example::op"));
                let text = match caught {
                    Ok(()) => "no panic".to_string(),
                    Err(payload) => payload
                        .downcast_ref::<String>()
                        .cloned()
                        .unwrap_or_else(|| "a panic without text".to_string()),
                };
                *self.0.lock().unwrap() = Some(text);
            }
        }
        let slot = std::sync::Arc::new(Mutex::new(None));
        let guard = Guard(slot.clone());
        rt.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await
        });
        rt.block_on(tokio::task::yield_now());
        drop(rt);
        let text = slot.lock().unwrap().take();
        text.expect("the task was dropped")
    }

    #[test]
    fn a_call_made_while_its_runtime_shuts_down_proceeds_with_or_without_an_earlier_probe() {
        let _serial = serial_table();
        // Never probed: the check asks the runtime, which has closed.
        let fresh = caller_check_while_shutting_down(with_timers());
        assert_eq!(fresh, "no panic");
        // Probed before: the kept answer is positive.
        let rt = with_timers();
        assert!(runtime_has_timers(rt.handle()));
        let probed = caller_check_while_shutting_down(rt);
        assert_eq!(probed, "no panic");
        // Probed before and built without the driver: the kept answer is
        // negative, and the runtime has closed since.
        let bare = without_timers();
        assert!(!runtime_has_timers(bare.handle()));
        let kept_negative = caller_check_while_shutting_down(bare);
        assert_eq!(kept_negative, "no panic");
    }

    #[test]
    fn a_caught_panic_loop_queues_at_most_one_probe_task_per_call() {
        let _serial = serial_table();
        let undriven = without_timers();
        for _ in 0..200 {
            assert!(
                catch_unwind(|| require_runtime_timers("Example::op", undriven.handle())).is_err()
            );
        }
        let queued = undriven.handle().metrics().num_alive_tasks();
        assert!(
            queued <= 200,
            "{queued} probe tasks queued by 200 caught panics"
        );
    }

    #[test]
    fn after_the_table_starts_over_every_thread_reads_the_same_answer() {
        let _serial = serial_table();
        use std::sync::{Arc, Barrier};
        const READERS: usize = 4;
        let first = with_timers();
        let handle = first.handle().clone();
        assert!(runtime_has_timers(&handle));
        let primed = Arc::new(Barrier::new(READERS + 1));
        let flushed = Arc::new(Barrier::new(READERS + 1));
        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let (handle, primed, flushed) = (handle.clone(), primed.clone(), flushed.clone());
                std::thread::spawn(move || {
                    // This thread's last-seen entry holds the first runtime.
                    assert!(runtime_has_timers(&handle));
                    primed.wait();
                    flushed.wait();
                    runtime_timer_state(&handle)
                })
            })
            .collect();
        primed.wait();
        // More runtimes than the table keeps start it over, and the first
        // runtime ends. Each other runtime has the time driver and no I/O
        // driver, so it opens no file descriptor, and it ends as soon as it
        // is probed; runtime ids are never reused, so each is a distinct
        // entry of the table.
        for _ in 0..TABLE_LIMIT + 4 {
            let other = Builder::new_current_thread().enable_time().build().unwrap();
            assert!(runtime_has_timers(other.handle()));
        }
        drop(first);
        flushed.wait();
        let mine = runtime_timer_state(&handle);
        for reader in readers {
            assert_eq!(reader.join().unwrap(), mine, "every thread agrees");
        }
        assert_eq!(mine, TimerState::ShutDown, "the table forgot the answer");
    }

    #[test]
    fn repeated_checks_of_an_undriven_runtime_queue_at_most_one_task() {
        let _serial = serial_table();
        let undriven = with_timers();
        for _ in 0..1000 {
            assert_eq!(runtime_timer_state(undriven.handle()), TimerState::Has);
            assert!(runtime_has_timers(undriven.handle()));
            require_runtime_timers("Example::op", undriven.handle());
        }
        let queued = undriven.handle().metrics().num_alive_tasks();
        assert!(
            queued <= 1,
            "{queued} tasks queued on a runtime nobody runs"
        );
    }

    #[test]
    fn a_runtime_known_to_have_the_driver_keeps_that_state_after_it_ends() {
        let _serial = serial_table();
        let rt = with_timers();
        let handle = rt.handle().clone();
        assert_eq!(runtime_timer_state(&handle), TimerState::Has);
        drop(rt);
        assert_eq!(runtime_timer_state(&handle), TimerState::Has);
        assert_eq!(
            probes_run(&handle),
            1,
            "the cached answer is not probed again"
        );
    }

    #[test]
    fn a_shut_down_runtime_is_found_without_a_timer_probe() {
        let _serial = serial_table();
        let ended = shut_down_with_timers();
        for _ in 0..5 {
            assert_eq!(runtime_timer_state(&ended), TimerState::ShutDown);
        }
        assert_eq!(
            probes_run(&ended),
            0,
            "an ended runtime is found before any timer probe"
        );
        assert!(!runtime_known_to_have_timers(&ended));
        assert_eq!(
            lock_table().get(&ended.id()),
            Some(&TimerState::ShutDown),
            "the answer is kept"
        );
    }

    #[test]
    fn the_runtime_check_panics_only_for_a_runtime_built_without_the_driver() {
        let _serial = serial_table();
        let yes = with_timers();
        let no = without_timers();
        let ended = shut_down_with_timers();
        require_runtime_timers("Example::op", yes.handle());
        require_runtime_timers("Example::op", &ended);
        let caught =
            catch_unwind(|| require_runtime_timers("Example::op", no.handle())).unwrap_err();
        let text = caught.downcast_ref::<String>().map(String::as_str).unwrap();
        assert!(text.contains("Example::op"), "{text}");
        assert!(text.contains("enable_time"), "{text}");
    }

    #[test]
    fn every_answer_is_kept() {
        let _serial = serial_table();
        let yes = with_timers();
        let no = without_timers();
        for _ in 0..100 {
            assert!(runtime_has_timers(yes.handle()));
            assert!(!runtime_has_timers(no.handle()));
        }
        assert_eq!(probes_run(yes.handle()), 1, "a positive answer is kept");
        assert_eq!(probes_run(no.handle()), 1, "a negative one is kept too");
    }

    #[test]
    fn alternating_runtimes_on_one_thread_read_the_table() {
        let _serial = serial_table();
        let a = with_timers();
        let b = without_timers();
        for _ in 0..10 {
            assert!(runtime_has_timers(a.handle()));
            assert!(!runtime_has_timers(b.handle()));
        }
        assert_eq!(probes_run(a.handle()), 1);
        assert_eq!(probes_run(b.handle()), 1);
    }

    #[test]
    fn another_thread_reads_the_positive_answer_the_first_thread_stored() {
        let _serial = serial_table();
        let yes = with_timers();
        assert!(runtime_has_timers(yes.handle()));
        let handle = yes.handle().clone();
        std::thread::spawn({
            let handle = handle.clone();
            move || assert!(runtime_has_timers(&handle))
        })
        .join()
        .unwrap();
        assert_eq!(probes_run(&handle), 1);
    }

    fn table_locks() -> usize {
        TABLE_LOCKS.with(Cell::get)
    }

    #[test]
    fn a_repeated_call_on_one_runtime_never_locks_the_table_again() {
        let _serial = serial_table();
        let yes = with_timers();
        let no = without_timers();
        yes.block_on(async {
            assert!(runtime_has_timers(&Handle::current()));
            let after_first = table_locks();
            assert!(after_first >= 1, "the first call reads the table");
            for _ in 0..1000 {
                assert!(runtime_has_timers(&Handle::current()));
                require_caller_timers("Example::op");
                require_timers_if_in_runtime("Example::op");
            }
            assert_eq!(table_locks(), after_first, "the thread-local entry serves");
        });
        no.block_on(async {
            let id = Handle::current().id();
            let before = probes_run(&Handle::current());
            for _ in 0..1000 {
                assert!(!runtime_has_timers(&Handle::current()));
            }
            assert_eq!(
                probes_run(&Handle::current()),
                before + 1,
                "a runtime without the driver is asked once"
            );
            assert_eq!(
                lock_table().get(&id),
                Some(&TimerState::Missing),
                "the negative answer is stored"
            );
        });
    }

    #[test]
    fn the_entry_follows_the_runtime_when_a_thread_alternates() {
        let _serial = serial_table();
        let a = with_timers();
        let b = without_timers();
        for _ in 0..10 {
            assert!(a.block_on(async { runtime_has_timers(&Handle::current()) }));
            assert!(!b.block_on(async { runtime_has_timers(&Handle::current()) }));
        }
    }

    /// The thread-local entry and the table rest on runtime ids not being
    /// reused while the process lives.
    #[test]
    fn runtime_ids_never_repeat_across_many_built_and_dropped_runtimes() {
        let mut seen = std::collections::HashSet::new();
        for n in 0..3000 {
            let rt = if n % 2 == 0 {
                with_timers()
            } else {
                without_timers()
            };
            let id = rt.handle().id();
            assert!(seen.insert(id), "runtime id {id} was handed out twice");
            drop(rt);
        }
        // Ids drawn on other threads share the one counter.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    (0..200)
                        .map(|_| with_timers().handle().id())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for handle in handles {
            for id in handle.join().unwrap() {
                assert!(seen.insert(id), "runtime id {id} was handed out twice");
            }
        }
    }

    #[test]
    fn the_caller_check_panics_with_no_runtime_current_and_the_lenient_one_does_not() {
        let _serial = serial_table();
        require_timers_if_in_runtime("test op");
        let caught = catch_unwind(|| require_caller_timers("Example::op")).unwrap_err();
        assert_eq!(
            caught.downcast_ref::<String>().map(String::as_str),
            Some(
                "tokio-actors: Example::op needs a Tokio runtime with the time driver on the \
                 calling thread (Builder::enable_time or enable_all)"
            )
        );
    }

    #[test]
    fn the_caller_check_names_the_operation_on_a_runtime_without_the_driver() {
        let _serial = serial_table();
        let no = without_timers();
        let message = no.block_on(async {
            let caught = catch_unwind(|| require_caller_timers("Example::op")).unwrap_err();
            caught.downcast_ref::<String>().cloned().unwrap_or_default()
        });
        assert_eq!(
            message,
            "tokio-actors: Example::op needs the Tokio time driver on the calling runtime \
             (Builder::enable_time or enable_all)"
        );
        let lenient = no.block_on(async {
            let caught = catch_unwind(|| require_timers_if_in_runtime("Example::op")).unwrap_err();
            caught.downcast_ref::<String>().cloned().unwrap_or_default()
        });
        assert_eq!(lenient, message);
    }

    #[test]
    fn the_caller_check_passes_on_a_runtime_with_the_driver() {
        let _serial = serial_table();
        let yes = with_timers();
        yes.block_on(async {
            require_caller_timers("Example::op");
            require_timers_if_in_runtime("Example::op");
        });
    }

    /// A spawn on a runtime without the driver does not probe it again and
    /// again: the sweep that is skipped there asks once per system and
    /// runtime.
    #[cfg(feature = "tracing")]
    #[test]
    fn spawns_on_a_runtime_without_the_driver_probe_it_once() {
        let _serial = serial_table();
        use crate::actor::context::ActorContext;
        use crate::actor::{Actor, ActorExt};
        struct P;
        impl Actor for P {
            type Message = ();
            type Response = ();
            async fn handle(
                &mut self,
                _m: (),
                _c: &mut ActorContext<Self>,
            ) -> crate::ActorResult<()> {
                Ok(())
            }
        }
        let no = without_timers();
        let sys =
            crate::system::ActorSystem::create(format!("probe-once-{}", uuid::Uuid::new_v4()))
                .unwrap();
        sys.set_turn_warning_threshold(Some(Duration::from_secs(1)));
        no.block_on(async {
            for _ in 0..50 {
                let a = P.spawn().on_system(&sys).await.expect("starts");
                drop(a);
            }
        });
        let n = probes_run(no.handle());
        assert!(n <= 1, "probed {n} times for 50 spawns");
    }
}
