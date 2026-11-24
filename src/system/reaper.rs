//! The reaper: the task of a runtime that aborts an actor's task once its kill deadline has passed.

use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::{push_kill_step, step_deadline_in, ActorSystem, StopStep};
use crate::actor::handle::emit_stop_requested;
use crate::actor::incarnation::{IncarnationRef, RaiseResult, StopLane};
use crate::actor::runtime::saturating_deadline;
use crate::helper::HelperKind;
use crate::telemetry::KillStepKind;
use crate::types::StopReason;

impl ActorSystem {
    /// Returns the reaper feed of runtime `rt`, spawning its backing task on
    /// `rt` on first use. Every supervised child's link guard and every kill
    /// path feeds a deadline into it (see `ChildLinkGuard` in the
    /// `supervision` module and [`arm_kill_backstop`](Self::arm_kill_backstop));
    /// the task itself never blocks anything and aborts a task only if it has
    /// not exited by its scheduled deadline.
    ///
    /// There is one reaper per runtime, keyed by the runtime's identity: an
    /// abort takes effect only when the runtime that drives the task next
    /// polls it, so a reaper on one runtime cannot serve another, and a
    /// runtime that is alive but never driven holds only its own entries. A
    /// slot whose task has finished (its runtime ended) is replaced on use,
    /// because a runtime's identity may be reused once the runtime is gone.
    /// The finished slots of other runtimes are pruned only when a fresh slot
    /// is inserted, never on a lookup.
    pub(crate) fn reaper_handle(&self, rt: &Handle) -> ReaperFeed {
        let id = rt.id();
        let mut slots = self
            .reaper
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = slots.get(&id) {
            if !existing.task.is_finished() {
                return existing.tx.clone();
            }
        }
        slots.retain(|_, slot| !slot.task.is_finished());
        let (tx, rx) = mpsc::unbounded_channel();
        let task = self
            .helpers
            .spawn(rt, HelperKind::Reaper, None, |end| {
                end.observe(reaper_loop(rx))
            })
            .abort_handle();
        let fresh = tx.clone();
        slots.insert(id, ReaperSlot { tx, task });
        fresh
    }
}

// ---------------------------------------------------------------------------
// Reaper - the abort-on-deadline backstop fed by every kill path
// ---------------------------------------------------------------------------

/// The sending half of a reaper's feed.
///
/// The feed is unbounded, so an entry is never refused for lack of room and a
/// sweep never cuts a target's grace short. An entry lives until its abort
/// stage, when the reaper pops it and aborts its task unless the task has
/// ended (then it does nothing), so the feed holds at most the entries armed
/// within the longest `Shutdown` window plus kill grace, `stop_within`
/// window, or shutdown timeout plus kill grace in use. An entry is a
/// deadline and an abort handle, and for a two-stage entry a lane; its abort
/// handle keeps the task's cell allocated (the size of a run-loop future)
/// until the entry ends, even when the task ended long before.
pub(crate) type ReaperFeed = mpsc::UnboundedSender<ReaperEntry>;

/// The live reaper of one runtime: its feed and the `AbortHandle` used to tell
/// whether its backing task (and, with it, the runtime it was spawned on) is
/// still around.
pub(super) struct ReaperSlot {
    pub(super) tx: ReaperFeed,
    pub(super) task: AbortHandle,
}

/// One pending abort deadline, fed by a dying child's link guard or by a kill
/// path.
///
/// An entry has one stage or two. The abort stage is the entry's `deadline`.
/// A first stage, `kill_at`, raises `Kill` on a lane at its instant and
/// leaves the abort stage in place: it is the orderly stop of a supervisor
/// whose own supervisor ended, a final sweep's deadline, or the steps still
/// owed by a bounded stop whose stop task was lost, which the abort follows
/// once the target's kill grace has passed after that `Kill`. The
/// grace is the span between the two stages as built, and it is counted
/// again from the moment the `Kill` stage actually fires: an entry whose
/// stages were both already past when it was queued still gives its target
/// the whole grace after its `Kill`.
///
/// An entry that is dropped without having fired its abort aborts its task:
/// every entry belongs to a task that was already sent `Kill` or an orderly
/// stop that ends in one, so the deadline is a courtesy that a reaper unable
/// to keep it (its runtime was shut down, its task never polled, its feed
/// closed) must not turn into a child left alive with nothing watching it.
/// Dropping an entry, before its first stage, between its stages or in the
/// queue, is therefore never a way to cancel an abort.
pub(crate) struct ReaperEntry {
    /// The first stage while it is pending: the instant to raise `Kill`, the
    /// lane to raise it on, and the kill grace the abort follows it by.
    kill_at: Option<(Instant, StopLane, Duration)>,
    /// The abort instant: as built, and for a two-stage entry no earlier
    /// than its kill grace after the moment its `Kill` stage fired.
    pub(super) deadline: Instant,
    abort: AbortHandle,
    /// The target's abort record: owed from the moment the entry exists
    /// (nothing is owed to a task that already finished), written when the
    /// entry aborts the task.
    record: IncarnationRef,
    /// The step cell of the bounded stop whose ladder this entry carries on:
    /// the first stage writes it in the step that raises the `Kill`.
    step: Option<Arc<StopStep>>,
    pub(super) fired: bool,
}

impl ReaperEntry {
    pub(crate) fn new(deadline: Instant, abort: AbortHandle, record: IncarnationRef) -> Self {
        record.owe_for(&abort);
        push_kill_step(&record, None, KillStepKind::AbortArmed, || {
            step_deadline_in(deadline)
        });
        Self {
            kill_at: None,
            deadline,
            abort,
            record,
            step: None,
            fired: false,
        }
    }

    /// An entry whose first stage raises `Kill` on `lane` at `kill_at`, and
    /// whose abort stage is due at `deadline`.
    pub(crate) fn two_stage(
        kill_at: Instant,
        lane: StopLane,
        deadline: Instant,
        abort: AbortHandle,
    ) -> Self {
        let record = lane.abort_record().clone();
        record.owe_for(&abort);
        push_kill_step(&record, None, KillStepKind::AbortArmed, || {
            step_deadline_in(deadline)
        });
        Self {
            kill_at: Some((kill_at, lane, deadline.saturating_duration_since(kill_at))),
            deadline,
            abort,
            record,
            step: None,
            fired: false,
        }
    }

    /// Makes the first stage take the step of a bounded stop: the cell is
    /// written with the raise.
    pub(crate) fn with_step(mut self, step: Arc<StopStep>) -> Self {
        self.step = Some(step);
        self
    }

    /// The instant the reaper acts on this entry next: the pending first
    /// stage, then the abort.
    pub(super) fn due(&self) -> Instant {
        self.kill_at
            .as_ref()
            .map_or(self.deadline, |(kill_at, _, _)| *kill_at)
    }

    /// The next instant is due. A pending first stage whose task has not
    /// finished raises `Kill` on its lane, moves the abort stage to no
    /// earlier than its kill grace from now, and hands the entry back, still
    /// unfired, for that stage. Anything else ends the entry through the
    /// record: the abort of a task that has not exited, and the release of
    /// the owed mark of one that has.
    pub(super) fn fire(mut self) -> Option<Self> {
        if !self.abort.is_finished() {
            if let Some((_, lane, grace)) = self.kill_at.take() {
                // The entry holds the abort handle itself, so no deadline is
                // owed to an attach: a duplicate arm by one is harmless.
                // Decided by the lane's own answer to this raise, never by a
                // second read of the lane: a seal between the raise and a
                // later read would hide a `Kill` the lane accepted.
                let result = match &self.step {
                    Some(step) => step.raise_shared(&lane, || {}),
                    None => lane.try_raise(StopReason::Kill),
                };
                if result != RaiseResult::Refused {
                    push_kill_step(&self.record, None, KillStepKind::Raised, || None);
                }
                emit_stop_requested(
                    None,
                    &lane,
                    &StopReason::Kill,
                    result,
                    crate::telemetry::StopOrigin::Requested,
                    None,
                );
                self.deadline = self
                    .deadline
                    .max(saturating_deadline(Instant::now(), grace));
                return Some(self);
            }
        }
        self.fired = true;
        self.abort_task();
        None
    }

    /// Aborts the task through the record, which reports the `AbortFired`
    /// step for the abort that finds the task unfinished first: a finished
    /// task gets no abort and no step, and a task a caller's force path
    /// aborted already has had its step reported by that abort.
    fn abort_task(&self) {
        self.record.abort(&self.abort);
    }
}

impl Drop for ReaperEntry {
    fn drop(&mut self) {
        if !self.fired {
            self.abort_task();
        }
    }
}

impl PartialEq for ReaperEntry {
    fn eq(&self, other: &Self) -> bool {
        self.due() == other.due()
    }
}

impl Eq for ReaperEntry {}

impl PartialOrd for ReaperEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for ReaperEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        // Reversed so a `BinaryHeap` (a max-heap by default) pops the
        // SOONEST instant first, turning it into a min-heap by the next
        // instant the entry is due.
        other.due().cmp(&self.due())
    }
}

/// The reaper task's state: the feed and the entries waiting for their
/// deadlines. It is built before the task is spawned, so a task dropped before
/// its first poll still owns the feed and the entries queued in it, which
/// abort when they drop.
struct ReaperState {
    rx: mpsc::UnboundedReceiver<ReaperEntry>,
    heap: BinaryHeap<ReaperEntry>,
}

/// One task per [`ActorSystem`] and runtime, lazily spawned on the runtime's
/// first use: aborts a task if it has not exited by its scheduled deadline.
/// Entries arrive through `ActorSystem::feed_reaper` - a synchronous,
/// non-blocking send on an unbounded feed - and are never awaited by
/// anything: a closed feed makes the sender retry once on the reaper its
/// runtime has by then before it aborts.
pub(super) fn reaper_loop(
    rx: mpsc::UnboundedReceiver<ReaperEntry>,
) -> impl std::future::Future<Output = ()> {
    let state = ReaperState {
        rx,
        heap: BinaryHeap::new(),
    };
    reaper_run(state)
}

/// Pops the entry that is due and fires it. An entry with a stage left comes
/// back and is queued again before anything else runs, so no entry is ever
/// held across an await.
pub(super) fn fire_due(heap: &mut BinaryHeap<ReaperEntry>) {
    if let Some(due) = heap.pop() {
        if let Some(rest) = due.fire() {
            heap.push(rest);
        }
    }
}

async fn reaper_run(mut state: ReaperState) {
    let mut channel_open = true;

    loop {
        if !channel_open {
            match state.heap.peek().map(ReaperEntry::due) {
                Some(due) => {
                    tokio::time::sleep_until(due).await;
                    fire_due(&mut state.heap);
                }
                None => break,
            }
            continue;
        }

        match state.heap.peek().map(ReaperEntry::due) {
            Some(due) => {
                tokio::select! {
                    entry = state.rx.recv() => match entry {
                        Some(e) => state.heap.push(e),
                        None => channel_open = false,
                    },
                    _ = tokio::time::sleep_until(due) => {
                        fire_due(&mut state.heap);
                    }
                }
            }
            None => match state.rx.recv().await {
                Some(e) => state.heap.push(e),
                None => channel_open = false,
            },
        }
    }
}
